use chrono::Datelike;
use chrono_tz::Tz;
use matrix_sdk::{
    Client, Room,
    ruma::{
        OwnedUserId,
        events::{Mentions, room::message::RoomMessageEventContent},
    },
};
use tracing::{error, info, warn};

use crate::{
    BotContext,
    state::{ReminderKind, current_iso_week, week_dates},
};

async fn mention_message(
    text: &str,
    mention_mxids: &[String],
    room: &Room,
) -> RoomMessageEventContent {
    let parsed: Vec<OwnedUserId> =
        mention_mxids.iter().filter_map(|s| s.parse().ok()).collect();
    let all_mxids = crate::format::extract_mxids(text);
    let refs: Vec<&str> = all_mxids.iter().map(String::as_str).collect();
    let names = crate::format::fetch_names(room, &refs).await;
    let content = crate::format::mentionify_with_names(text, &names);
    if parsed.is_empty() { content } else { content.add_mentions(Mentions::with_user_ids(parsed)) }
}

pub async fn run(ctx: BotContext, client: Client) {
    info!("Scheduler started");
    loop {
        if let Err(e) = tick(&ctx, &client).await { error!("Scheduler error: {e}"); }
        tokio::time::sleep(tokio::time::Duration::from_secs(30 * 60)).await;
    }
}

async fn tick(ctx: &BotContext, client: &Client) -> anyhow::Result<()> {
    let tz: Tz        = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
    let local_now     = chrono::Utc::now().with_timezone(&tz);
    let local_weekday = local_now.weekday().num_days_from_monday() as u8;
    let (year, week)  = current_iso_week();
    let interval      = ctx.config.schedule.interval_weeks;

    let room = match client.get_room(&ctx.room_id) {
        Some(r) => r,
        None => { warn!("Scheduler: bot is not in room {}", ctx.room_id); return Ok(()); }
    };

    let mut state = ctx.state.lock().await;
    let groups = state.cleaning_groups.clone();

    for group in &groups {
        if !state.is_due(&group.id, year, week, interval) { continue; }

        let responsible = state.responsible_person(group, year, week, interval);
        let (resp_mxids, users_text): (Vec<String>, String) = match responsible {
            None    => (vec![], "(nobody assigned)".to_owned()),
            Some(p) => (
                p.matrix_id.as_ref().map(|m| vec![m.clone()]).unwrap_or_default(),
                p.display_name.clone(),
            ),
        };
        let rooms_line = group.rooms_text().map(|r| format!("\n{r}")).unwrap_or_default();

        if local_weekday == ctx.config.schedule.reminder_weekday
            && !state.reminder_sent(&group.id, year, week, &ReminderKind::Initial)
        {
            let msg = format!(
                "🧹 **{}** · week {week} ({})\n{users_text}{rooms_line}\n✅ React or type !done",
                group.name, week_dates(year, week)
            );
            let resp = room.send(mention_message(&msg, &resp_mxids, &room).await).await
                .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
            state.reminder_event_ids.insert(resp.response.event_id.to_string(), group.id.clone());
            state.mark_reminder_sent(&group.id, year, week, ReminderKind::Initial);
            info!("Sent initial reminder for «{}» week {week}/{year}", group.name);
        }

        if local_weekday == ctx.config.schedule.final_reminder_weekday
            && !state.reminder_sent(&group.id, year, week, &ReminderKind::Final)
        {
            let msg = format!(
                "⚠️ **{}** still not cleaned · week {week} ({})\n{users_text}{rooms_line}\n✅ React or type !done · swap: !swap @user",
                group.name, week_dates(year, week)
            );
            let resp = room.send(mention_message(&msg, &resp_mxids, &room).await).await
                .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
            state.reminder_event_ids.insert(resp.response.event_id.to_string(), group.id.clone());
            state.mark_reminder_sent(&group.id, year, week, ReminderKind::Final);
            info!("Sent final reminder for «{}» week {week}/{year}", group.name);
        }
    }

    if let Some(summary_day) = ctx.config.schedule.summary_weekday {
        if local_weekday == summary_day
            && !state.reminder_sent("", year, week, &ReminderKind::WeeklySummary)
        {
            let (msg, missed_mxids) = build_summary(&state, year, week, interval);
            room.send(mention_message(&msg, &missed_mxids, &room).await).await
                .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
            state.mark_reminder_sent("", year, week, ReminderKind::WeeklySummary);
            info!("Sent weekly summary for week {week}/{year}");
        }
    }

    state.save(&ctx.state_path).await?;
    Ok(())
}

fn build_summary(
    state: &crate::state::State,
    year: i32, week: u32, interval: u32,
) -> (String, Vec<String>) {
    let groups = state.cleaning_groups.clone();
    let mut lines = vec![format!("📋 **Cleaning summary** · week {week} ({})", week_dates(year, week))];
    lines.push(String::new());
    let mut done_count = 0usize;
    let mut due_count  = 0usize;
    let mut mention_mxids: Vec<String> = Vec::new();

    for group in &groups {
        if !state.is_due(&group.id, year, week, interval) { continue; }
        due_count += 1;

        if state.is_completed(&group.id, year, week) {
            done_count += 1;
            let by = state.completions.iter()
                .find(|c| c.group_id == group.id && c.iso_year == year && c.iso_week == week)
                .and_then(|c| state.person_by_id(&c.completed_by_id))
                .map(|p| format!(" · {}", p.display_name))
                .unwrap_or_default();
            lines.push(format!("✅ **{}**{by}", group.name));
        } else {
            let resp = state.responsible_person(group, year, week, interval);
            let resp_text = resp.map(|p| p.display_name.as_str()).unwrap_or("(nobody assigned)");
            let rooms_str = group.rooms_text().map(|r| format!("\n  {r}")).unwrap_or_default();
            lines.push(format!("❌ **{}** · {resp_text}{rooms_str}", group.name));
            if let Some(mxid) = resp.and_then(|p| p.matrix_id.as_ref()) {
                mention_mxids.push(mxid.clone());
            }
        }
        let streak = state.streak_for(&group.id, interval);
        if streak >= 3 && state.is_completed(&group.id, year, week) {
            lines.push(format!("   🔥 {streak}-week streak!"));
        }
    }

    lines.push(String::new());
    if due_count > 0 {
        lines.push(format!("{done_count}/{due_count} cleaned this week"));
        let overdue: Vec<_> = groups.iter()
            .filter(|g| state.missed_weeks_for(&g.id, interval).len() >= 2)
            .collect();
        if !overdue.is_empty() {
            lines.push(String::new());
            lines.push("⚠️ Repeatedly missed (2+ weeks in a row):".into());
            for g in overdue {
                lines.push(format!("   {} : {} weeks missed", g.name, state.missed_weeks_for(&g.id, interval).len()));
            }
        }
    } else {
        lines.push("No groups were scheduled this week.".into());
    }

    mention_mxids.sort();
    mention_mxids.dedup();
    (lines.join("\n"), mention_mxids)
}
