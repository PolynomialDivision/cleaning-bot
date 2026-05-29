use chrono::Datelike;
use chrono_tz::Tz;
use matrix_sdk::{
    Client, Room,
    ruma::{
        OwnedUserId,
        events::{
            Mentions,
            room::message::RoomMessageEventContent,
        },
    },
};
use tracing::{error, info, warn};

use crate::{
    BotContext,
    state::{ReminderKind, current_iso_week, week_dates},
};

/// Build a rich message: display-name pills for every MXID found in `text`,
/// plus proper `m.mentions` metadata so push notifications fire for
/// `mention_user_ids` (the people who should actually be pinged).
async fn mention_message(
    text: &str,
    mention_user_ids: &[String],
    room: &Room,
) -> RoomMessageEventContent {
    let parsed: Vec<OwnedUserId> =
        mention_user_ids.iter().filter_map(|s| s.parse().ok()).collect();

    // Scan the full message text for MXIDs — this picks up both the
    // responsible users *and* "done by @user" completions in one shot.
    let all_mxids = crate::format::extract_mxids(text);
    let refs: Vec<&str> = all_mxids.iter().map(String::as_str).collect();
    let names = crate::format::fetch_names(room, &refs).await;

    let content = crate::format::mentionify_with_names(text, &names);
    if parsed.is_empty() {
        content
    } else {
        content.add_mentions(Mentions::with_user_ids(parsed))
    }
}

/// Background task: wake up every 30 minutes, check whether any reminders or
/// the weekly summary need to go out.
pub async fn run(ctx: BotContext, client: Client) {
    info!("Scheduler started");
    loop {
        if let Err(e) = tick(&ctx, &client).await {
            error!("Scheduler error: {e}");
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(30 * 60)).await;
    }
}

async fn tick(ctx: &BotContext, client: &Client) -> anyhow::Result<()> {
    let tz: Tz = ctx.config.schedule.timezone
        .parse()
        .unwrap_or(chrono_tz::UTC);

    let local_now = chrono::Utc::now().with_timezone(&tz);
    let local_weekday = local_now.weekday().num_days_from_monday() as u8; // 0=Mon…6=Sun
    let (year, week) = current_iso_week();
    let interval = ctx.config.schedule.interval_weeks;

    let room = match client.get_room(&ctx.room_id) {
        Some(r) => r,
        None => {
            warn!("Scheduler: bot is not in room {}", ctx.room_id);
            return Ok(());
        }
    };

    let mut state = ctx.state.lock().await;

    // ── Per-unit reminders ────────────────────────────────────────────────────
    let units = state.units();
    for unit in &units {
        let due = state.is_due(&unit.name, year, week, interval);
        if !due {
            continue;
        }

        // Only the one user whose turn it is gets pinged.
        let responsible: Vec<String> = state
            .responsible_user(unit, year, week, interval)
            .map(|u| vec![u])
            .unwrap_or_default();
        let users_text = if responsible.is_empty() {
            "(nobody assigned)".to_owned()
        } else {
            responsible.join(", ")
        };

        let rooms_line = unit.rooms_text()
            .map(|r| format!("\n{r}"))
            .unwrap_or_default();

        // Initial reminder
        if local_weekday == ctx.config.schedule.reminder_weekday
            && !state.reminder_sent(&unit.name, year, week, &ReminderKind::Initial)
        {
            let msg = format!(
                "🧹 **{}** · week {week} ({})\n\
                 {users_text}{rooms_line}\n\
                 ✅ React or type !done",
                unit.name, week_dates(year, week)
            );
            let resp = room.send(mention_message(&msg, &responsible, &room).await).await
                .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
            state.reminder_event_ids.insert(resp.response.event_id.to_string(), unit.name.clone());
            state.mark_reminder_sent(&unit.name, year, week, ReminderKind::Initial);
            info!("Sent initial reminder for «{}» week {week}/{year}", unit.name);
        }

        // Final reminder
        if local_weekday == ctx.config.schedule.final_reminder_weekday
            && !state.reminder_sent(&unit.name, year, week, &ReminderKind::Final)
        {
            let msg = format!(
                "⚠️ **{}** still not cleaned · week {week} ({})\n\
                 {users_text}{rooms_line}\n\
                 ✅ React or type !done · swap: !swap @user",
                unit.name, week_dates(year, week)
            );
            let resp = room.send(mention_message(&msg, &responsible, &room).await).await
                .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
            state.reminder_event_ids.insert(resp.response.event_id.to_string(), unit.name.clone());
            state.mark_reminder_sent(&unit.name, year, week, ReminderKind::Final);
            info!("Sent final reminder for «{}» week {week}/{year}", unit.name);
        }
    }

    // ── Weekly summary ────────────────────────────────────────────────────────
    if let Some(summary_day) = ctx.config.schedule.summary_weekday {
        if local_weekday == summary_day
            && !state.reminder_sent("", year, week, &ReminderKind::WeeklySummary)
        {
            let (msg, missed_users) = build_summary(&state, year, week, interval);
            room.send(mention_message(&msg, &missed_users, &room).await).await
                .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
            state.mark_reminder_sent("", year, week, ReminderKind::WeeklySummary);
            info!("Sent weekly summary for week {week}/{year}");
        }
    }

    state.save(&ctx.state_path).await?;
    Ok(())
}

/// Returns (message_text, users_to_mention).
/// Only users responsible for still-uncleaned units are mentioned in the
/// summary so people who already did their job don't get pinged.
fn build_summary(
    state: &crate::state::State,
    year: i32,
    week: u32,
    interval: u32,
) -> (String, Vec<String>) {
    let units = state.units();
    let mut lines = vec![format!("📋 **Cleaning summary** · week {week} ({})", week_dates(year, week))];
    lines.push(String::new());

    let mut done_count = 0usize;
    let mut due_count = 0usize;
    let mut mention_users: Vec<String> = Vec::new();

    for unit in &units {
        let due = state.is_due(&unit.name, year, week, interval);
        if !due {
            continue;
        }
        due_count += 1;

        let completed = state.is_completed(&unit.name, year, week);
        if completed {
            done_count += 1;
            let by = state.completions.iter()
                .find(|c| c.unit_name == unit.name && c.iso_year == year && c.iso_week == week)
                .map(|c| format!(" · {}", c.completed_by))
                .unwrap_or_default();
            lines.push(format!("✅ **{}**{by}", unit.name));
        } else {
            let responsible_opt = state.responsible_user(unit, year, week, interval);
            let responsible = responsible_opt.as_deref().unwrap_or("(nobody assigned)");
            let rooms_str = unit.rooms_text()
                .map(|r| format!("\n  {r}"))
                .unwrap_or_default();
            lines.push(format!("❌ **{}** · {responsible}{rooms_str}", unit.name));
            // Only ping the responsible user of uncleaned units
            if let Some(u) = responsible_opt {
                mention_users.push(u);
            }
        }

        let streak = state.streak_for(&unit.name, interval);
        if streak >= 3 && completed {
            lines.push(format!("   🔥 {streak}-week streak!"));
        }
    }

    lines.push(String::new());
    if due_count > 0 {
        lines.push(format!("{done_count}/{due_count} cleaned this week"));

        let overdue: Vec<_> = units.iter()
            .filter(|u| state.missed_weeks_for(&u.name, interval).len() >= 2)
            .collect();
        if !overdue.is_empty() {
            lines.push(String::new());
            lines.push("⚠️ Repeatedly missed (2+ weeks in a row):".into());
            for u in overdue {
                let missed = state.missed_weeks_for(&u.name, interval);
                lines.push(format!("   {} : {} weeks missed", u.name, missed.len()));
            }
        }
    } else {
        lines.push("No units were scheduled this week.".into());
    }

    // Deduplicate mention list
    mention_users.sort();
    mention_users.dedup();

    (lines.join("\n"), mention_users)
}
