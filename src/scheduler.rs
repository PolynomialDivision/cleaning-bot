use chrono::Datelike;
use chrono_tz::Tz;
use matrix_sdk::{
    Client, Room,
    ruma::{
        OwnedEventId, OwnedUserId,
        events::{
            Mentions,
            reaction::ReactionEventContent,
            relation::Annotation,
            room::message::RoomMessageEventContent,
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
        && !state.reminder_sent("*", year, week, &ReminderKind::Initial)
        && !any_per_group_sent
        && !canonical_exists
        && !due_groups.is_empty()
    {
        let (msg, mxids) = build_weekly_plan(&state, year, week, interval, &due_groups);
        let resp = room.send(mention_message(&msg, &mxids, &room).await).await
            .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
        let plan_eid = resp.response.event_id.clone();

        // Collect old plan event_ids (all weeks except this one) for unpinning.
        let old_plan_eids: Vec<String> = state.weekly_plan_canonical.iter()
            .filter(|(k, _)| **k != week_key)
            .map(|(_, v)| v.clone())
            .collect();

        state.weekly_plan_event_ids.insert(plan_eid.to_string(), (year, week));
        state.weekly_plan_canonical.insert(week_key, plan_eid.to_string());
        state.mark_reminder_sent("*", year, week, ReminderKind::Initial);
        state.save(&ctx.state_path).await?;
        info!("Sent consolidated weekly plan for week {week}/{year}");
        drop(state);

        // Self-react ✅ (UI affordance) then update room pin.
        room.send(ReactionEventContent::new(Annotation::new(plan_eid.clone(), "✅".to_string()))).await.ok();
        pin_weekly_plan(&room, &plan_eid, &old_plan_eids).await;
        return Ok(());
    }

    // ── Consolidated final reminder ───────────────────────────────────────────
    if local_weekday == ctx.config.schedule.final_reminder_weekday
        && !state.reminder_sent("*", year, week, &ReminderKind::Final)
    {
        let pending: Vec<CleaningGroup> = due_groups.iter()
            .filter(|g| !state.is_completed(&g.id, year, week))
            .cloned()
            .collect();

        if !pending.is_empty() {
            let (msg, mxids) = build_final_reminder(&state, year, week, interval, &pending);
            let resp = room.send(mention_message(&msg, &mxids, &room).await).await
                .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
            let final_eid = resp.response.event_id.clone();
            state.weekly_plan_event_ids.insert(final_eid.to_string(), (year, week));
            state.mark_reminder_sent("*", year, week, ReminderKind::Final);
            state.save(&ctx.state_path).await?;
            info!("Sent consolidated final reminder for week {week}/{year}");
            drop(state);
            room.send(ReactionEventContent::new(Annotation::new(final_eid, "✅".to_string()))).await.ok();
            return Ok(());
        } else {
            state.mark_reminder_sent("*", year, week, ReminderKind::Final);
        }
    }

    // ── Weekly summary ────────────────────────────────────────────────────────
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
    let (msg, mxids, old_canonical_eid) = {
        let state = ctx.state.lock().await;
        let due_groups: Vec<_> = state.cleaning_groups.iter()
            .filter(|g| g.is_active && state.is_due(&g.id, year, week, interval))
            .cloned()
            .collect();
        if due_groups.is_empty() { return Ok(None); }
        let old_eid = state.weekly_plan_canonical.get(&week_key).cloned();
        let (msg, mxids) = build_weekly_plan(&state, year, week, interval, &due_groups);
        (msg, mxids, old_eid)
    };

    // ── Phase 2: send message (no lock held) ─────────────────────────────────
    let resp = room.send(mention_message(&msg, &mxids, room).await).await
        .map_err(|e| anyhow::anyhow!("announce send failed: {e}"))?;
    let new_eid = resp.response.event_id.clone();

    // ── Phase 3: update state (brief lock) ───────────────────────────────────
    let eids_to_unpin = {
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

        // Collect event_ids to unpin: current week's old plan + all other weeks' plans.
        let mut unpin: Vec<String> = state.weekly_plan_canonical.iter()
            .filter(|(k, _)| **k != week_key)
            .map(|(_, v)| v.clone())
            .collect();
        if let Some(old) = old_canonical_eid { unpin.push(old); }
        unpin
    };

    // ── Phase 4: react + pin (no lock held) ──────────────────────────────────
    room.send(ReactionEventContent::new(Annotation::new(new_eid.clone(), "✅".to_string()))).await.ok();
    pin_weekly_plan(room, &new_eid, &eids_to_unpin).await;

    Ok(Some(new_eid))
}

// ── Room pin management ───────────────────────────────────────────────────────

/// Pin `new_eid` and unpin every event_id in `old_eids` (previous weeks' plans).
/// Uses the SDK's `pin_event`/`unpin_event` helpers, which preserve any other
/// pinned messages already in the room.
pub(crate) async fn pin_weekly_plan(room: &Room, new_eid: &OwnedEventId, old_eids: &[String]) {
    for old_str in old_eids {
        if let Ok(old_eid) = old_str.parse::<OwnedEventId>() {
            if let Err(e) = room.unpin_event(&old_eid).await {
                warn!("Failed to unpin old weekly plan {old_str}: {e}");
            }
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
        format!("📋 **Cleaning Plan · Week {week} ({})**", week_dates(year, week)),
        String::new(),
    ];
    let mut all_mxids: Vec<String> = Vec::new();

    for group in due_groups {
        lines.push(format!("🧹 **{}**", group.name));
        if group.is_multi_slot() {
            for (slot_idx, slot) in group.slots.iter().enumerate() {
                let done = state.is_slot_completed(&group.id, &slot.id, year, week);
                let rooms = if slot.room_names.is_empty() { String::new() }
                    else { format!("\nRooms: {}", slot.room_names.join(", ")) };
                match state.slot_assignee(group, slot_idx, year, week, interval) {
                    None => lines.push(format!("(nobody assigned){rooms}")),
                    Some(p) => {
                        if let Some(m) = &p.matrix_id { all_mxids.push(m.clone()); }
                        if done {
                            lines.push(format!("✅ {}{rooms}", p.display_name));
                        } else {
                            lines.push(format!("{}{rooms}", p.display_name));
                        }
                    }
                }
            }
        } else {
            let done = state.is_completed(&group.id, year, week);
            let rooms = group.rooms_text().map(|r| format!("\n{r}")).unwrap_or_default();
            match state.responsible_person(group, year, week, interval) {
                None => lines.push(format!("(nobody assigned){rooms}")),
                Some(p) => {
                    if let Some(m) = &p.matrix_id { all_mxids.push(m.clone()); }
                    if done {
                        lines.push(format!("✅ {}{rooms}", p.display_name));
                    } else {
                        lines.push(format!("{}{rooms}", p.display_name));
                    }
                }
            }
        }
        lines.push(String::new());
    }

    lines.push("✅ React or use !done".to_string());

    all_mxids.sort();
    all_mxids.dedup();
    (lines.join("\n"), all_mxids)
}

fn build_final_reminder(
    state: &crate::state::State,
    year: i32, week: u32, interval: u32,
    pending: &[CleaningGroup],
) -> (String, Vec<String>) {
    let mut lines = vec![
        format!("⚠️ **Still pending · Week {week} ({})**", week_dates(year, week)),
        String::new(),
    ];
    let mut all_mxids: Vec<String> = Vec::new();

    for group in pending {
        if group.is_multi_slot() {
            lines.push(format!("❌ **{}**", group.name));
            for (slot_idx, slot) in group.slots.iter().enumerate() {
                if state.is_slot_completed(&group.id, &slot.id, year, week) { continue; }
                let rooms = if slot.room_names.is_empty() { String::new() }
                    else { format!("\n  Rooms: {}", slot.room_names.join(", ")) };
                match state.slot_assignee(group, slot_idx, year, week, interval) {
                    None => lines.push(format!("  (nobody assigned){rooms}")),
                    Some(p) => {
                        if let Some(m) = &p.matrix_id { all_mxids.push(m.clone()); }
                        lines.push(format!("  {}{rooms}", p.display_name));
                    }
                }
            }
        } else {
            let rooms = group.rooms_text().map(|r| format!("\n  {r}")).unwrap_or_default();
            match state.responsible_person(group, year, week, interval) {
                None => lines.push(format!("❌ **{}** · (nobody assigned){rooms}", group.name)),
                Some(p) => {
                    if let Some(m) = &p.matrix_id { all_mxids.push(m.clone()); }
                    lines.push(format!("❌ **{}** · {}{rooms}", group.name, p.display_name));
                }
            }
        }
        lines.push(String::new());
    }

    lines.push("✅ React or use !done · swap: !swap @user".to_string());

    all_mxids.sort();
    all_mxids.dedup();
    (lines.join("\n"), all_mxids)
}

fn build_summary(
    state: &crate::state::State,
    year: i32, week: u32, interval: u32,
) -> (String, Vec<String>) {
    let groups: Vec<_> = state.cleaning_groups.iter()
        .filter(|g| g.is_active)
        .cloned()
        .collect();
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
