use chrono::{Datelike, Timelike};
use chrono_tz::Tz;
use matrix_sdk::{
    Client, Room,
    ruma::{
        OwnedEventId, OwnedUserId,
        events::{
            Mentions,
            reaction::ReactionEventContent,
            relation::{Annotation, Reply},
            room::message::{Relation, ReplacementMetadata, RoomMessageEventContent},
        },
    },
};
use tracing::{error, info, warn};

use crate::{
    BotContext,
    domain::CleaningGroup,
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

/// Edit the pinned weekly plan message to reflect the current completion state.
/// No-op if no plan has been sent for this week yet.
pub(crate) async fn refresh_pinned_plan(ctx: &BotContext, room: &Room, year: i32, week: u32) {
    let week_key = format!("{year}-W{week:02}");
    let interval = ctx.config.schedule.interval_weeks;

    let (canonical_eid, msg, mxids) = {
        let state = ctx.state.lock().await;
        let eid_str = match state.weekly_plan_canonical.get(&week_key) {
            Some(e) => e.clone(),
            None    => return,
        };
        let eid = match eid_str.parse::<OwnedEventId>() {
            Ok(e)  => e,
            Err(_) => return,
        };
        let due_groups: Vec<_> = state.cleaning_groups.iter()
            .filter(|g| g.is_active && state.is_due(&g.id, year, week, interval))
            .cloned()
            .collect();
        if due_groups.is_empty() { return; }
        let (msg, mxids) = build_weekly_plan(&state, year, week, interval, &due_groups);
        (eid, msg, mxids)
    };

    let content = mention_message(&msg, &mxids, room).await
        .make_replacement(ReplacementMetadata::new(canonical_eid, None));
    if let Err(e) = room.send(content).await {
        warn!("Failed to refresh pinned plan: {e}");
    }
}

async fn tick(ctx: &BotContext, client: &Client) -> anyhow::Result<()> {
    let tz: Tz        = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
    let local_now     = chrono::Utc::now().with_timezone(&tz);
    let local_weekday = local_now.weekday().num_days_from_monday() as u8;
    let (remind_h, remind_m) = parse_hhmm(&ctx.config.schedule.reminder_time);
    let after_hour = (local_now.hour() as u8, local_now.minute() as u8) >= (remind_h, remind_m);
    let (year, week)  = current_iso_week();
    let interval      = ctx.config.schedule.interval_weeks;

    let room = match client.get_room(&ctx.room_id) {
        Some(r) => r,
        None => { warn!("Scheduler: bot is not in room {}", ctx.room_id); return Ok(()); }
    };

    let mut state = ctx.state.lock().await;

    let due_groups: Vec<CleaningGroup> = state.cleaning_groups.iter()
        .filter(|g| g.is_active && state.is_due(&g.id, year, week, interval))
        .cloned()
        .collect();

    let week_key = format!("{year}-W{week:02}");

    // Guard: skip if a canonical plan already exists for this week (belt-and-suspenders idempotency)
    // or if per-group reminders were already sent (migration from the old scheduler).
    let any_per_group_sent = state.cleaning_groups.iter()
        .any(|g| state.reminder_sent(&g.id, year, week, &ReminderKind::Initial));
    let canonical_exists = state.weekly_plan_canonical.contains_key(&week_key);

    // ── Consolidated weekly plan (initial reminder) ───────────────────────────
    if local_weekday == ctx.config.schedule.reminder_weekday
        && after_hour
        && !state.reminder_sent("*", year, week, &ReminderKind::Initial)
        && !any_per_group_sent
        && !canonical_exists
        && !due_groups.is_empty()
    {
        let (msg, mxids) = build_weekly_plan(&state, year, week, interval, &due_groups);
        let resp = room.send(mention_message(&msg, &mxids, &room).await).await
            .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
        let plan_eid = resp.response.event_id.clone();

        state.weekly_plan_event_ids.insert(plan_eid.to_string(), (year, week));
        state.weekly_plan_canonical.insert(week_key, plan_eid.to_string());
        state.mark_reminder_sent("*", year, week, ReminderKind::Initial);
        state.save(&ctx.state_path).await?;
        info!("Sent consolidated weekly plan for week {week}/{year}");
        drop(state);

        // Self-react ✅ (UI affordance) then update room pin.
        room.send(ReactionEventContent::new(Annotation::new(plan_eid.clone(), "✅".to_string()))).await.ok();
        pin_weekly_plan(&room, &plan_eid).await;
        return Ok(());
    }

    // ── Consolidated final reminder ───────────────────────────────────────────
    if local_weekday == ctx.config.schedule.final_reminder_weekday
        && after_hour
        && !state.reminder_sent("*", year, week, &ReminderKind::Final)
    {
        let pending: Vec<CleaningGroup> = due_groups.iter()
            .filter(|g| !state.is_completed(&g.id, year, week))
            .cloned()
            .collect();

        if !pending.is_empty() {
            let (msg, mxids, reply_to_plan) = build_final_reminder(&state, year, week, interval, &pending);
            let mut content = mention_message(&msg, &mxids, &room).await;
            if let Some(plan_eid) = reply_to_plan {
                if let Ok(plan_eid) = plan_eid.parse::<OwnedEventId>() {
                    content.relates_to = Some(Relation::Reply(Reply::with_event_id(plan_eid)));
                }
            }
            room.send(content).await
                .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
            state.mark_reminder_sent("*", year, week, ReminderKind::Final);
            state.save(&ctx.state_path).await?;
            info!("Sent consolidated final reminder for week {week}/{year}");
            return Ok(());
        } else {
            state.mark_reminder_sent("*", year, week, ReminderKind::Final);
        }
    }

    state.save(&ctx.state_path).await?;
    Ok(())
}

// ── Announce helper (used by scheduler tick and !announceweek command) ────────

/// Send a consolidated weekly plan for `(year, week)`, replacing any previous
/// plan for that week in state, pinning the new message, and adding a ✅ reaction.
///
/// Behaviour:
/// - Removes ALL existing `weekly_plan_event_ids` entries for `(year, week)` so
///   reactions on stale messages no longer trigger completions.
/// - Updates `weekly_plan_canonical` to the new event_id (single active plan).
/// - Marks the week's `Initial` reminder as sent so the scheduler doesn't re-fire.
/// - Returns `None` when no active groups are due this week.
pub(crate) async fn announce_weekly_plan(
    ctx: &BotContext,
    room: &Room,
    year: i32,
    week: u32,
) -> anyhow::Result<Option<OwnedEventId>> {
    let interval  = ctx.config.schedule.interval_weeks;
    let week_key  = format!("{year}-W{week:02}");

    // ── Phase 1: build message (brief lock) ───────────────────────────────────
    let (msg, mxids) = {
        let state = ctx.state.lock().await;
        let due_groups: Vec<_> = state.cleaning_groups.iter()
            .filter(|g| g.is_active && state.is_due(&g.id, year, week, interval))
            .cloned()
            .collect();
        if due_groups.is_empty() { return Ok(None); }
        build_weekly_plan(&state, year, week, interval, &due_groups)
    };

    // ── Phase 2: send message (no lock held) ─────────────────────────────────
    let resp = room.send(mention_message(&msg, &mxids, room).await).await
        .map_err(|e| anyhow::anyhow!("announce send failed: {e}"))?;
    let new_eid = resp.response.event_id.clone();

    // ── Phase 3: update state (brief lock) ───────────────────────────────────
    {
        let mut state = ctx.state.lock().await;

        // Remove ALL stale entries for this week so old reactions stop working.
        state.weekly_plan_event_ids.retain(|_, &mut (y, w)| (y, w) != (year, week));
        // Register only the new plan as the active entry for this week.
        state.weekly_plan_event_ids.insert(new_eid.to_string(), (year, week));
        state.weekly_plan_canonical.insert(week_key.clone(), new_eid.to_string());
        // Ensure the scheduler won't auto-send another plan for this week.
        if !state.reminder_sent("*", year, week, &ReminderKind::Initial) {
            state.mark_reminder_sent("*", year, week, ReminderKind::Initial);
        }
        state.save(&ctx.state_path).await?;
    };

    // ── Phase 4: react + pin (no lock held) ──────────────────────────────────
    room.send(ReactionEventContent::new(Annotation::new(new_eid.clone(), "✅".to_string()))).await.ok();
    pin_weekly_plan(room, &new_eid).await;

    Ok(Some(new_eid))
}

// ── Room pin management ───────────────────────────────────────────────────────

/// Pin `new_eid` and unpin every currently pinned event in the room.
/// Reads live room state so it catches messages pinned before state tracking began.
pub(crate) async fn pin_weekly_plan(room: &Room, new_eid: &OwnedEventId) {
    for old_eid in room.pinned_event_ids().unwrap_or_default() {
        if old_eid == *new_eid { continue; }
        if let Err(e) = room.unpin_event(&old_eid).await {
            warn!("Failed to unpin {old_eid}: {e}");
        }
    }
    if let Err(e) = room.pin_event(new_eid).await {
        warn!("Failed to pin weekly plan: {e}");
    }
}

// ── Message builders ──────────────────────────────────────────────────────────

pub(crate) fn build_weekly_plan(
    state: &crate::state::State,
    year: i32, week: u32, interval: u32,
    due_groups: &[CleaningGroup],
) -> (String, Vec<String>) {
    let mut lines = vec![
        format!("🧹 **Week {week} · {}**", week_dates(year, week)),
        "React ✅ when your part is done.".to_owned(),
        String::new(),
    ];
    let mut all_mxids: Vec<String> = Vec::new();

    for group in due_groups {
        lines.push(format!("**{}**", group.name));
        if group.is_multi_slot() {
            for (slot_idx, slot) in group.slots.iter().enumerate() {
                let done = state.is_slot_completed(&group.id, &slot.id, year, week);
                let rooms = if slot.room_names.is_empty() { String::new() }
                    else { format!(" · {}", slot.room_names.join(", ")) };
                match state.slot_assignee(group, slot_idx, year, week, interval) {
                    None => lines.push(format!("{} {}{rooms}", status_icon(done), slot.name)),
                    Some(p) => {
                        if let Some(m) = &p.matrix_id { all_mxids.push(m.clone()); }
                        if done {
                            lines.push(format!("✅ {} · cleaned{rooms}", p.display_name));
                        } else {
                            lines.push(format!("⬜ {} · {}{rooms}", slot.name, person_label(p)));
                        }
                    }
                }
            }
        } else {
            let done = state.is_completed(&group.id, year, week);
            let rooms = group.rooms_text().map(|r| format!(" · {}", r.replace('\n', " · "))).unwrap_or_default();
            match state.responsible_person(group, year, week, interval) {
                None => lines.push(format!("{} nobody assigned{rooms}", status_icon(done))),
                Some(p) => {
                    if let Some(m) = &p.matrix_id { all_mxids.push(m.clone()); }
                    if done {
                        lines.push(format!("✅ {} · cleaned{rooms}", p.display_name));
                    } else {
                        lines.push(format!("⬜ {}{rooms}", person_label(p)));
                    }
                }
            }
        }
        lines.push(String::new());
    }

    all_mxids.sort();
    all_mxids.dedup();
    (lines.join("\n"), all_mxids)
}

fn build_final_reminder(
    state: &crate::state::State,
    year: i32, week: u32, interval: u32,
    pending: &[CleaningGroup],
) -> (String, Vec<String>, Option<String>) {
    let mut lines = vec![
        format!("⏰ **Still open · Week {week}**"),
        String::new(),
    ];
    let mut all_mxids: Vec<String> = Vec::new();

    for group in pending {
        if group.is_multi_slot() {
            lines.push(format!("**{}**", group.name));
            for (slot_idx, slot) in group.slots.iter().enumerate() {
                if state.is_slot_completed(&group.id, &slot.id, year, week) { continue; }
                let rooms = if slot.room_names.is_empty() { String::new() }
                    else { format!(" · {}", slot.room_names.join(", ")) };
                match state.slot_assignee(group, slot_idx, year, week, interval) {
                    None => lines.push(format!("⬜ {}{rooms}", slot.name)),
                    Some(p) => {
                        if let Some(m) = &p.matrix_id { all_mxids.push(m.clone()); }
                        lines.push(format!("⬜ {} · {}{rooms}", slot.name, person_label(p)));
                    }
                }
            }
        } else {
            let rooms = group.rooms_text().map(|r| format!(" · {}", r.replace('\n', " · "))).unwrap_or_default();
            match state.responsible_person(group, year, week, interval) {
                None => lines.push(format!("**{}** · nobody assigned{rooms}", group.name)),
                Some(p) => {
                    if let Some(m) = &p.matrix_id { all_mxids.push(m.clone()); }
                    lines.push(format!("**{}** · {}{rooms}", group.name, person_label(p)));
                }
            }
        }
        lines.push(String::new());
    }

    all_mxids.sort();
    all_mxids.dedup();
    let week_key = format!("{year}-W{week:02}");
    let reply_to_plan = state.weekly_plan_canonical.get(&week_key).cloned();
    (lines.join("\n"), all_mxids, reply_to_plan)
}

/// Parse "HH:MM" → (hour, minute). Falls back to (9, 0) on bad input.
fn parse_hhmm(s: &str) -> (u8, u8) {
    let mut parts = s.splitn(2, ':');
    let h = parts.next().and_then(|p| p.parse().ok()).unwrap_or(9u8);
    let m = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0u8);
    (h.min(23), m.min(59))
}

/// Returns the MXID if the person has one (so `mentionify_with_names` renders
/// a pill and the Mentions field triggers a push notification), otherwise falls
/// back to the plain display name (for non-Matrix users).
fn person_label(p: &crate::domain::Person) -> &str {
    p.matrix_id.as_deref().unwrap_or(&p.display_name)
}

fn status_icon(done: bool) -> &'static str {
    if done { "✅" } else { "⬜" }
}
