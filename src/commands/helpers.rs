//! Shared helpers for the command handlers (lookups, group joins, formatting).

use super::*;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// True for every command whose successful effect can change who's
/// responsible for, or the completion status of, the currently displayed
/// weekly plan — `handle` re-renders the pinned message from `State` after
/// any of these (see the call site below) instead of leaving it to drift
/// until the next scheduler tick or restart. Pulled out as its own function
/// (rather than an inline `matches!` at the call site) so this list — which
/// `!importplan` was added to alongside the other admin overrides — is
/// covered by a direct unit test instead of only being verified by reading
/// the dispatcher.
pub(crate) fn command_may_change_current_plan(cmd: &str) -> bool {
    matches!(
        cmd,
        "!done"
            | "!skip"
            | "!undo"
            | "!assign"
            | "!unassign"
            | "!takeover"
            | "!acceptswap"
            | "!importplan"
    )
}

pub(crate) fn require_admin(ctx: &BotContext, sender: &OwnedUserId) -> Result<()> {
    Ok(mxbot_common::admin::require_admin(
        &ctx.admin_users,
        sender,
    )?)
}

/// Returns the MXID or display_name depending on whether the person has Matrix.
pub(crate) fn person_key(p: &Person) -> &str {
    p.matrix_id.as_deref().unwrap_or(&p.display_name)
}

/// Format a CLI deviation as a percentage and a human-readable label.
///
/// Thresholds:  > +10% → overloaded  |  < -10% → under-contributing  |  else → balanced
pub(crate) fn load_icon(actual: f64, expected: f64) -> &'static str {
    let (_, label) = load_delta_pct(actual, expected);
    if label == "under-contributing" {
        "🔴"
    } else if label == "overloaded" {
        "🟠"
    } else {
        "🟢"
    }
}

pub(crate) fn load_delta_pct(actual: f64, expected: f64) -> (String, &'static str) {
    if expected < 0.01 {
        // No expected history yet — can't compute a meaningful percentage.
        return ("n/a".to_owned(), "balanced");
    }
    let pct = (actual - expected) / expected * 100.0;
    let pct_str = if pct.abs() < 0.05 {
        "±0.0%".to_owned()
    } else if pct > 0.0 {
        format!("+{pct:.1}%")
    } else {
        format!("{pct:.1}%")
    };
    let label = if pct > 10.0 {
        "overloaded"
    } else if pct < -10.0 {
        "under-contributing"
    } else {
        "balanced"
    };
    (pct_str, label)
}

/// Fill not-yet-frozen future weeks for every group using the current
/// rotation queue, up to `weeks_ahead` due-cycles. Idempotent and purely
/// additive — `resolver::materialize` never revisits or changes a week it
/// already froze, so this is safe to call at any time without disturbing
/// anyone's existing plan.
pub(crate) fn materialize_and_apply(
    ctx: &BotContext,
    state: &mut crate::state::State,
    weeks_ahead: usize,
) -> anyhow::Result<()> {
    let interval = ctx.config.schedule.interval_weeks;
    for ev in resolver::materialize(state, interval, weeks_ahead) {
        state.apply_event(ev)?;
    }
    Ok(())
}

/// How many due-cycles ahead of `first_due_week` a group is *already*
/// materialized (i.e. has a frozen `SlotAssignment` for), based on its
/// furthest currently-stored assignment. 0 means nothing is frozen yet.
pub(crate) fn group_horizon_weeks_ahead(
    state: &crate::state::State,
    group_id: &GroupId,
    interval: u32,
) -> usize {
    let first_due = crate::state::first_due_week(state, interval);
    let max_offset = state
        .slot_assignments
        .iter()
        .filter(|a| a.group_id == *group_id)
        .filter_map(|a| {
            let d = crate::state::weeks_between(first_due, (a.iso_year, a.iso_week));
            (d >= 0).then_some(d as usize)
        })
        .max();
    match max_offset {
        Some(d) => d / (interval.max(1) as usize) + 1,
        None => 0,
    }
}

/// Admin escape hatch (`!resetplan`): explicitly wipe and redistribute a
/// group's future schedule, unlike a join/leave which never touches
/// already-frozen weeks. Clears future `slot_assignments`, resets the
/// rotation queue to plain `member_ids` order, then refills.
pub(crate) fn reset_and_rematerialize(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &str,
) -> anyhow::Result<usize> {
    let (cur_y, cur_w) = current_iso_week();
    let before = state.slot_assignments.len();
    // Drop assignments after the active week; they'll be rebuilt with the full list.
    state.slot_assignments.retain(|a| {
        a.group_id != group_id || a.iso_year < cur_y || (a.iso_year == cur_y && a.iso_week <= cur_w)
    });
    let cleared = before - state.slot_assignments.len();

    if let Some(group) = state.group_by_id(&group_id.to_owned()).cloned() {
        state.apply_event(DomainEvent::RotationQueueSet {
            group_id: group_id.to_owned(),
            queue: group.member_ids.clone(),
        })?;
    }
    // Explicit admin escape hatch: commit the full configured horizon, not
    // just one more due-cycle — unlike a join/leave, this is a deliberate
    // "redistribute everything now" action.
    let weeks = ctx.config.schedule.materialize_weeks as usize;
    materialize_and_apply(ctx, state, weeks)?;
    Ok(cleared)
}

/// Insert `person_id` into `group_id`'s rotation queue and fill in any
/// newly-reachable future weeks.
///
/// Must be called with `state` already locked and `person_id` already a
/// registered `Person`, BEFORE `PersonJoinedGroup` is applied (this function
/// applies it). Never touches an already-frozen week: the active week is
/// explicitly frozen first (using the *pre-join* queue) so nobody's current
/// task changes because someone else just joined, and everything already
/// materialized beyond that stays exactly as it was.
///
/// Insertion point: right after the last currently-queued member who
/// hasn't had a single turn yet (any `SlotAssignment` in this group), and
/// right before the first one who has. A newcomer must never skip someone
/// who is still waiting for their *first* turn — but if everyone already
/// queued has had at least one, the newcomer belongs at the very front,
/// ahead of anyone about to start a repeat lap. When nobody queued has had
/// a turn yet (a brand-new group being set up), this is equivalent to
/// appending at the back, so founding members keep their natural join order.
pub(crate) fn apply_group_join(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &GroupId,
    person_id: &PersonId,
) -> anyhow::Result<()> {
    let rotation_was_empty = state
        .group_by_id(group_id)
        .is_some_and(|g| g.member_ids.is_empty());

    // Freeze the active week (if not already frozen) using the OLD queue,
    // before the newcomer can possibly be picked for a week already underway.
    freeze_schedule_before_join(ctx, state, group_id)?;

    if let Some(group) = state.group_by_id(group_id).cloned() {
        let mut queue = resolver::reconcile_queue(state, &group);
        queue.retain(|id| id != person_id);
        let has_had_a_turn = |pid: &PersonId| {
            state
                .slot_assignments
                .iter()
                .any(|a| a.group_id == *group_id && a.person_id.as_deref() == Some(pid.as_str()))
        };
        let insert_at = queue
            .iter()
            .rposition(|pid| !has_had_a_turn(pid))
            .map_or(0, |i| i + 1);
        queue.insert(insert_at, person_id.clone());
        state.apply_event(DomainEvent::RotationQueueSet {
            group_id: group_id.clone(),
            queue,
        })?;
    }

    state.apply_event(DomainEvent::PersonJoinedGroup {
        person_id: person_id.clone(),
        group_id: group_id.clone(),
    })?;

    if rotation_was_empty {
        // First-ever member: don't let materialize hand them a week that's
        // already underway — their first real turn starts next week.
        keep_active_week_unassigned_for_first_member(ctx, state, group_id)?;
    }

    // Reveal exactly one more due-cycle than is already frozen (capped at
    // the configured horizon) — enough that the joiner's own turn becomes
    // visible soon, without a single early member's join greedily claiming
    // the *entire* configured horizon before anyone else has a chance to
    // join. (Deeper horizons still get filled by bot startup or !resetplan.)
    let interval = ctx.config.schedule.interval_weeks;
    let materialize_weeks = ctx.config.schedule.materialize_weeks as usize;
    let horizon = group_horizon_weeks_ahead(state, group_id, interval);
    let weeks_ahead = (horizon + 1).min(materialize_weeks.max(horizon));
    materialize_and_apply(ctx, state, weeks_ahead)
}

/// Drop `person_id` from `group_id`'s rotation queue, clear their future
/// (not-yet-past, not-current) frozen assignments, and refill the resulting
/// gaps from the remaining queue. Other members' already-frozen weeks are
/// never touched.
///
/// Must be called with `state` already locked, AFTER `PersonLeftGroup` is
/// applied (so `reconcile_queue` naturally drops the leaver). Returns the
/// number of future assignments that were cleared and refilled.
pub(crate) fn apply_group_departure(
    ctx: &BotContext,
    state: &mut crate::state::State,
    person_id: &PersonId,
    group_id: &GroupId,
) -> anyhow::Result<usize> {
    let removed = remove_future_assignments_for_person(state, person_id, group_id);

    if let Some(group) = state.group_by_id(group_id).cloned() {
        let queue = resolver::reconcile_queue(state, &group);
        state.apply_event(DomainEvent::RotationQueueSet {
            group_id: group_id.clone(),
            queue,
        })?;
    }

    // Refill within whatever horizon this group already had — a leave
    // creates gaps, it never needs to extend the horizon further out.
    let interval = ctx.config.schedule.interval_weeks;
    let horizon = group_horizon_weeks_ahead(state, group_id, interval);
    materialize_and_apply(ctx, state, horizon)?;
    Ok(removed)
}

pub(crate) fn remove_future_assignments_for_person(
    state: &mut crate::state::State,
    person_id: &str,
    group_id: &str,
) -> usize {
    let (cur_y, cur_w) = current_iso_week();
    let before = state.slot_assignments.len();
    state.slot_assignments.retain(|a| {
        a.group_id != group_id
            || a.person_id.as_deref() != Some(person_id)
            || a.iso_year < cur_y
            || (a.iso_year == cur_y && a.iso_week <= cur_w)
    });
    before - state.slot_assignments.len()
}

/// Extract an optional trailing `week <1-53>` clause from `args`.
///
/// Returns the remaining args (with the clause removed) and the resolved
/// `(year, week)` — the current week when no clause is present, rolling into
/// next year when the requested week number has already passed this year.
/// `None` means a `week` keyword was present but not followed by a valid
/// 1-53 number; the caller should show its own usage message in that case.
pub(crate) fn extract_week_arg<'a>(args: &'a [&'a str]) -> Option<(&'a [&'a str], (i32, u32))> {
    let (cur_y, cur_w) = current_iso_week();
    match args.iter().position(|a| a.eq_ignore_ascii_case("week")) {
        Some(pos) => {
            let n: u32 = args
                .get(pos + 1)
                .and_then(|s| s.parse().ok())
                .filter(|n| (1..=53).contains(n))?;
            let y = if n < cur_w { cur_y + 1 } else { cur_y };
            Some((&args[..pos], (y, n)))
        }
        None => Some((args, (cur_y, cur_w))),
    }
}

/// Resolve `<group> [<slot>]` against `state`, matching the convention used
/// by `!addroom`/`!removeroom`: the token right after the group name is only
/// treated as a slot name when the group is multi-slot and it actually
/// matches one of its slots.  Returns the group id, the slot index to use in
/// a `SlotAssignment` (0 for single-slot groups), and the remaining args.
pub(crate) fn resolve_group_and_slot<'a>(
    state: &crate::state::State,
    group_name: &str,
    rest: &'a [&'a str],
) -> std::result::Result<(GroupId, usize, &'a [&'a str]), String> {
    let group = state
        .group_by_name(group_name)
        .ok_or_else(|| format!("Group «{group_name}» not found."))?;
    if group.is_multi_slot() {
        match rest.first().and_then(|s| {
            group
                .slots
                .iter()
                .position(|slot| slot.name.eq_ignore_ascii_case(s))
        }) {
            Some(idx) => Ok((group.id.clone(), idx, &rest[1..])),
            None => Err(format!(
                "«{group_name}» has slots. Specify one: {}",
                group
                    .slots
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    } else {
        Ok((group.id.clone(), 0, rest))
    }
}

/// Resolve what a `!takeover` invocation refers to, applying self-service
/// defaults instead of requiring the full explicit `<group> <slot>` syntax:
///
/// - No group given → the sender's own group, provided they're in exactly
///   one (never guessed among several — that returns a message listing them).
/// - No slot given → auto-picked when the target group/week has exactly one
///   takeable slot (not already completed/skipped, not already the sender's);
///   with zero or several candidates, returns a message instead of guessing.
/// - `!takeover <slot>` (a single token that isn't a known group name) is
///   read as a slot name within the sender's own single group.
///
/// The full explicit syntax (`!takeover <group> [<slot>] [week <N>]`) keeps
/// working unchanged — it's just the first two branches below, same as
/// `resolve_group_and_slot`.
pub(crate) fn resolve_takeover_target(
    state: &crate::state::State,
    sender_person_id: &PersonId,
    rest: &[&str],
    year: i32,
    week: u32,
    interval: u32,
) -> std::result::Result<(GroupId, usize), String> {
    fn own_group(
        state: &crate::state::State,
        sender_person_id: &PersonId,
    ) -> std::result::Result<CleaningGroup, String> {
        match state.groups_for_person(sender_person_id).as_slice() {
            [] => Err("You are not in any cleaning group. Specify one: !takeover <group> [<slot>]".into()),
            [g] => Ok((*g).clone()),
            many => Err(format!(
                "You are in multiple groups — specify one: {}\nUsage: !takeover <group> [<slot>] [week <N>]",
                many.iter().map(|g| g.name.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    let (group, slot_token): (CleaningGroup, Option<&str>) = match rest.first() {
        Some(&first) if state.group_by_name(first).is_some() => (
            state.group_by_name(first).unwrap().clone(),
            rest.get(1).copied(),
        ),
        Some(&first) => (own_group(state, sender_person_id)?, Some(first)),
        None => (own_group(state, sender_person_id)?, None),
    };

    let slot_index = match slot_token {
        Some(name) => {
            if !group.is_multi_slot() {
                return Err(format!("«{}» does not have slots.", group.name));
            }
            match group
                .slots
                .iter()
                .position(|s| s.name.eq_ignore_ascii_case(name))
            {
                Some(idx) => idx,
                None => {
                    return Err(format!(
                        "«{name}» is not a group or a slot of «{}». Slots: {}",
                        group.name,
                        group
                            .slots
                            .iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
        }
        None => auto_pick_takeover_slot(state, &group, sender_person_id, year, week, interval)?,
    };
    Ok((group.id.clone(), slot_index))
}

/// Auto-pick the slot for a group/week when `!takeover` wasn't given one
/// explicitly: 0 for a single-slot group, or the sole takeable slot of a
/// multi-slot group. Never guesses between several — returns the choices
/// (or "nothing to take over") as a message instead.
pub(crate) fn auto_pick_takeover_slot(
    state: &crate::state::State,
    group: &CleaningGroup,
    sender_person_id: &PersonId,
    year: i32,
    week: u32,
    interval: u32,
) -> std::result::Result<usize, String> {
    if !group.is_multi_slot() {
        return Ok(0);
    }
    let candidates: Vec<usize> = group
        .slots
        .iter()
        .enumerate()
        .filter(|(i, slot)| {
            !state.is_slot_completed(&group.id, &slot.id, year, week)
                && state
                    .slot_assignee(group, *i, year, week, interval)
                    .is_none_or(|p| &p.id != sender_person_id)
        })
        .map(|(i, _)| i)
        .collect();
    match candidates.as_slice() {
        [] => Err(format!(
            "Nothing to take over in «{}» this week — every slot is already done, skipped, or already yours.",
            group.name
        )),
        [i] => Ok(*i),
        many => Err(format!(
            "«{}» has multiple open slots this week: {}\nSpecify one: !takeover {} <slot>",
            group.name,
            many.iter().map(|&i| group.slots[i].name.as_str()).collect::<Vec<_>>().join(", "),
            group.name,
        )),
    }
}

pub(crate) fn validate_matrix_user_id(mxid: &str) -> std::result::Result<(), String> {
    OwnedUserId::try_from(mxid)
        .map(|_| ())
        .map_err(|_| format!("«{mxid}» is not a valid Matrix user ID. Use @user:server."))
}

pub(crate) fn freeze_schedule_before_join(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &GroupId,
) -> anyhow::Result<()> {
    let (cur_y, cur_w) = current_iso_week();
    let interval = ctx.config.schedule.interval_weeks;
    for event in resolver::materialize(state, interval, 1) {
        // Apply this group's active-week pick (so it's locked in before the
        // join can affect it) AND the queue advancement that produced it —
        // otherwise whoever got picked would stay stuck at the queue's front
        // and get reused for the very next week too.
        let applies = match &event {
            DomainEvent::SlotAssigned {
                group_id: g,
                iso_year,
                iso_week,
                ..
            } => g == group_id && *iso_year == cur_y && *iso_week == cur_w,
            DomainEvent::RotationQueueSet { group_id: g, .. } => g == group_id,
            _ => false,
        };
        if applies {
            state.apply_event(event)?;
        }
    }
    Ok(())
}

/// Freezes the active week as explicitly unassigned for a group that just
/// got its first member, so their real first turn starts next week instead
/// of a week already underway. Deliberately does NOT apply the
/// `RotationQueueSet` that the underlying preview-materialize would have
/// produced (it would have advanced the queue past this person) — the queue
/// must stay exactly as `apply_group_join` left it (them at the front) so
/// they're picked again, for real, next week.
pub(crate) fn keep_active_week_unassigned_for_first_member(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &GroupId,
) -> anyhow::Result<()> {
    let (cur_y, cur_w) = current_iso_week();
    let interval = ctx.config.schedule.interval_weeks;
    let active_placeholders: Vec<DomainEvent> = resolver::materialize(state, interval, 1)
        .into_iter()
        .filter_map(|event| match event {
            DomainEvent::SlotAssigned {
                group_id: event_group_id,
                slot_index,
                iso_year,
                iso_week,
                source,
                ..
            } if event_group_id == *group_id && iso_year == cur_y && iso_week == cur_w => {
                Some(DomainEvent::SlotAssigned {
                    group_id: event_group_id,
                    slot_index,
                    iso_year,
                    iso_week,
                    person_id: None,
                    source,
                    actor_id: None,
                    previous_person_id: None,
                })
            }
            _ => None,
        })
        .collect();
    for event in active_placeholders {
        state.apply_event(event)?;
    }
    Ok(())
}

pub(crate) fn current_open_assignments(
    state: &crate::state::State,
    group_id: &GroupId,
    person_id: &PersonId,
    interval: u32,
) -> Vec<String> {
    let (year, week) = current_iso_week();
    let Some(group) = state.group_by_id(group_id) else {
        return Vec::new();
    };
    let has_frozen_current = state.slot_assignments.iter().any(|assignment| {
        assignment.group_id == *group_id
            && assignment.iso_year == year
            && assignment.iso_week == week
    });
    if !has_frozen_current && !state.is_due(group_id, year, week, interval) {
        return Vec::new();
    }

    if group.is_multi_slot() {
        group
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| !state.is_slot_completed(group_id, &slot.id, year, week))
            .filter_map(|(index, slot)| {
                state
                    .slot_assignee(group, index, year, week, interval)
                    .filter(|person| &person.id == person_id)
                    .map(|_| slot.name.clone())
            })
            .collect()
    } else if !state.is_completed(group_id, year, week)
        && state
            .responsible_person(group, year, week, interval)
            .is_some_and(|person| &person.id == person_id)
    {
        vec![group.name.clone()]
    } else {
        Vec::new()
    }
}

pub(crate) fn person_label(person: &Person) -> String {
    match person.matrix_id.as_deref() {
        Some(mxid) if person.display_name != mxid => format!("{} ({mxid})", person.display_name),
        Some(mxid) => mxid.to_owned(),
        None => format!("{} (no Matrix)", person.display_name),
    }
}

pub(crate) fn next_assignment_summary(state: &crate::state::State, group_id: &GroupId) -> String {
    let (cur_y, cur_w) = current_iso_week();
    let Some(group) = state.group_by_id(group_id) else {
        return "Next: unavailable.".to_owned();
    };
    if group.member_ids.is_empty() {
        return "Next: rotation is empty.".to_owned();
    }

    let next_week = state
        .slot_assignments
        .iter()
        .filter(|a| {
            a.group_id == *group_id
                && (a.iso_year > cur_y || (a.iso_year == cur_y && a.iso_week > cur_w))
        })
        .map(|a| (a.iso_year, a.iso_week))
        .min();

    let Some((year, week)) = next_week else {
        return "Next: no future assignment is materialized.".to_owned();
    };
    let mut names: Vec<String> = state
        .slot_assignments
        .iter()
        .filter(|a| a.group_id == *group_id && a.iso_year == year && a.iso_week == week)
        .filter_map(|a| a.person_id.as_ref())
        .filter_map(|id| state.person_by_id(id))
        .map(person_label)
        .collect();
    names.dedup();
    let who = if names.is_empty() {
        "unassigned".to_owned()
    } else {
        names.join(", ")
    };
    let dates = week_dates(year, week);
    format!("Next: {who} · week {week} ({dates}).")
}

/// Fetch Matrix display names for all known Matrix users and update state.
/// Keeps Person.display_name in sync with the real Matrix profile name.
/// Called before PDF generation and whenever a command comes in.
pub(crate) async fn refresh_display_names(ctx: &BotContext, room: &Room) {
    let mxids: Vec<String> = ctx
        .state
        .lock()
        .await
        .persons
        .iter()
        .filter_map(|p| p.matrix_id.clone())
        .collect();
    if mxids.is_empty() {
        return;
    }
    let refs: Vec<&str> = mxids.iter().map(String::as_str).collect();
    let fetched = format::fetch_names(room, &refs).await;
    if fetched.is_empty() {
        return;
    }
    let mut state = ctx.state.lock().await;
    for p in &mut state.persons {
        if let Some(mxid) = &p.matrix_id {
            if let Some(name) = fetched.get(mxid.as_str()) {
                if !name.is_empty() && name != mxid {
                    p.display_name = name.clone();
                }
            }
        }
    }
}
