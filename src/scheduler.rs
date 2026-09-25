use chrono::{Datelike, Timelike};
use chrono_tz::Tz;
use matrix_sdk::{
    ruma::{
        events::{
            reaction::ReactionEventContent,
            relation::{Annotation, Reply},
            room::message::{Relation, ReplacementMetadata, RoomMessageEventContent},
            Mentions,
        },
        OwnedEventId, OwnedUserId,
    },
    Client, Room,
};
use mxbot_common::matrix_sdk;
use tracing::{error, info, warn};

use crate::{
    domain::CleaningGroup,
    rhythm::Turn,
    state::{current_iso_week, week_dates, ReminderKind},
    BotContext,
};

async fn mention_message(
    text: &str,
    mention_mxids: &[String],
    room: &Room,
) -> RoomMessageEventContent {
    let parsed: Vec<OwnedUserId> = mention_mxids
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
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

pub async fn run(ctx: BotContext, client: Client) {
    info!("Scheduler started");
    loop {
        if let Err(e) = tick(&ctx, &client).await {
            error!("Scheduler error: {e}");
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(30 * 60)).await;
    }
}

/// Edit the pinned weekly plan message to reflect the current completion state.
/// No-op if no plan has been sent for this week yet, or if the rendered
/// content already matches what was last sent (avoids a pointless Matrix edit).
pub(crate) async fn refresh_pinned_plan(ctx: &BotContext, room: &Room, year: i32, week: u32) {
    let week_key = format!("{year}-W{week:02}");

    let (canonical_eid, msg, mxids) = {
        let state = ctx.state.lock().await;
        let eid_str = match state.weekly_plan_canonical.get(&week_key) {
            Some(e) => e.clone(),
            None => return,
        };
        let eid = match eid_str.parse::<OwnedEventId>() {
            Ok(e) => e,
            Err(_) => return,
        };
        let due_groups: Vec<_> = state
            .cleaning_groups
            .iter()
            .filter(|g| state.belongs_in_weekly_plan(g, year, week))
            .cloned()
            .collect();
        if due_groups.is_empty() {
            return;
        }
        let (msg, mxids) = build_weekly_plan(&state, year, week, &due_groups);
        if state.weekly_plan_rendered.get(&week_key) == Some(&msg) {
            return;
        }
        (eid, msg, mxids)
    };

    let content = mention_message(&msg, &mxids, room)
        .await
        .make_replacement(ReplacementMetadata::new(canonical_eid.clone(), None));
    match room.send(content).await {
        Ok(_) => {
            if let Err(e) =
                register_weekly_plan_message(ctx, year, week, &msg, &canonical_eid).await
            {
                warn!("Failed to persist refreshed plan record for {week_key}: {e}");
            }
        }
        Err(e) => warn!("Failed to refresh pinned plan: {e}"),
    }
}

/// Record `msg` as the canonical, rendered plan message for `(year, week)`:
/// registers `event_id` in both id maps (so it's the single active plan/edit
/// target and reactions on it are recognized), snapshots the text it now
/// shows, marks the week's initial reminder as sent, and persists. Callers
/// are responsible for actually sending/editing the message beforehand —
/// this only updates bookkeeping. Shared by the first-ever post of a week's
/// plan, `refresh_pinned_plan`, `!plan announce`, and startup reconciliation.
async fn register_weekly_plan_message(
    ctx: &BotContext,
    year: i32,
    week: u32,
    msg: &str,
    event_id: &OwnedEventId,
) -> anyhow::Result<()> {
    let week_key = format!("{year}-W{week:02}");
    let mut state = ctx.state.lock().await;
    state
        .weekly_plan_event_ids
        .retain(|_, &mut (y, w)| (y, w) != (year, week));
    state
        .weekly_plan_event_ids
        .insert(event_id.to_string(), (year, week));
    state
        .weekly_plan_canonical
        .insert(week_key.clone(), event_id.to_string());
    state.weekly_plan_rendered.insert(week_key, msg.to_owned());
    if !state.reminder_sent("*", year, week, 0, &ReminderKind::Initial) {
        state.mark_reminder_sent("*", year, week, 0, ReminderKind::Initial);
    }
    state.save(&ctx.state_path).await
}

async fn tick(ctx: &BotContext, client: &Client) -> anyhow::Result<()> {
    let tz: Tz = ctx
        .config
        .schedule
        .timezone
        .parse()
        .unwrap_or(chrono_tz::UTC);
    let local_now = chrono::Utc::now().with_timezone(&tz);
    let local_weekday = local_now.weekday().num_days_from_monday() as u8;
    let today = local_now.date_naive();
    let (remind_h, remind_m) = parse_hhmm(&ctx.config.schedule.reminder_time);
    let after_hour = (local_now.hour() as u8, local_now.minute() as u8) >= (remind_h, remind_m);
    let (year, week) = current_iso_week();

    let room = match client.get_room(&ctx.room_id) {
        Some(r) => r,
        None => {
            warn!("Scheduler: bot is not in room {}", ctx.room_id);
            return Ok(());
        }
    };
    if !after_hour {
        return Ok(());
    }

    // ── Weekly plan: every turn of the week, posted and pinned once ──────────
    let plan = {
        let state = ctx.state.lock().await;
        let week_key = format!("{year}-W{week:02}");
        let plan_groups: Vec<CleaningGroup> = state
            .cleaning_groups
            .iter()
            .filter(|g| state.belongs_in_weekly_plan(g, year, week))
            .cloned()
            .collect();
        // Per-group initial reminders are from the scheduler before the
        // consolidated plan; a week that has them needs no plan.
        let any_per_group_sent = state
            .cleaning_groups
            .iter()
            .any(|g| state.reminder_sent(&g.id, year, week, 0, &ReminderKind::Initial));
        (local_weekday == ctx.config.schedule.reminder_weekday
            && !state.reminder_sent("*", year, week, 0, &ReminderKind::Initial)
            && !any_per_group_sent
            && !state.weekly_plan_canonical.contains_key(&week_key)
            && !plan_groups.is_empty())
        .then(|| build_weekly_plan(&state, year, week, &plan_groups))
    };
    if let Some((msg, mxids)) = plan {
        let resp = room
            .send(mention_message(&msg, &mxids, &room).await)
            .await
            .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
        let plan_eid = resp.response.event_id.clone();
        register_weekly_plan_message(ctx, year, week, &msg, &plan_eid).await?;
        info!("Sent consolidated weekly plan for week {week}/{year}");

        // Self-react ✅ (UI affordance) then update room pin.
        room.send(ReactionEventContent::new(Annotation::new(
            plan_eid.clone(),
            "✅".to_string(),
        )))
        .await
        .ok();
        pin_weekly_plan(&room, &plan_eid).await;
    }

    // ── Turn reminders: a shift starting today, open turns ending today ──────
    for kind in [ReminderKind::Initial, ReminderKind::Final] {
        let due = {
            let state = ctx.state.lock().await;
            let turns = turns_to_remind(
                &state,
                today,
                &kind,
                ctx.config.schedule.final_reminder_weekday,
            );
            (!turns.is_empty()).then(|| {
                let (msg, mxids) = build_turn_reminder(&state, &kind, &turns);
                let reply_to = state
                    .weekly_plan_canonical
                    .get(&format!("{year}-W{week:02}"))
                    .cloned();
                (turns, msg, mxids, reply_to)
            })
        };
        let Some((turns, msg, mxids, reply_to)) = due else {
            continue;
        };
        let mut content = mention_message(&msg, &mxids, &room).await;
        if let Some(plan_eid) = reply_to.and_then(|e| e.parse::<OwnedEventId>().ok()) {
            content.relates_to = Some(Relation::Reply(Reply::with_event_id(plan_eid)));
        }
        room.send(content)
            .await
            .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
        let mut state = ctx.state.lock().await;
        for (group, turn) in &turns {
            state.mark_reminder_sent(&group.id, turn.year, turn.week, turn.shift, kind.clone());
        }
        state.save(&ctx.state_path).await?;
        info!("Sent {kind:?} reminder for {} turn(s)", turns.len());
    }
    Ok(())
}

/// Turns that get a reminder of `kind` today and haven't had one:
/// - `Initial`: a shift that starts today and isn't the first of its week
///   (the first is announced by the weekly plan);
/// - `Final`: an open turn on its last day — for whole-week turns the
///   configured `final_reminder_weekday` instead.
pub(crate) fn turns_to_remind(
    state: &crate::state::State,
    today: chrono::NaiveDate,
    kind: &ReminderKind,
    final_weekday: u8,
) -> Vec<(CleaningGroup, Turn)> {
    let (year, week) = (today.iso_week().year(), today.iso_week().week());
    let weekday = today.weekday().num_days_from_monday() as u8;
    let mut due = Vec::new();
    for group in state.cleaning_groups.iter().filter(|g| g.is_active) {
        for turn in state.turns_in_week(group, year, week) {
            let Some(shift) = group.rhythm.shift(turn.shift) else {
                continue;
            };
            let wanted = match kind {
                ReminderKind::Initial => turn.shift > 0 && shift.start == weekday,
                ReminderKind::Final if group.rhythm.is_split() => shift.end == weekday,
                ReminderKind::Final => final_weekday == weekday,
                ReminderKind::WeeklySummary => false,
            };
            // The consolidated final reminder of the old scheduler covered
            // every whole-week group of its week.
            let legacy_final = matches!(kind, ReminderKind::Final)
                && state.reminder_sent("*", year, week, 0, kind);
            if wanted
                && !legacy_final
                && !state.is_turn_done(group, turn)
                && !state.reminder_sent(&group.id, year, week, turn.shift, kind)
            {
                due.push((group.clone(), turn));
            }
        }
    }
    due
}

// ── Announce helper (used by scheduler tick and !plan announce command) ────────

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
    // ── Phase 1: build message (brief lock) ───────────────────────────────────
    let (msg, mxids) = {
        let state = ctx.state.lock().await;
        let due_groups: Vec<_> = state
            .cleaning_groups
            .iter()
            .filter(|g| state.belongs_in_weekly_plan(g, year, week))
            .cloned()
            .collect();
        if due_groups.is_empty() {
            return Ok(None);
        }
        build_weekly_plan(&state, year, week, &due_groups)
    };

    // ── Phase 2: send message (no lock held) ─────────────────────────────────
    let resp = room
        .send(mention_message(&msg, &mxids, room).await)
        .await
        .map_err(|e| anyhow::anyhow!("announce send failed: {e}"))?;
    let new_eid = resp.response.event_id.clone();

    // ── Phase 3: update state (brief lock, via shared bookkeeping helper) ─────
    // (Removes ALL stale `weekly_plan_event_ids` entries for this week so old
    // reactions stop working, registers the new event as canonical, snapshots
    // its rendered text, and marks the initial reminder sent.)
    register_weekly_plan_message(ctx, year, week, &msg, &new_eid).await?;

    // ── Phase 4: react + pin (no lock held) ──────────────────────────────────
    room.send(ReactionEventContent::new(Annotation::new(
        new_eid.clone(),
        "✅".to_string(),
    )))
    .await
    .ok();
    pin_weekly_plan(room, &new_eid).await;

    Ok(Some(new_eid))
}

// ── Room pin management ───────────────────────────────────────────────────────

/// Pin `new_eid` and unpin every currently pinned event in the room.
/// Reads live room state so it catches messages pinned before state tracking began.
pub(crate) async fn pin_weekly_plan(room: &Room, new_eid: &OwnedEventId) {
    for old_eid in room.pinned_event_ids().unwrap_or_default() {
        if old_eid == *new_eid {
            continue;
        }
        if let Err(e) = room.unpin_event(&old_eid).await {
            warn!("Failed to unpin {old_eid}: {e}");
        }
    }
    if let Err(e) = room.pin_event(new_eid).await {
        warn!("Failed to pin weekly plan: {e}");
    }
}

// ── Startup reconciliation ──────────────────────────────────────────────────

/// What startup reconciliation should do about the current week's plan
/// message. Deciding this is kept pure/synchronous (no Matrix I/O, no
/// `weekly_plan_rendered` cache) so it can be unit tested directly —
/// `reconcile_on_startup` is the thin async wrapper that fetches the actual
/// live Matrix content via `room.event()` and then carries out whichever
/// action this returns.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlanReconcileAction {
    /// No plan is tracked for this week yet — nothing to check.
    NoPlanTracked,
    /// Nothing is due/completed for this week — no message should exist.
    NothingDue,
    /// The tracked message exists and its live content already matches
    /// persisted state.
    AlreadyConsistent,
    /// The tracked message exists but its live content differs from what
    /// state says it should be — edit it in place.
    NeedsEdit { event_id: OwnedEventId },
    /// The tracked message is missing, redacted, or otherwise inaccessible —
    /// send a replacement and re-register its event id.
    NeedsRecreate,
}

/// Pure decision step: given persisted state and the *actual current
/// content Matrix is showing* for the tracked event, decide what (if
/// anything) needs to happen to bring the message back in line with state.
///
/// `expected_effective_body` and `actual_body` must both be the final,
/// display-name-resolved message body (i.e. `RoomMessageEventContent::body()`
/// after `mentionify_with_names`, and — for `actual_body` — after resolving
/// any bundled Matrix edit) so the comparison isn't fooled by formatting.
/// Deliberately does **not** consult `weekly_plan_rendered`: that field is
/// only a same-process cache for `refresh_pinned_plan`'s fast path, and must
/// never substitute for actually checking what Matrix currently shows here —
/// otherwise a message edited/changed out from under the bot (or a state
/// restore that leaves the cache correct but reality wrong) would go
/// undetected.
pub(crate) fn decide_plan_reconcile_action(
    state: &crate::state::State,
    year: i32,
    week: u32,
    expected_effective_body: &str,
    actual_body: Option<&str>,
) -> PlanReconcileAction {
    let week_key = format!("{year}-W{week:02}");
    let Some(event_id) = state
        .weekly_plan_canonical
        .get(&week_key)
        .and_then(|s| s.parse::<OwnedEventId>().ok())
    else {
        return PlanReconcileAction::NoPlanTracked;
    };
    let any_due = state
        .cleaning_groups
        .iter()
        .any(|g| state.belongs_in_weekly_plan(g, year, week));
    if !any_due {
        return PlanReconcileAction::NothingDue;
    }

    match actual_body {
        None => PlanReconcileAction::NeedsRecreate,
        Some(actual) if actual == expected_effective_body => PlanReconcileAction::AlreadyConsistent,
        Some(_) => PlanReconcileAction::NeedsEdit { event_id },
    }
}

/// Extract the *effective* current body of a fetched plan message: the
/// bundled `m.replace` edit's new content if the homeserver bundled one in
/// `unsigned` (the standard aggregation format for event replacements —
/// see the Matrix spec's "Event replacements" section), otherwise the
/// event's own body. `None` when there's no usable body at all, which also
/// covers a redacted event (redaction empties `content`) without needing a
/// separate check.
fn effective_plan_body(evt: &matrix_sdk::deserialized_responses::TimelineEvent) -> Option<String> {
    let raw: serde_json::Value = evt.kind.raw().deserialize_as().ok()?;
    if let Some(new_body) = raw
        .pointer("/unsigned/m.relations/m.replace/content/m.new_content/body")
        .and_then(|v| v.as_str())
    {
        return Some(new_body.to_owned());
    }
    raw.pointer("/content/body")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// Startup self-healing for the current week's pinned plan message: fetch
/// what Matrix is *actually, currently* showing for the tracked event
/// (resolving any edit) and compare it against what persisted state says it
/// should be, repairing on any mismatch (stale/edited-elsewhere → edit in
/// place; missing/redacted → recreate and re-pin). Persisted `State` is
/// authoritative throughout — Matrix is only ever read to check the live
/// message, never to reconstruct state.
///
/// Intended to be called once, after the initial sync completes and before
/// the scheduler loop starts ticking, so this can never race a concurrent
/// `refresh_pinned_plan`/`announce_weekly_plan`/tick call for the same week.
/// A failure here (missing room, network error, ...) is logged and does not
/// prevent the bot from starting.
pub async fn reconcile_on_startup(ctx: &BotContext, client: &Client) {
    let Some(room) = client.get_room(&ctx.room_id) else {
        warn!(
            "Reconcile: bot is not in room {} — skipping startup reconciliation",
            ctx.room_id
        );
        return;
    };
    let (year, week) = current_iso_week();
    let week_key = format!("{year}-W{week:02}");
    info!("Reconcile: checking weekly plan for {week_key} against persisted state");

    // Cheap pure pre-check (dummy body args — only NoPlanTracked/NothingDue
    // are inspected here) so a brand-new/idle week never costs a Matrix
    // round-trip.
    let stored_eid: Option<OwnedEventId> = {
        let state = ctx.state.lock().await;
        match decide_plan_reconcile_action(&state, year, week, "", None) {
            PlanReconcileAction::NoPlanTracked => {
                info!("Reconcile: no weekly plan tracked for {week_key} — nothing to check");
                None
            }
            PlanReconcileAction::NothingDue => {
                info!("Reconcile: nothing due/completed for {week_key} — nothing to check");
                None
            }
            _ => state
                .weekly_plan_canonical
                .get(&week_key)
                .and_then(|s| s.parse().ok()),
        }
    };
    let Some(stored_eid) = stored_eid else {
        return;
    };

    // What state says the message should show, with member names resolved
    // exactly the way a real send/edit would resolve them — computed once
    // and reused both for the comparison below and (if a repair turns out
    // to be needed) as the content actually sent, so the two can never
    // disagree with each other.
    let (raw_msg, mxids) = {
        let state = ctx.state.lock().await;
        let due_groups: Vec<_> = state
            .cleaning_groups
            .iter()
            .filter(|g| state.belongs_in_weekly_plan(g, year, week))
            .cloned()
            .collect();
        build_weekly_plan(&state, year, week, &due_groups)
    };
    let expected_content = mention_message(&raw_msg, &mxids, &room).await;
    let expected_effective_body = expected_content.body().to_owned();

    // What Matrix is *actually, currently* showing — following any edit,
    // not just the original send.
    let actual_body: Option<String> = match room.event(&stored_eid, None).await {
        Ok(evt) => effective_plan_body(&evt),
        Err(e) => {
            use matrix_sdk::ruma::api::error::ErrorKind as MatrixErrorKind;
            if matches!(e.client_api_error_kind(), Some(MatrixErrorKind::NotFound)) {
                None
            } else {
                // Couldn't confirm either way (network hiccup, permissions, ...) —
                // don't risk creating a duplicate off an inconclusive check.
                warn!("Reconcile: could not verify plan event {stored_eid} for {week_key} ({e}) — leaving it untouched this run");
                return;
            }
        }
    };

    let action = {
        let state = ctx.state.lock().await;
        decide_plan_reconcile_action(
            &state,
            year,
            week,
            &expected_effective_body,
            actual_body.as_deref(),
        )
    };

    match action {
        PlanReconcileAction::NoPlanTracked | PlanReconcileAction::NothingDue => {}
        PlanReconcileAction::AlreadyConsistent => {
            info!("Reconcile: {week_key} plan message already matches persisted state (verified against live Matrix content) — no repair needed");
            // Keep the fast-path cache honest too, in case it was stale/missing
            // even though the live content happened to already be correct.
            if let Err(e) =
                register_weekly_plan_message(ctx, year, week, &raw_msg, &stored_eid).await
            {
                warn!("Reconcile: failed to refresh cached plan record for {week_key}: {e}");
            }
        }
        PlanReconcileAction::NeedsEdit { event_id } => {
            info!("Reconcile: {week_key} plan message differs from persisted state (verified against live Matrix content) — editing in place");
            let content =
                expected_content.make_replacement(ReplacementMetadata::new(event_id.clone(), None));
            match room.send(content).await {
                Ok(_) => {
                    match register_weekly_plan_message(ctx, year, week, &raw_msg, &event_id).await {
                        Ok(()) => {
                            info!("Reconcile: repaired {week_key} plan message (edited in place)")
                        }
                        Err(e) => error!(
                            "Reconcile: failed to persist repaired {week_key} plan record: {e}"
                        ),
                    }
                }
                Err(e) => error!("Reconcile: failed to edit {week_key} plan message: {e}"),
            }
        }
        PlanReconcileAction::NeedsRecreate => {
            warn!(
                "Reconcile: stored plan event for {week_key} is missing/inaccessible — recreating"
            );
            match room.send(expected_content).await {
                Ok(resp) => {
                    let new_eid = resp.response.event_id.clone();
                    match register_weekly_plan_message(ctx, year, week, &raw_msg, &new_eid).await {
                        Ok(()) => {
                            room.send(ReactionEventContent::new(Annotation::new(
                                new_eid.clone(),
                                "✅".to_string(),
                            )))
                            .await
                            .ok();
                            pin_weekly_plan(&room, &new_eid).await;
                            info!("Reconcile: recreated {week_key} plan message ({new_eid}) and pinned it");
                        }
                        Err(e) => error!(
                            "Reconcile: failed to persist recreated {week_key} plan record: {e}"
                        ),
                    }
                }
                Err(e) => error!("Reconcile: failed to recreate {week_key} plan message: {e}"),
            }
        }
    }
}

// ── Message builders ──────────────────────────────────────────────────────────

pub(crate) fn build_weekly_plan(
    state: &crate::state::State,
    year: i32,
    week: u32,
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
        for turn in state.turns_in_week(group, year, week) {
            for (line, mxid) in turn_lines(state, group, turn, true) {
                lines.push(line);
                all_mxids.extend(mxid);
            }
        }
        lines.push(String::new());
    }

    all_mxids.sort();
    all_mxids.dedup();
    (lines.join("\n"), all_mxids)
}

/// One line per slot of a turn, with the assignee's MXID for mentions:
/// "⬜ person", "⬜ Slot · person", "⬜ Mon–Wed · person" — prefixed with the
/// shift for groups split into shifts. `with_done` also lists finished slots.
fn turn_lines(
    state: &crate::state::State,
    group: &CleaningGroup,
    turn: Turn,
    with_done: bool,
) -> Vec<(String, Option<String>)> {
    let shift = turn
        .shift_label(&group.rhythm)
        .map(|l| format!("{l} · "))
        .unwrap_or_default();
    let mut out = Vec::new();
    for (slot_index, assignee) in state.turn_assignees(group, turn) {
        let slot = group.slots.get(slot_index);
        let rooms = match slot {
            Some(slot) if !slot.room_names.is_empty() => {
                format!(" · {}", slot.room_names.join(", "))
            }
            Some(_) => String::new(),
            None => group
                .rooms_text()
                .map(|r| format!(" · {}", r.replace('\n', " · ")))
                .unwrap_or_default(),
        };
        let slot_name = slot.map(|s| format!("{} · ", s.name)).unwrap_or_default();
        let completion = state.completion_for(group, slot_index, turn);
        if completion.is_some() && !with_done {
            continue;
        }
        let line = match (assignee, completion) {
            (None, done) => format!(
                "{} {shift}{}{rooms}",
                status_icon(done.is_some()),
                slot.map_or("nobody assigned", |s| s.name.as_str())
            ),
            (Some(p), Some(c)) if c.skipped => {
                format!("⏭️ {shift}{slot_name}{} · skipped{rooms}", person_label(p))
            }
            (Some(p), Some(_)) => format!("✅ {shift}{} · cleaned{rooms}", person_label(p)),
            (Some(p), None) => {
                let away = away_suffix(state, &p.id, &group.id, turn.year, turn.week);
                format!("⬜ {shift}{slot_name}{}{away}{rooms}", person_label(p))
            }
        };
        out.push((line, assignee.and_then(|p| p.matrix_id.clone())));
    }
    out
}

/// A consolidated turn reminder: the shifts starting today, or the open
/// turns ending today.
fn build_turn_reminder(
    state: &crate::state::State,
    kind: &ReminderKind,
    turns: &[(CleaningGroup, Turn)],
) -> (String, Vec<String>) {
    let title = match kind {
        ReminderKind::Initial => "🔔 **Your turn starts today**",
        _ => "⏰ **Still open · ends today**",
    };
    let mut lines = vec![title.to_owned(), String::new()];
    let mut all_mxids: Vec<String> = Vec::new();
    for (group, turn) in turns {
        lines.push(format!("**{}**", group.name));
        for (line, mxid) in turn_lines(state, group, *turn, false) {
            lines.push(line);
            all_mxids.extend(mxid);
        }
        lines.push(String::new());
    }
    all_mxids.sort();
    all_mxids.dedup();
    (lines.join("\n"), all_mxids)
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
    if done {
        "✅"
    } else {
        "⬜"
    }
}

/// " (🌴 away)" when the shown assignee is on record absence for this
/// (group, week) — purely a display hint. `!member away` never reassigns an
/// already-frozen week automatically (see `resolver::materialize`'s
/// eligibility filter, which only applies to not-yet-frozen picks); this
/// just makes it visible on the plan that the frozen assignee won't be
/// doing it themselves, so `!takeover`/`!swap`/`!plan assign` is expected.
fn away_suffix(
    state: &crate::state::State,
    person_id: &crate::domain::PersonId,
    group_id: &crate::domain::GroupId,
    year: i32,
    week: u32,
) -> &'static str {
    if state.is_absent(person_id, group_id, year, week) {
        " (🌴 away)"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CleaningGroup, Person, SlotAssignment},
        state::{Absence, State},
    };
    use std::collections::HashMap;

    #[test]
    fn plan_marks_an_already_frozen_but_now_absent_assignee_as_away() {
        // The frozen assignment itself is untouched by the absence (see
        // resolver::materialize's tests) — this only checks that the
        // dashboard additionally surfaces it, so someone knows a
        // takeover/swap/assign is expected instead of Alice showing up.
        let alice = Person::new_matrix("@alice:example.org");
        let aid = alice.id.clone();
        let mut group = CleaningGroup::new("Kitchen");
        let gid = group.id.clone();
        group.member_ids = vec![aid.clone()];

        let mut state = State::default();
        state.persons.push(alice);
        let year = 2024;
        let week = 10;
        state.slot_assignments.push(SlotAssignment {
            group_id: gid.clone(),
            slot_index: 0,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(aid.clone()),
            source: Default::default(),
        });
        state.absences.push(Absence {
            person_id: aid,
            group_id: gid.clone(),
            from_year: year,
            from_week: week,
            duration_weeks: 1,
        });
        state.cleaning_groups.push(group);

        let (plan, _mxids) = build_weekly_plan(&state, year, week, &state.cleaning_groups);
        assert!(plan.contains("🌴 away"), "{plan}");
    }

    #[test]
    fn refreshing_the_plan_after_completion_keeps_the_person_visible_as_done() {
        // Reproduces the group-disappears-on-refresh bug: `refresh_pinned_plan`
        // and `announce_weekly_plan` used to select which groups to render via
        // `is_due` alone. Once the sole responsible person marked their task
        // done, `is_due` flipped to false and the whole group — including the
        // person who just finished — dropped out of the re-rendered message
        // instead of showing "✅ ... cleaned". `belongs_in_weekly_plan` is what
        // both call sites now filter on; this checks it end-to-end through
        // `build_weekly_plan`.
        let bob = Person::new_matrix("@bob:example.org");
        let bid = bob.id.clone();
        let mut group = CleaningGroup::new("Kitchen");
        let gid = group.id.clone();
        group.member_ids = vec![bid.clone()];

        let mut state = State::default();
        state.persons.push(bob);
        let year = 2024;
        let week = 10;
        state.slot_assignments.push(SlotAssignment {
            group_id: gid.clone(),
            slot_index: 0,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(bid.clone()),
            source: Default::default(),
        });
        state.completions.push(crate::state::Completion {
            group_id: gid.clone(),
            slot_id: None,
            completed_by_id: bid,
            responsible_person_ids: vec![],
            iso_year: year,
            iso_week: week,
            shift: 0,
            completed_at: chrono::Utc::now(),
            skipped: false,
        });
        state.cleaning_groups.push(group);

        // What refresh_pinned_plan / announce_weekly_plan now render.
        let due_groups: Vec<_> = state
            .cleaning_groups
            .iter()
            .filter(|g| state.belongs_in_weekly_plan(g, year, week))
            .cloned()
            .collect();
        assert_eq!(
            due_groups.len(),
            1,
            "the completed group must still be included"
        );

        let (plan, _mxids) = build_weekly_plan(&state, year, week, &due_groups);
        assert!(
            plan.contains("✅ bob") || plan.contains("✅ @bob:example.org"),
            "{plan}"
        );
        assert!(plan.contains("cleaned"), "{plan}");
    }

    // ── Mentions ──────────────────────────────────────────────────────────────
    //
    // Regression coverage for a bug where `build_weekly_plan` rendered a
    // *completed* assignee's line with `p.display_name` instead of
    // `person_label(p)`. Since `person_label` is what puts the raw
    // `@mxid:server` token into the text for `extract_mxids`/`mentionify_*`
    // to find, the completed line silently stopped producing a real Matrix
    // mention (no `m.mentions` entry, no clickable pill) — it just showed
    // plain display-name text, even though the person was still a room
    // member with a valid Matrix ID.

    fn plan_body(c: &RoomMessageEventContent) -> (String, Option<String>) {
        use matrix_sdk::ruma::events::room::message::MessageType;
        match &c.msgtype {
            MessageType::Text(t) => (t.body.clone(), t.formatted.as_ref().map(|f| f.body.clone())),
            _ => panic!("unexpected msgtype"),
        }
    }

    fn uid(s: &str) -> OwnedUserId {
        <&matrix_sdk::ruma::UserId>::try_from(s).unwrap().to_owned()
    }

    #[test]
    fn weekly_plan_mentions_a_completed_room_member_with_a_real_mxid_pill() {
        let alice = Person::new_matrix("@alice:example.org");
        let aid = alice.id.clone();
        let mut group = CleaningGroup::new("Kitchen");
        let gid = group.id.clone();
        group.member_ids = vec![aid.clone()];

        let mut state = State::default();
        state.persons.push(alice);
        let (year, week) = (2024, 10);
        state.slot_assignments.push(SlotAssignment {
            group_id: gid.clone(),
            slot_index: 0,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(aid.clone()),
            source: Default::default(),
        });
        state.completions.push(crate::state::Completion {
            group_id: gid.clone(),
            slot_id: None,
            completed_by_id: aid,
            responsible_person_ids: vec![],
            iso_year: year,
            iso_week: week,
            shift: 0,
            completed_at: chrono::Utc::now(),
            skipped: false,
        });
        state.cleaning_groups.push(group);

        let due_groups: Vec<_> = state
            .cleaning_groups
            .iter()
            .filter(|g| state.belongs_in_weekly_plan(g, year, week))
            .cloned()
            .collect();
        let (raw_msg, mxids) = build_weekly_plan(&state, year, week, &due_groups);
        assert!(
            mxids.contains(&"@alice:example.org".to_string()),
            "{mxids:?}"
        );

        let mut names = HashMap::new();
        names.insert("@alice:example.org".to_string(), "Alice".to_string());
        let content = crate::format::mentionify_with_names(&raw_msg, &names);

        let mentions = content
            .mentions
            .clone()
            .expect("completed member must still produce m.mentions");
        assert!(
            mentions.user_ids.contains(&uid("@alice:example.org")),
            "{mentions:?}"
        );

        let (_, html) = plan_body(&content);
        let html = html.expect("should have an HTML body with a mention pill");
        assert!(
            html.contains(r#"href="https://matrix.to/#/@alice:example.org""#),
            "expected a real mention pill, got: {html}"
        );
        assert!(
            html.contains(">Alice<"),
            "pill should show the display name: {html}"
        );
    }

    #[test]
    fn weekly_plan_mentions_every_assigned_room_member_across_groups() {
        let alice = Person::new_matrix("@alice:example.org");
        let bob = Person::new_matrix("@bob:example.org");
        let (aid, bid) = (alice.id.clone(), bob.id.clone());

        let mut kitchen = CleaningGroup::new("Kitchen");
        kitchen.member_ids = vec![aid.clone()];
        let mut bath = CleaningGroup::new("Bathroom");
        bath.member_ids = vec![bid.clone()];
        let (kid, wid) = (kitchen.id.clone(), bath.id.clone());

        let mut state = State::default();
        state.persons.push(alice);
        state.persons.push(bob);
        let (year, week) = (2024, 10);
        state.slot_assignments.push(SlotAssignment {
            group_id: kid,
            slot_index: 0,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(aid),
            source: Default::default(),
        });
        state.slot_assignments.push(SlotAssignment {
            group_id: wid,
            slot_index: 0,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(bid),
            source: Default::default(),
        });
        state.cleaning_groups.push(kitchen);
        state.cleaning_groups.push(bath);

        let (raw_msg, mxids) = build_weekly_plan(&state, year, week, &state.cleaning_groups);
        assert_eq!(mxids.len(), 2, "{mxids:?}");

        let mut names = HashMap::new();
        names.insert("@alice:example.org".to_string(), "Alice".to_string());
        names.insert("@bob:example.org".to_string(), "Bob".to_string());
        let content = crate::format::mentionify_with_names(&raw_msg, &names);

        let mentions = content.mentions.expect("m.mentions must be set");
        assert_eq!(mentions.user_ids.len(), 2, "{mentions:?}");
        assert!(mentions.user_ids.contains(&uid("@alice:example.org")));
        assert!(mentions.user_ids.contains(&uid("@bob:example.org")));
    }

    #[test]
    fn weekly_plan_falls_back_to_plain_text_for_an_unresolvable_or_matrix_less_person() {
        // No Matrix ID at all: `person_label` falls back to the plain
        // display name, so the text never contains an `@mxid` token — the
        // message must still render without panicking and without a bogus
        // mention.
        let mut carol = Person::new_matrix("@carol:example.org");
        carol.matrix_id = None;
        let cid = carol.id.clone();
        let mut group = CleaningGroup::new("Kitchen");
        let gid = group.id.clone();
        group.member_ids = vec![cid.clone()];

        let mut state = State::default();
        state.persons.push(carol);
        let (year, week) = (2024, 10);
        state.slot_assignments.push(SlotAssignment {
            group_id: gid,
            slot_index: 0,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(cid),
            source: Default::default(),
        });
        state.cleaning_groups.push(group);

        let (raw_msg, mxids) = build_weekly_plan(&state, year, week, &state.cleaning_groups);
        assert!(
            mxids.is_empty(),
            "person without a Matrix ID must not be listed for mention: {mxids:?}"
        );

        // Even if a stale/unresolvable mxid ended up in the text, `fetch_names`
        // simply won't have an entry for it — `mentionify_with_names` must
        // still degrade gracefully (no panic, message still sendable) rather
        // than dropping the line or failing.
        let content = crate::format::mentionify_with_names(&raw_msg, &HashMap::new());
        let (plain, _) = plan_body(&content);
        assert!(plain.contains("carol"), "{plain}");
    }

    #[test]
    fn refreshing_the_plan_to_completed_preserves_the_mention_across_the_edit() {
        // The same `build_weekly_plan` + `mentionify_with_names` pipeline is
        // reused by the initial send, `refresh_pinned_plan`'s edit path, and
        // startup reconciliation's edit/recreate path — so a mention present
        // before completion must still be present in the re-rendered text
        // used for the edit after completion.
        let dave = Person::new_matrix("@dave:example.org");
        let did = dave.id.clone();
        let mut group = CleaningGroup::new("Kitchen");
        let gid = group.id.clone();
        group.member_ids = vec![did.clone()];

        let mut state = State::default();
        state.persons.push(dave);
        let (year, week) = (2024, 10);
        state.slot_assignments.push(SlotAssignment {
            group_id: gid.clone(),
            slot_index: 0,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(did.clone()),
            source: Default::default(),
        });
        state.cleaning_groups.push(group.clone());

        let mut names = HashMap::new();
        names.insert("@dave:example.org".to_string(), "Dave".to_string());

        // Before completion.
        let (msg_open, mxids_open) = build_weekly_plan(&state, year, week, &state.cleaning_groups);
        assert!(mxids_open.contains(&"@dave:example.org".to_string()));
        let mentions_open = crate::format::mentionify_with_names(&msg_open, &names)
            .mentions
            .expect("m.mentions must be set before completion");
        assert!(mentions_open.user_ids.contains(&uid("@dave:example.org")));

        // Dave marks his task done — this is what triggers the edit/refresh.
        state.completions.push(crate::state::Completion {
            group_id: gid.clone(),
            slot_id: None,
            completed_by_id: did,
            responsible_person_ids: vec![],
            iso_year: year,
            iso_week: week,
            shift: 0,
            completed_at: chrono::Utc::now(),
            skipped: false,
        });

        let due_groups: Vec<_> = state
            .cleaning_groups
            .iter()
            .filter(|g| state.belongs_in_weekly_plan(g, year, week))
            .cloned()
            .collect();
        let (msg_done, mxids_done) = build_weekly_plan(&state, year, week, &due_groups);
        assert!(
            mxids_done.contains(&"@dave:example.org".to_string()),
            "{mxids_done:?}"
        );

        let content_done = crate::format::mentionify_with_names(&msg_done, &names);
        let mentions_done = content_done
            .mentions
            .clone()
            .expect("m.mentions must survive the edit after completion");
        assert!(mentions_done.user_ids.contains(&uid("@dave:example.org")));

        let (_, html_done) = plan_body(&content_done);
        let html_done = html_done.expect("edited message should still carry a mention pill");
        assert!(
            html_done.contains(r#"href="https://matrix.to/#/@dave:example.org""#),
            "{html_done}"
        );
    }

    // ── Startup reconciliation ────────────────────────────────────────────────

    fn timeline_event_from_json(
        v: serde_json::Value,
    ) -> matrix_sdk::deserialized_responses::TimelineEvent {
        let raw: matrix_sdk::ruma::serde::Raw<matrix_sdk::ruma::events::AnySyncTimelineEvent> =
            serde_json::from_value(v).unwrap();
        matrix_sdk::deserialized_responses::TimelineEvent::from_plaintext(raw)
    }

    #[test]
    fn effective_plan_body_prefers_a_bundled_edit_over_the_original_content() {
        // Exercises the actual JSON shape the homeserver bundles under
        // `unsigned.m.relations.m.replace` for an edited event (per the
        // Matrix spec's event-replacement aggregation format) — this is what
        // makes `reconcile_on_startup` see the *effective* (post-edit) body
        // instead of accidentally comparing against the stale original body.
        let evt = timeline_event_from_json(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$plan1:example.org",
            "sender": "@bot:example.org",
            "origin_server_ts": 0,
            "room_id": "!room:example.org",
            "content": { "msgtype": "m.text", "body": "original text" },
            "unsigned": {
                "m.relations": {
                    "m.replace": {
                        "type": "m.room.message",
                        "event_id": "$edit1:example.org",
                        "sender": "@bot:example.org",
                        "origin_server_ts": 1,
                        "room_id": "!room:example.org",
                        "content": {
                            "msgtype": "m.text",
                            "body": "* edited text",
                            "m.new_content": { "msgtype": "m.text", "body": "edited text" },
                            "m.relates_to": { "rel_type": "m.replace", "event_id": "$plan1:example.org" }
                        }
                    }
                }
            }
        }));

        assert_eq!(effective_plan_body(&evt).as_deref(), Some("edited text"));
    }

    #[test]
    fn effective_plan_body_falls_back_to_the_original_body_when_never_edited() {
        let evt = timeline_event_from_json(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$plan1:example.org",
            "sender": "@bot:example.org",
            "origin_server_ts": 0,
            "room_id": "!room:example.org",
            "content": { "msgtype": "m.text", "body": "never edited" }
        }));

        assert_eq!(effective_plan_body(&evt).as_deref(), Some("never edited"));
    }

    #[test]
    fn effective_plan_body_is_none_for_a_redacted_event() {
        // Redaction empties `content`, so neither the bundled-edit pointer
        // nor the plain-body pointer resolves — this is what makes a
        // redacted (but still fetchable, so not a 404) event correctly fall
        // into the NeedsRecreate path rather than being misread as an empty
        // but "matching" body.
        let evt = timeline_event_from_json(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$plan1:example.org",
            "sender": "@bot:example.org",
            "origin_server_ts": 0,
            "room_id": "!room:example.org",
            "content": {},
            "unsigned": { "redacted_because": { "type": "m.room.redaction", "event_id": "$r:example.org" } }
        }));

        assert_eq!(effective_plan_body(&evt), None);
    }

    /// One active group with one due member and a canonical plan event
    /// already registered for the current week. Mirrors what state looks
    /// like right after a real plan message has been sent at some point in
    /// the past. `$plan1:example.org` is a syntactically valid (if fake)
    /// Matrix event id, used only for equality checks against `NeedsEdit`.
    fn plan_reconcile_fixture() -> (State, i32, u32, String) {
        let bob = Person::new_matrix("@bob:example.org");
        let bid = bob.id.clone();
        let mut group = CleaningGroup::new("Kitchen");
        let gid = group.id.clone();
        group.member_ids = vec![bid.clone()];

        let mut state = State::default();
        state.persons.push(bob);
        let (year, week) = (2024, 10);
        state.slot_assignments.push(SlotAssignment {
            group_id: gid,
            slot_index: 0,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(bid),
            source: Default::default(),
        });
        state.cleaning_groups.push(group);

        let week_key = format!("{year}-W{week:02}");
        state
            .weekly_plan_canonical
            .insert(week_key.clone(), "$plan1:example.org".to_owned());
        (state, year, week, week_key)
    }

    fn plan1_event_id() -> OwnedEventId {
        "$plan1:example.org".parse().unwrap()
    }

    #[test]
    fn reconcile_no_plan_tracked_and_nothing_due_are_left_alone() {
        let state = State::default();
        assert_eq!(
            decide_plan_reconcile_action(&state, 2024, 10, "", None),
            PlanReconcileAction::NoPlanTracked,
            "nothing stored for this week at all"
        );

        let (mut untracked, year, week, week_key) = plan_reconcile_fixture();
        untracked.weekly_plan_canonical.remove(&week_key);
        assert_eq!(
            decide_plan_reconcile_action(&untracked, year, week, "", None),
            PlanReconcileAction::NoPlanTracked
        );
    }

    #[test]
    fn reconcile_reports_already_consistent_when_state_and_live_matrix_content_match() {
        let (state, year, week, _) = plan_reconcile_fixture();
        let (raw_msg, _) = build_weekly_plan(&state, year, week, &state.cleaning_groups);

        // `expected_effective_body` and `actual_body` are the same string,
        // simulating a `room.event()` fetch that shows exactly what state
        // expects.
        assert_eq!(
            decide_plan_reconcile_action(&state, year, week, &raw_msg, Some(raw_msg.as_str())),
            PlanReconcileAction::AlreadyConsistent
        );
    }

    #[test]
    fn reconcile_edits_a_stale_matrix_message() {
        let (state, year, week, _) = plan_reconcile_fixture();
        let (raw_msg, _) = build_weekly_plan(&state, year, week, &state.cleaning_groups);
        let stale_live_content = "this is what an old, out-of-date plan text looked like";

        assert_eq!(
            decide_plan_reconcile_action(&state, year, week, &raw_msg, Some(stale_live_content)),
            PlanReconcileAction::NeedsEdit {
                event_id: plan1_event_id()
            }
        );
    }

    #[test]
    fn reconcile_recreates_a_missing_matrix_message() {
        let (state, year, week, _) = plan_reconcile_fixture();
        let (raw_msg, _) = build_weekly_plan(&state, year, week, &state.cleaning_groups);

        // `actual_body: None` is what the caller passes when `room.event()`
        // confirmed the event is gone (404) or redacted/unusable.
        assert_eq!(
            decide_plan_reconcile_action(&state, year, week, &raw_msg, None),
            PlanReconcileAction::NeedsRecreate
        );
    }

    #[test]
    fn reconcile_edits_when_completion_state_differs_from_what_matrix_shows() {
        // Matrix still shows the ⬜ open state; state says it's since been
        // completed. This is the "⬜ vs ✅ drifted" case the task calls out
        // by name, distinct from an arbitrary stale-text edit.
        let (mut state, year, week, _) = plan_reconcile_fixture();
        let (open_text, _) = build_weekly_plan(&state, year, week, &state.cleaning_groups);
        assert!(open_text.contains('⬜'), "{open_text}");

        let bid = state.persons[0].id.clone();
        let gid = state.cleaning_groups[0].id.clone();
        state.completions.push(crate::state::Completion {
            group_id: gid,
            slot_id: None,
            completed_by_id: bid,
            responsible_person_ids: vec![],
            iso_year: year,
            iso_week: week,
            shift: 0,
            completed_at: chrono::Utc::now(),
            skipped: false,
        });
        let (expected_now, _) = build_weekly_plan(&state, year, week, &state.cleaning_groups);
        assert!(
            expected_now.contains('✅') && expected_now.contains("cleaned"),
            "{expected_now}"
        );

        // Matrix (`open_text`) hasn't caught up with the completion yet.
        assert_eq!(
            decide_plan_reconcile_action(
                &state,
                year,
                week,
                &expected_now,
                Some(open_text.as_str())
            ),
            PlanReconcileAction::NeedsEdit {
                event_id: plan1_event_id()
            }
        );
    }

    #[test]
    fn reconcile_detects_drift_even_when_the_rendered_cache_agrees_with_state() {
        // Regression test: state says the expected plan text is A,
        // `weekly_plan_rendered` (the same-process "what we last believe we
        // sent" cache) *also* says A — but the actual Matrix message has
        // since been edited/changed to B by something other than this bot
        // (or the cache is simply stale/wrong, e.g. after a state restore).
        // Before this fix, reconciliation compared the expected text only
        // against `weekly_plan_rendered` and would have reported
        // AlreadyConsistent here, leaving Matrix at B. It must instead be
        // driven by the live fetched content — `decide_plan_reconcile_action`
        // doesn't even accept `weekly_plan_rendered` as an input, by design.
        let (mut state, year, week, week_key) = plan_reconcile_fixture();
        let (raw_msg, _) = build_weekly_plan(&state, year, week, &state.cleaning_groups);

        // State and the cache agree with each other ("A")...
        state.weekly_plan_rendered.insert(week_key, raw_msg.clone());

        // ...but Matrix is actually showing something else ("B").
        let live_matrix_body = "someone hand-edited this pinned message";

        assert_eq!(
            decide_plan_reconcile_action(&state, year, week, &raw_msg, Some(live_matrix_body)),
            PlanReconcileAction::NeedsEdit {
                event_id: plan1_event_id()
            },
            "must repair based on live Matrix content, not the weekly_plan_rendered cache"
        );
    }

    #[test]
    fn reconcile_is_idempotent_across_repeated_runs() {
        let (state, year, week, _) = plan_reconcile_fixture();
        let (raw_msg, _) = build_weekly_plan(&state, year, week, &state.cleaning_groups);

        // First pass: Matrix shows something stale — needs a repair.
        assert_eq!(
            decide_plan_reconcile_action(&state, year, week, &raw_msg, Some("stale")),
            PlanReconcileAction::NeedsEdit {
                event_id: plan1_event_id()
            }
        );

        // Once the repair is applied, Matrix shows exactly `raw_msg` —
        // repeated runs against that same live content must be a no-op,
        // never a repeat edit and never a duplicate message.
        for _ in 0..3 {
            assert_eq!(
                decide_plan_reconcile_action(&state, year, week, &raw_msg, Some(raw_msg.as_str())),
                PlanReconcileAction::AlreadyConsistent
            );
        }
    }
}
