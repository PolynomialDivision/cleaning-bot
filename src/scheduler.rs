use chrono::{Datelike, Timelike};
use chrono_tz::Tz;
use matrix_sdk::{
    ruma::{
        events::{
            reaction::ReactionEventContent,
            relation::Annotation,
            room::{
                message::{ReplacementMetadata, RoomMessageEventContent},
                pinned_events::RoomPinnedEventsEventContent,
            },
            Mentions,
        },
        OwnedEventId, OwnedUserId,
    },
    Client, Room,
};
use mxbot_common::matrix_sdk;
use std::collections::HashSet;
use tracing::{error, info, warn};

use crate::{
    domain::CleaningGroup,
    rhythm::Turn,
    state::{current_iso_week, ReminderKind, ReminderMessage, ReminderNote},
    view, BotContext,
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
    let mut content = crate::format::mentionify_with_names(text, &names);
    content.mentions = Some(Mentions::with_user_ids(parsed));
    content
}

/// The buttons under the plan and a reminder: ✅ done, 🆘 can't make it.
async fn seed_buttons(room: &Room, event_id: &OwnedEventId) {
    for key in ["✅", crate::trades::ASK] {
        room.send(ReactionEventContent::new(Annotation::new(
            event_id.clone(),
            key.to_owned(),
        )))
        .await
        .ok();
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

/// Keep every group's frozen plan `materialize_weeks` deep as time passes —
/// the same additive fill startup does, so how far ahead the plan is fixed
/// no longer depends on when the bot last restarted. Never changes a turn
/// that is already frozen.
pub(crate) async fn roll_planning_horizon(ctx: &BotContext) -> anyhow::Result<()> {
    let mut state = ctx.state.lock().await;
    let events =
        crate::resolver::materialize(&state, ctx.config.schedule.materialize_weeks as usize);
    if events.is_empty() {
        return Ok(());
    }
    for ev in events {
        state.apply_event(ev)?;
    }
    state.save(&ctx.state_path).await
}

/// Edit the pinned weekly plan message — and that week's reminders — to
/// reflect the current completion state.
/// No-op if no plan has been sent for this week yet, or if the rendered
/// content already matches what was last sent (avoids a pointless Matrix edit).
pub(crate) async fn refresh_pinned_plan(ctx: &BotContext, room: &Room, year: i32, week: u32) {
    // The week's reminders show who's done, too — 🆘 requests close once
    // settled.
    refresh_reminders(ctx, room, year, week).await;
    crate::trades::tidy(ctx, room, false).await;
    // Turn menus show who has a turn now, and what was asked.
    crate::turn_menu::refresh_all(ctx, &room.client()).await;
    let week_key = format!("{year}-W{week:02}");

    let (canonical_eid, msg, mxids, already_mentioned) = {
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
        let (msg, mxids) = build_weekly_plan(&state, year, week, &due_groups);
        let previous = state.weekly_plan_rendered.get(&week_key);
        if previous == Some(&msg) {
            return;
        }
        (eid, msg, mxids, previous.map(|p| previous_mentions(p)))
    };

    // Only people new on the plan (a takeover, say) are notified by the
    // edit — not everyone again on every ✅.
    let content =
        mention_message(&msg, &mxids, room)
            .await
            .make_replacement(ReplacementMetadata::new(
                canonical_eid.clone(),
                already_mentioned,
            ));
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

/// The mentions a plan message rendered from `text` carried.
fn previous_mentions(text: &str) -> Mentions {
    Mentions::with_user_ids(
        crate::format::extract_mxids(text)
            .into_iter()
            .filter_map(|m| m.parse::<OwnedUserId>().ok()),
    )
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
    let _operation = ctx.operations.lock().await;
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

    roll_planning_horizon(ctx).await?;

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
    // 🆘 requests: close settled ones; nobody stepped in by the start of
    // the turn → tell the admins.
    crate::trades::tidy(ctx, &room, true).await;

    // ── Weekly plan: every turn of the week, posted and pinned once ──────────
    let plan = {
        let state = ctx.state.lock().await;
        let plan_groups: Vec<CleaningGroup> = state
            .cleaning_groups
            .iter()
            .filter(|g| state.belongs_in_weekly_plan(g, year, week))
            .cloned()
            .collect();
        (!plan_groups.is_empty()
            && weekly_plan_due(
                &state,
                year,
                week,
                local_weekday,
                ctx.config.schedule.reminder_weekday,
            ))
        .then(|| build_weekly_plan(&state, year, week, &plan_groups))
    };
    if let Some((msg, mxids)) = plan {
        let resp = room
            .send(mention_message(&msg, &mxids, &room).await)
            .with_transaction_id(format!("weekly-{year}-{week}").into())
            .await
            .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
        let plan_eid = resp.response.event_id.clone();
        register_weekly_plan_message(ctx, year, week, &msg, &plan_eid).await?;
        info!("Sent consolidated weekly plan for week {week}/{year}");

        // The ✅ and 🆘 buttons, then the pin.
        seed_buttons(&room, &plan_eid).await;
        pin_weekly_plan(ctx, &room, Some(&plan_eid)).await;
    }

    // ── Turn reminders: a shift starting today, open turns ending today ──────
    for kind in [ReminderKind::Initial, ReminderKind::Final] {
        let turns = {
            let state = ctx.state.lock().await;
            turns_to_remind(
                &state,
                today,
                &kind,
                ctx.config.schedule.final_reminder_weekday,
            )
        };
        if turns.is_empty() {
            continue;
        }
        let note = match kind {
            ReminderKind::Initial => ReminderNote::StartsToday,
            _ => ReminderNote::EndsToday,
        };
        // The same reminder, retried after a crash, is sent only once —
        // but a later one the same day (other turns) is a new message.
        let covered: Vec<String> = turns
            .iter()
            .map(|(g, t)| format!("{}:{}:{}:{}", g.id, t.year, t.week, t.shift))
            .collect();
        let txn = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, covered.join(",").as_bytes());
        let turn_ids = turns.iter().map(|(g, t)| (g.id.clone(), t.shift)).collect();
        send_reminder(
            ctx,
            &room,
            note,
            (year, week),
            turn_ids,
            Some(format!("reminder-{kind:?}-{txn}")),
        )
        .await?;
        let mut state = ctx.state.lock().await;
        for (group, turn) in &turns {
            state.mark_reminder_sent(&group.id, turn.year, turn.week, turn.shift, kind.clone());
        }
        state.save(&ctx.state_path).await?;
        info!("Sent {kind:?} reminder for {} turn(s)", turns.len());
    }
    Ok(())
}

/// Whether the weekly plan of `year`/`week` is still to be posted on
/// `weekday`: on the configured `plan_weekday` — or later that week, when
/// the bot was down then — but only once (also across a restart).
pub(crate) fn weekly_plan_due(
    state: &crate::state::State,
    year: i32,
    week: u32,
    weekday: u8,
    plan_weekday: u8,
) -> bool {
    // Per-group initial reminders are from the scheduler before the
    // consolidated plan; a week that has them needs no plan.
    let any_per_group_sent = state
        .cleaning_groups
        .iter()
        .any(|g| state.reminder_sent(&g.id, year, week, 0, &ReminderKind::Initial));
    weekday >= plan_weekday
        && !state.reminder_sent("*", year, week, 0, &ReminderKind::Initial)
        && !any_per_group_sent
        && !state
            .weekly_plan_canonical
            .contains_key(&format!("{year}-W{week:02}"))
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
            let Some(shift) = group.rhythm.for_week(year, week).shift(turn.shift) else {
                continue;
            };
            let wanted = match kind {
                ReminderKind::Initial => turn.shift > 0 && shift.start == weekday,
                ReminderKind::Final if group.rhythm.for_week(year, week).is_split() => {
                    shift.end == weekday
                }
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
    seed_buttons(room, &new_eid).await;
    pin_weekly_plan(ctx, room, Some(&new_eid)).await;

    Ok(Some(new_eid))
}

// ── Room pin management ───────────────────────────────────────────────────────

/// Make `plan` the bot's only pinned message: every other message the bot
/// sent — old plans and reminders of any age or format — is unpinned.
/// Messages people pinned stay.
///
/// Sent as one `m.room.pinned_events` event, computed from the list as the
/// server has it now. (`Room::pin_event`/`unpin_event` each start from the
/// locally cached list, which sync hasn't updated between two calls: a loop
/// of them wrote the earlier pins back, and old plans piled up.)
pub(crate) async fn pin_weekly_plan(ctx: &BotContext, room: &Room, plan: Option<&OwnedEventId>) {
    let pinned = match room.load_pinned_events().await {
        Ok(pinned) => pinned.unwrap_or_default(),
        Err(e) => {
            warn!("Pins: could not read the pinned messages: {e}");
            return;
        }
    };
    let known: HashSet<String> = {
        let state = ctx.state.lock().await;
        state
            .weekly_plan_canonical
            .values()
            .cloned()
            .chain(state.weekly_plan_event_ids.keys().cloned())
            .chain(state.reminder_messages.keys().cloned())
            .collect()
    };
    let mut bots = HashSet::new();
    for id in &pinned {
        if Some(id) != plan && (known.contains(id.as_str()) || is_own_or_gone(room, id).await) {
            bots.insert(id.clone());
        }
    }
    let wanted = wanted_pins(&pinned, plan, &bots);
    if wanted == pinned {
        return;
    }
    let content = RoomPinnedEventsEventContent::new(wanted);
    match room.send_state_event(content).await {
        Ok(_) => info!("Pins: {} unpinned, plan pinned", bots.len()),
        Err(e) => warn!("Pins: could not update the pinned messages: {e}"),
    }
}

/// Whether pinned `id` is the bot's own message — or no longer exists, so
/// the pin shows nothing anyway. When unsure (a network error), no.
async fn is_own_or_gone(room: &Room, id: &OwnedEventId) -> bool {
    match room.event(id, None).await {
        Ok(event) => event
            .kind
            .raw()
            .deserialize_as::<serde_json::Value>()
            .is_ok_and(|v| v["sender"].as_str() == room.client().user_id().map(|u| u.as_str())),
        Err(e) => {
            use matrix_sdk::ruma::api::error::ErrorKind;
            matches!(e.client_api_error_kind(), Some(ErrorKind::NotFound))
        }
    }
}

/// The pinned list without the bot's messages, `plan` last (newest).
fn wanted_pins(
    pinned: &[OwnedEventId],
    plan: Option<&OwnedEventId>,
    bots: &HashSet<OwnedEventId>,
) -> Vec<OwnedEventId> {
    let mut wanted: Vec<OwnedEventId> = pinned
        .iter()
        .filter(|id| !bots.contains(*id) && Some(*id) != plan)
        .cloned()
        .collect();
    wanted.extend(plan.cloned());
    wanted
}

/// On startup: only the newest plan — this week's, or the last one before
/// it — stays pinned of the bot's messages.
pub async fn tidy_pins_on_startup(ctx: &BotContext, room: &Room) {
    let plan = newest_plan(&*ctx.state.lock().await, current_iso_week());
    pin_weekly_plan(ctx, room, plan.as_ref()).await;
}

/// The plan of `week`, or else the newest one before it.
fn newest_plan(state: &crate::state::State, (year, week): (i32, u32)) -> Option<OwnedEventId> {
    let this_week = format!("{year}-W{week:02}");
    state
        .weekly_plan_canonical
        .iter()
        .filter(|(key, _)| key.as_str() <= this_week.as_str())
        .max_by(|a, b| a.0.cmp(b.0))
        .and_then(|(_, id)| id.parse().ok())
}

// ── Reminders ─────────────────────────────────────────────────────────────────

/// Send a reminder of `turns` (group, shift) in `week` — short, pinging only
/// whoever is still open — and keep it to be updated as they finish
/// (`refresh_reminders`). `None` when no open turn has anyone assigned.
pub(crate) async fn send_reminder(
    ctx: &BotContext,
    room: &Room,
    note: ReminderNote,
    week: (i32, u32),
    turns: Vec<(crate::domain::GroupId, u8)>,
    txn_id: Option<String>,
) -> anyhow::Result<Option<OwnedEventId>> {
    let rendered = {
        let state = ctx.state.lock().await;
        let link = plan_permalink(ctx, &state, week);
        reminder_text(&state, note, week, &turns, link.as_deref())
    };
    let Some((text, mxids)) = rendered else {
        return Ok(None);
    };
    let mut send = room.send(mention_message(&text, &mxids, room).await);
    if let Some(txn_id) = txn_id {
        send = send.with_transaction_id(txn_id.into());
    }
    let event_id = send
        .await
        .map_err(|e| anyhow::anyhow!("send failed: {e}"))?
        .response
        .event_id;
    {
        let mut state = ctx.state.lock().await;
        let now = current_iso_week();
        let last_week = crate::state::add_weeks(now.0, now.1, -1);
        state
            .reminder_messages
            .retain(|_, r| (r.iso_year, r.iso_week) >= last_week);
        state.reminder_messages.insert(
            event_id.to_string(),
            ReminderMessage {
                iso_year: week.0,
                iso_week: week.1,
                note,
                turns,
                rendered: text,
            },
        );
        state.save(&ctx.state_path).await?;
    }
    // Like on the plan: ✅ and 🆘 to tap.
    seed_buttons(room, &event_id).await;
    Ok(Some(event_id))
}

/// A link to the plan message of `week` — `None` before it is posted.
fn plan_permalink(
    ctx: &BotContext,
    state: &crate::state::State,
    (year, week): (i32, u32),
) -> Option<String> {
    let event_id = state
        .weekly_plan_canonical
        .get(&format!("{year}-W{week:02}"))?;
    // The bot's own server surely knows the room.
    let via = ctx
        .config
        .matrix
        .user_id
        .split_once(':')
        .map(|(_, server)| server);
    Some(permalink(ctx.room_id.as_str(), event_id, via))
}

/// `https://matrix.to/#/<room>/<event>?via=<server>`: clients open it at
/// that message (Element, FluffyChat). IDs are percent-encoded as matrix.to
/// expects — older event IDs may contain `/` or `+`.
fn permalink(room_id: &str, event_id: &str, via: Option<&str>) -> String {
    let encode = |id: &str| {
        id.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-._~!$:@".contains(c) {
                    c.to_string()
                } else {
                    let mut buf = [0u8; 4];
                    c.encode_utf8(&mut buf)
                        .bytes()
                        .map(|b| format!("%{b:02X}"))
                        .collect()
                }
            })
            .collect::<String>()
    };
    let via = via
        .map(|server| format!("?via={}", encode(server)))
        .unwrap_or_default();
    format!(
        "https://matrix.to/#/{}/{}{via}",
        encode(room_id),
        encode(event_id)
    )
}

/// Bring the reminders of `week` up to date — who is done, who is still
/// open. The edits notify nobody.
async fn refresh_reminders(ctx: &BotContext, room: &Room, year: i32, week: u32) {
    let stale: Vec<(String, String)> = {
        let state = ctx.state.lock().await;
        state
            .reminder_messages
            .iter()
            .filter(|(_, r)| (r.iso_year, r.iso_week) == (year, week))
            .filter_map(|(id, r)| {
                let link = plan_permalink(ctx, &state, (year, week));
                let (text, _) =
                    reminder_text(&state, r.note, (year, week), &r.turns, link.as_deref())?;
                (text != r.rendered).then(|| (id.clone(), text))
            })
            .collect()
    };
    for (id, text) in stale {
        let Ok(event_id) = id.parse::<OwnedEventId>() else {
            continue;
        };
        let edit = crate::format::quiet(mention_message(&text, &[], room).await)
            .make_replacement(ReplacementMetadata::new(event_id, None));
        match room.send(edit).await {
            Ok(_) => {
                let mut state = ctx.state.lock().await;
                if let Some(r) = state.reminder_messages.get_mut(&id) {
                    r.rendered = text;
                }
                if let Err(e) = state.save(&ctx.state_path).await {
                    warn!("Failed to save an updated reminder: {e}");
                }
            }
            Err(e) => warn!("Failed to update a reminder: {e}"),
        }
    }
}

/// A reminder as it should read now, with the Matrix IDs to notify (those
/// still open):
///
/// ```text
/// ⏰ Still open, ends today: @bob (Bath · Thu–Fri) · Dan (Kitchen)
/// ✅ alice (2nd Floor)
/// React ✅ here or on the plan when it's done · 🆘 if you can't make it.
/// ```
///
/// "the plan" links to that week's plan message (`plan_link`), if any.
///
/// Once everyone is done: "✨ All done — thanks!" over the ✅ line. `None`
/// when none of the turns has anyone assigned.
pub(crate) fn reminder_text(
    state: &crate::state::State,
    note: ReminderNote,
    week: (i32, u32),
    turns: &[(crate::domain::GroupId, u8)],
    plan_link: Option<&str>,
) -> Option<(String, Vec<String>)> {
    let snapshot = crate::schedule::build_schedule_from(state, week, 1);
    let duties: Vec<_> = snapshot
        .assignments
        .iter()
        .filter(|a| turns.iter().any(|(g, s)| *g == a.group_id && *s == a.shift))
        .filter(|a| a.assignee.is_some() && !a.is_skipped)
        .collect();
    if duties.is_empty() {
        return None;
    }
    let what = |a: &crate::schedule::AssignmentInstance| {
        let mut parts = vec![a.group_name.clone()];
        parts.extend(a.shift_label.clone());
        parts.extend(a.slot_name.clone());
        parts.join(" · ")
    };
    let mut open = Vec::new();
    let mut done = Vec::new();
    let mut mxids = Vec::new();
    for a in duties {
        let person = a.assignee.as_ref().expect("filtered above");
        if a.is_completed {
            let link = view::user_link(&crate::domain::Person {
                id: person.id.clone(),
                display_name: person.name.clone(),
                active: true,
                matrix_id: person.mxid.clone(),
            });
            done.push(format!("{link} ({})", what(a)));
        } else {
            open.push(format!(
                "{} ({})",
                person.mxid.as_deref().unwrap_or(&person.name),
                what(a)
            ));
            mxids.extend(person.mxid.clone());
        }
    }
    let mut lines = Vec::new();
    if open.is_empty() {
        lines.push("✨ All done — thanks!".to_owned());
    } else {
        let title = match note {
            ReminderNote::StartsToday => "🔔 Your turn starts today:",
            ReminderNote::EndsToday => "⏰ Still open, ends today:",
            ReminderNote::StillOpen => "🔔 Still open this week:",
        };
        lines.push(format!("{title} {}", open.join(" · ")));
    }
    if !done.is_empty() {
        lines.push(format!("✅ {}", done.join(" · ")));
    }
    if !open.is_empty() {
        let plan = plan_link.map_or_else(|| "the plan".to_owned(), |l| format!("[the plan]({l})"));
        lines.push(format!(
            "React ✅ here or on {plan} when it's done · 🆘 if you can't make it."
        ));
    }
    mxids.sort();
    mxids.dedup();
    Some((lines.join("\n"), mxids))
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
    /// The tracked message is gone and nothing is due this week (any more)
    /// — no need to bring it back.
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

    // A plan whose groups were all disabled since is still edited, to say
    // there's nothing to clean — it mustn't keep showing old duties.
    match actual_body {
        None if !any_due => PlanReconcileAction::NothingDue,
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
    let _operation = ctx.operations.lock().await;
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

    // Cheap pure pre-check (dummy body args — only NoPlanTracked is
    // inspected here) so a week without a plan never costs a Matrix
    // round-trip.
    let stored_eid: Option<OwnedEventId> = {
        let state = ctx.state.lock().await;
        match decide_plan_reconcile_action(&state, year, week, "", None) {
            PlanReconcileAction::NoPlanTracked => {
                info!("Reconcile: no weekly plan tracked for {week_key} — nothing to check");
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
            let already_mentioned = {
                let state = ctx.state.lock().await;
                state
                    .weekly_plan_rendered
                    .get(&week_key)
                    .map(|p| previous_mentions(p))
            };
            let content = expected_content.make_replacement(ReplacementMetadata::new(
                event_id.clone(),
                already_mentioned,
            ));
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
                            seed_buttons(&room, &new_eid).await;
                            pin_weekly_plan(ctx, &room, Some(&new_eid)).await;
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
    let snapshot = crate::schedule::build_schedule_from(state, (year, week), 1);
    let all_done = due_groups.iter().all(|g| {
        state
            .turns_in_week(g, year, week)
            .into_iter()
            .all(|t| state.is_turn_done(g, t))
    });
    let mut lines = vec![
        format!("🧹 **{}**", view::week_label(year, week)),
        if due_groups.is_empty() {
            "No cleaning due this week.".to_owned()
        } else if all_done {
            "✨ All done for this week — thank you!".to_owned()
        } else {
            "React ✅ when your part is done 🫧 · 🆘 if you can't make it".to_owned()
        },
    ];
    let mut all_mxids: Vec<String> = Vec::new();
    for group in due_groups {
        lines.push(String::new());
        lines.extend(group_heading(group));
        for turn in state.turns_in_week(group, year, week) {
            for (line, mxid) in turn_lines(&snapshot, group, turn, true) {
                lines.push(line);
                all_mxids.extend(mxid);
            }
        }
    }
    all_mxids.sort();
    all_mxids.dedup();
    (lines.join("\n"), all_mxids)
}

/// A group's heading, with its rooms as an indented subtitle when they
/// belong to the whole group (a group with slots lists them per slot).
pub(crate) fn group_heading(group: &CleaningGroup) -> Vec<String> {
    let mut lines = vec![format!("**{}**", group.name)];
    if !group.is_multi_slot() && !group.room_names.is_empty() {
        lines.push(format!(
            "{}{}",
            view::INDENT,
            view::rooms(&group.room_names)
        ));
    }
    lines
}

/// One line per slot of a turn, with the assignee's MXID for mentions —
/// "⬜ @bob", "✅ Scharni: @alice", "⬜ Thu–Sun: @carol 🌴 away" — each
/// slot's rooms indented below it. `with_done` also lists finished slots.
fn turn_lines(
    snapshot: &crate::schedule::ScheduleSnapshot,
    group: &CleaningGroup,
    turn: Turn,
    with_done: bool,
) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for a in snapshot
        .assignments
        .iter()
        .filter(|a| a.group_id == group.id && a.shift == turn.shift)
    {
        if a.is_completed && !with_done {
            continue;
        }
        out.push((
            a.matrix_line(false, true),
            a.assignee.as_ref().and_then(|p| p.mxid.clone()),
        ));
        if a.slot_name.is_some() && !a.room_names.is_empty() {
            out.push((
                format!("{}{}", view::INDENT, view::rooms(&a.room_names)),
                None,
            ));
        }
    }
    out
}

/// Parse "HH:MM" → (hour, minute). Falls back to (9, 0) on bad input.
fn parse_hhmm(s: &str) -> (u8, u8) {
    let mut parts = s.splitn(2, ':');
    let h = parts.next().and_then(|p| p.parse().ok()).unwrap_or(9u8);
    let m = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0u8);
    (h.min(23), m.min(59))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CleaningGroup, Person, SlotAssignment},
        state::{Absence, State},
    };
    use matrix_sdk::ruma::events::room::message::Relation;
    use std::collections::HashMap;

    #[test]
    fn only_the_newest_plan_stays_pinned_of_the_bots_messages() {
        let id = |s: &str| OwnedEventId::try_from(s).unwrap();
        let pinned = [
            id("$plan38"),
            id("$house_rules"),
            id("$plan39"),
            id("$plan40"),
        ];
        let bots: HashSet<_> = [id("$plan38"), id("$plan39")].into();
        // People's pins stay where they were; the plan goes last (newest).
        let wanted = wanted_pins(&pinned, Some(&id("$plan40")), &bots);
        assert_eq!(wanted, [id("$house_rules"), id("$plan40")]);
        // Applying it again changes nothing — no needless state event.
        assert_eq!(wanted_pins(&wanted, Some(&id("$plan40")), &bots), wanted);
        // No plan yet: only the bot's pins go.
        assert_eq!(
            wanted_pins(&pinned, None, &bots),
            [id("$house_rules"), id("$plan40")]
        );

        // On startup: this week's plan, else the newest before it.
        let mut state = State::default();
        for (key, eid) in [
            ("2026-W38", "$plan38"),
            ("2026-W39", "$plan39"),
            ("2026-W41", "$plan41"),
        ] {
            state.weekly_plan_canonical.insert(key.into(), eid.into());
        }
        assert_eq!(newest_plan(&state, (2026, 40)), Some(id("$plan39")));
        assert_eq!(newest_plan(&state, (2026, 41)), Some(id("$plan41")));
        assert_eq!(newest_plan(&state, (2026, 37)), None);
    }

    #[test]
    fn a_reminder_is_short_pings_only_whos_open_and_shows_whos_done() {
        let alice = Person::new_matrix("@alice:example.org");
        let bob = Person::new_matrix("@bob:example.org");
        let dan = Person::new_named("Dan");
        let mut floor = CleaningGroup::new("Floor");
        floor.slots = vec![
            crate::domain::CleaningSlot::new("Scharni"),
            crate::domain::CleaningSlot::new("Colbe"),
        ];
        let kitchen = CleaningGroup::new("Kitchen");
        let mut hall = CleaningGroup::new("Hall");
        hall.member_ids = vec![dan.id.clone()];
        let (year, week) = (2024, 10);
        let mut state = State::default();
        for (group, slot, who) in [
            (&floor, 0, Some(&alice.id)),
            (&floor, 1, Some(&bob.id)),
            (&kitchen, 0, Some(&dan.id)),
            (&hall, 0, None),
        ] {
            state.slot_assignments.push(SlotAssignment {
                group_id: group.id.clone(),
                slot_index: slot,
                iso_year: year,
                iso_week: week,
                shift: 0,
                person_id: who.cloned(),
                source: Default::default(),
            });
        }
        let turns: Vec<_> = [&floor, &kitchen, &hall]
            .iter()
            .map(|g| (g.id.clone(), 0))
            .collect();
        let (floor_id, alice_id) = (floor.id.clone(), alice.id.clone());
        state.persons = vec![alice, bob, dan];
        state.cleaning_groups = vec![floor, kitchen, hall.clone()];

        let (text, mxids) =
            reminder_text(&state, ReminderNote::EndsToday, (year, week), &turns, None).unwrap();
        assert_eq!(
            text,
            "⏰ Still open, ends today: @alice:example.org (Floor · Scharni) · \
             @bob:example.org (Floor · Colbe) · Dan (Kitchen)\n\
             React ✅ here or on the plan when it's done · 🆘 if you can't make it."
        );
        assert_eq!(mxids, ["@alice:example.org", "@bob:example.org"]);

        // With the plan posted, "the plan" links to it.
        let link = permalink("!room:example.org", "$ab/c+d", Some("example.org"));
        assert_eq!(
            link,
            "https://matrix.to/#/!room:example.org/$ab%2Fc%2Bd?via=example.org"
        );
        let (text, _) = reminder_text(
            &state,
            ReminderNote::EndsToday,
            (year, week),
            &turns,
            Some(&link),
        )
        .unwrap();
        assert!(
            text.ends_with(&format!(
                "React ✅ here or on [the plan]({link}) when it's done · 🆘 if you can't make it."
            )),
            "{text}"
        );
        let content = crate::format::intentional(crate::format::mentionify(&text));
        let html = &content.msgtype;
        let html = match html {
            matrix_sdk::ruma::events::room::message::MessageType::Text(t) => {
                t.formatted.as_ref().unwrap().body.clone()
            }
            _ => unreachable!(),
        };
        assert!(
            html.contains(&format!(r#"<a href="{link}">the plan</a>"#)),
            "{html}"
        );
        // The link pings nobody.
        assert_eq!(
            content.mentions.unwrap().user_ids.len(),
            2,
            "only Alice and Bob, who are open"
        );

        // Alice is done: she moves to the ✅ line, as a pill that pings nobody.
        state
            .apply_event(crate::analytics::DomainEvent::CleaningCompleted {
                group_id: floor_id.clone(),
                slot_id: Some(state.cleaning_groups[0].slots[0].id.clone()),
                person_id: alice_id,
                responsible_person_ids: vec![],
                iso_year: year,
                iso_week: week,
                shift: 0,
            })
            .unwrap();
        let (text, mxids) =
            reminder_text(&state, ReminderNote::EndsToday, (year, week), &turns, None).unwrap();
        assert!(
            text.contains(
                "\n✅ [alice](https://matrix.to/#/@alice:example.org) (Floor · Scharni)\n"
            ),
            "{text}"
        );
        assert_eq!(mxids, ["@bob:example.org"]);

        // Everyone done (or excused): it says so, and asks nothing more.
        state.completions.clear();
        let all_done: Vec<_> = state
            .cleaning_groups
            .iter()
            .map(|g| {
                (
                    g.id.clone(),
                    g.slots.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
                )
            })
            .collect();
        for (group_id, slots) in all_done {
            let slots = if slots.is_empty() {
                vec![None]
            } else {
                slots.into_iter().map(Some).collect()
            };
            for slot_id in slots {
                state
                    .apply_event(crate::analytics::DomainEvent::CleaningCompleted {
                        group_id: group_id.clone(),
                        slot_id,
                        person_id: state.persons[2].id.clone(),
                        responsible_person_ids: vec![],
                        iso_year: year,
                        iso_week: week,
                        shift: 0,
                    })
                    .unwrap();
            }
        }
        let (text, mxids) =
            reminder_text(&state, ReminderNote::EndsToday, (year, week), &turns, None).unwrap();
        assert!(text.starts_with("✨ All done — thanks!\n✅ "), "{text}");
        assert!(!text.contains("React"), "{text}");
        assert!(mxids.is_empty());

        // Only unassigned turns: nobody to remind.
        assert!(reminder_text(
            &state,
            ReminderNote::StillOpen,
            (year, week),
            &[(hall.id, 0)],
            None
        )
        .is_none());
    }

    #[test]
    fn a_missed_weekly_plan_is_caught_up_once_also_after_a_restart() {
        let mut state = State::default();
        state.cleaning_groups.push(CleaningGroup::new("Hall"));
        let (year, week) = (2026, 40);
        // Plan day Wednesday: not before, but any later day of the week.
        assert!(!weekly_plan_due(&state, year, week, 1, 2));
        assert!(weekly_plan_due(&state, year, week, 2, 2));
        assert!(weekly_plan_due(&state, year, week, 5, 2));
        // Posted (on Saturday, after downtime): never again that week.
        state
            .weekly_plan_canonical
            .insert(format!("{year}-W{week:02}"), "$plan".into());
        let restored: State =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert!(!weekly_plan_due(&restored, year, week, 6, 2));
        assert!(weekly_plan_due(&restored, year, week + 1, 2, 2));
    }

    #[test]
    fn a_plan_with_nothing_due_says_so() {
        let state = State::default();
        let (plan, mxids) = build_weekly_plan(&state, 2026, 40, &[]);
        assert!(plan.ends_with("\nNo cleaning due this week."), "{plan}");
        assert!(mxids.is_empty());
    }

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

    #[test]
    fn a_plan_edit_only_notifies_people_new_on_the_plan() {
        // Dave was on the plan; Erin just took over a turn. The edit keeps
        // both pills, but only Erin gets a notification.
        let before = "🧹 Week 10\n⬜ @dave:example.org";
        let after = "🧹 Week 10\n✅ @dave:example.org\n⬜ @erin:example.org";
        let edit = crate::format::mentionify(after).make_replacement(ReplacementMetadata::new(
            plan1_event_id(),
            Some(previous_mentions(before)),
        ));
        let notified = edit.mentions.expect("the edit carries mentions");
        assert_eq!(
            notified.user_ids.into_iter().collect::<Vec<_>>(),
            vec![uid("@erin:example.org")]
        );
        let Some(Relation::Replacement(replacement)) = edit.relates_to else {
            panic!("an edit");
        };
        let shown = replacement
            .new_content
            .mentions
            .expect("new content mentions");
        assert!(shown.user_ids.contains(&uid("@dave:example.org")));
    }

    #[test]
    fn weekly_plan_reads_well_on_a_phone() {
        // Rooms are an indented subtitle — of the group, or of each slot —
        // instead of a long tail on every line; done lines keep their slot.
        let alice = Person::new_matrix("@alice:example.org");
        let bob = Person::new_named("Bob");
        let mut hall = CleaningGroup::new("Hall");
        hall.room_names = vec!["Stairs".into(), "Entrance".into()];
        hall.member_ids = vec![bob.id.clone()];
        let mut floor = CleaningGroup::new("Floor");
        let mut scharni = crate::domain::CleaningSlot::new("Scharni");
        scharni.room_names = vec!["Toilet".into(), "Shower".into()];
        floor.slots = vec![scharni, crate::domain::CleaningSlot::new("Colbe")];
        floor.member_ids = vec![alice.id.clone(), bob.id.clone()];
        let mut state = State::default();
        let (year, week) = (2024, 10);
        for (group, slot, who) in [
            (&hall, 0, &bob.id),
            (&floor, 0, &alice.id),
            (&floor, 1, &bob.id),
        ] {
            state.slot_assignments.push(SlotAssignment {
                group_id: group.id.clone(),
                slot_index: slot,
                iso_year: year,
                iso_week: week,
                shift: 0,
                person_id: Some(who.clone()),
                source: Default::default(),
            });
        }
        state.completions.push(crate::state::Completion {
            group_id: floor.id.clone(),
            slot_id: Some(floor.slots[0].id.clone()),
            completed_by_id: alice.id.clone(),
            responsible_person_ids: vec![],
            iso_year: year,
            iso_week: week,
            shift: 0,
            completed_at: chrono::Utc::now(),
            skipped: false,
        });
        state.persons = vec![alice, bob];
        state.cleaning_groups = vec![hall, floor];
        let (plan, mxids) = build_weekly_plan(&state, year, week, &state.cleaning_groups);
        let i = view::INDENT;
        assert_eq!(
            plan,
            format!(
                "🧹 **{}**\nReact ✅ when your part is done 🫧 · 🆘 if you can't make it\n\n\
                 **Hall**\n{i}🧽 Stairs, Entrance\n❌ Bob · missed\n\n\
                 **Floor**\n✅ Scharni: @alice:example.org · {}\n{i}🚽 Toilet · 🚿 Shower\n❌ Colbe: Bob · missed",
                view::week_label(year, week), crate::state::local_time(state.completions[0].completed_at).format("%a %-d %b")
            )
        );
        assert_eq!(mxids, vec!["@alice:example.org".to_owned()]);

        // Once every part is done (or excused), the plan says thanks
        // instead of asking for reactions.
        let hall_id = state.cleaning_groups[0].id.clone();
        let floor_id = state.cleaning_groups[1].id.clone();
        let colbe = state.cleaning_groups[1].slots[1].id.clone();
        let bob_id = state.persons[1].id.clone();
        for (group_id, slot_id, skipped) in [(hall_id, None, true), (floor_id, Some(colbe), false)]
        {
            state.completions.push(crate::state::Completion {
                group_id,
                slot_id,
                completed_by_id: bob_id.clone(),
                responsible_person_ids: vec![],
                iso_year: year,
                iso_week: week,
                shift: 0,
                completed_at: chrono::Utc::now(),
                skipped,
            });
        }
        let (plan, _) = build_weekly_plan(&state, year, week, &state.cleaning_groups);
        assert_eq!(
            plan.lines().nth(1),
            Some("✨ All done for this week — thank you!"),
            "{plan}"
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
        assert!(open_text.contains('❌'), "{open_text}");

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
            expected_now.contains("✅ @bob:example.org"),
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
