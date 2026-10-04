//! Shared helpers for the command handlers (lookups, group joins, formatting).

use super::*;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// True for every command (and lower-cased first argument, for commands
/// with subcommands) whose successful effect can change who's responsible
/// for, or the completion status of, the currently displayed weekly plan —
/// `handle` re-renders the pinned message from `State` after any of these
/// instead of leaving it to drift until the next scheduler tick or restart.
/// Kept as its own function so the list is covered by a direct unit test.
pub(crate) fn command_may_change_current_plan(cmd: &str, sub: Option<&str>) -> bool {
    match cmd {
        "!done" | "!undo" | "!takeover" | "!acceptswap" => true,
        "!swap" => sub == Some("accept"),
        "!plan" => matches!(
            sub,
            Some("assign" | "unassign" | "skip" | "reset" | "import")
        ),
        // Which groups/slots are shown, and who is marked away.
        "!groups" => matches!(
            sub,
            Some("enable" | "disable" | "remove" | "slot" | "rhythm")
        ),
        "!member" => matches!(sub, Some("add" | "remove" | "away" | "back")),
        "!join" | "!leave" => true,
        _ => false,
    }
}

pub(crate) fn require_admin(ctx: &BotContext, sender: &OwnedUserId) -> Result<()> {
    Ok(mxbot_common::admin::require_admin(
        &ctx.admin_users,
        sender,
    )?)
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

/// Fill not-yet-frozen upcoming turns of one group from its rotation queue,
/// up to `cycles_ahead` of its due weeks. Idempotent and purely additive.
pub(crate) fn materialize_group_and_apply(
    state: &mut crate::state::State,
    group_id: &GroupId,
    cycles_ahead: usize,
) -> anyhow::Result<()> {
    let Some(group) = state.group_by_id(group_id).cloned() else {
        return Ok(());
    };
    for ev in resolver::materialize_group(state, &group, cycles_ahead) {
        state.apply_event(ev)?;
    }
    Ok(())
}

/// How many of its due weeks (from the current one) a group is *already*
/// materialized for, based on its furthest stored assignment. 0 means
/// nothing is frozen yet.
pub(crate) fn group_horizon_weeks_ahead(state: &crate::state::State, group_id: &GroupId) -> usize {
    let Some(group) = state.group_by_id(group_id) else {
        return 0;
    };
    let first_due = state.next_due_week(group, current_iso_week());
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
        Some(d) => d / (group.rhythm.every_weeks() as usize) + 1,
        None => 0,
    }
}

/// Admin escape hatch (`!plan reset`): explicitly wipe and redistribute a
/// group's future schedule, unlike a join/leave which never touches
/// already-frozen weeks. Clears future `slot_assignments`, resets the
/// rotation queue to plain `member_ids` order, then refills.
pub(crate) fn reset_and_rematerialize(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &str,
) -> anyhow::Result<Vec<crate::domain::SlotAssignment>> {
    let cleared = drop_future_assignments(state, group_id);
    if let Some(group) = state.group_by_id(&group_id.to_owned()).cloned() {
        state.apply_event(DomainEvent::RotationQueueSet {
            group_id: group_id.to_owned(),
            queue: group.member_ids.clone(),
        })?;
    }
    // Explicit admin escape hatch: commit the full configured horizon, not
    // just one more due-cycle — unlike a join/leave, this is a deliberate
    // "redistribute everything now" action.
    let cycles = ctx.config.schedule.materialize_weeks as usize;
    materialize_group_and_apply(state, &group_id.to_owned(), cycles)?;
    Ok(cleared)
}

/// "Discarded 3 pinned week(s): 41 (imported), 42 (imported), 50 (assigned)."
/// — or nothing when only plain rotation weeks were cleared.
pub(crate) fn discarded_pins_note(cleared: &[crate::domain::SlotAssignment]) -> Option<String> {
    let mut pinned: Vec<&crate::domain::SlotAssignment> = cleared
        .iter()
        .filter(|a| a.person_id.is_some() && a.source != AssignmentSource::RoundRobin)
        .collect();
    if pinned.is_empty() {
        return None;
    }
    pinned.sort_by_key(|a| (a.iso_year, a.iso_week, a.shift, a.slot_index));
    let weeks: Vec<String> = pinned
        .iter()
        .map(|a| {
            format!(
                "{} ({})",
                a.iso_week,
                crate::view::source_note(&a.source).unwrap_or("planned")
            )
        })
        .collect();
    Some(format!(
        "⚠️ Discarded {} pinned week(s), now plain rotation: {}.",
        pinned.len(),
        weeks.join(", ")
    ))
}

/// Remove and return the group's assignments after the current week.
pub(crate) fn drop_future_assignments(
    state: &mut crate::state::State,
    group_id: &str,
) -> Vec<crate::domain::SlotAssignment> {
    let current = current_iso_week();
    let (future, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut state.slot_assignments)
        .into_iter()
        .partition(|a| a.group_id == group_id && (a.iso_year, a.iso_week) > current);
    state.slot_assignments = keep;
    future
}

/// Change a group's rhythm. Turns of the current week that already exist
/// keep their assignee (and new shifts of this week are filled); everything
/// after it is re-planned in the new rhythm, with the people drawn for the
/// dropped turns put back at the front of the queue in their order — so
/// nobody loses or gains a turn by the change.
pub(crate) fn apply_rhythm_change(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &GroupId,
    rhythm: crate::rhythm::Rhythm,
) -> anyhow::Result<()> {
    let group = state
        .group_by_id(group_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Unknown group"))?;
    let current = current_iso_week();
    let next = add_weeks(current.0, current.1, 1);
    let mut rhythm = rhythm;
    rhythm.effective_from = Some(crate::rhythm::week_monday(next.0, next.1));
    rhythm.previous = group.rhythm.previous.clone();
    let mut prior = group.rhythm.for_week(current.0, current.1).clone();
    prior.previous.clear();
    if rhythm.previous.last() != Some(&prior) {
        rhythm.previous.push(prior);
    }
    // Validate protected records before making any changes.
    let mut proposed = state.clone();
    proposed.apply_event(DomainEvent::RhythmSet {
        group_id: group_id.clone(),
        rhythm: rhythm.clone(),
    })?;
    let new_group = proposed.group_by_id(group_id).unwrap();
    for a in state
        .slot_assignments
        .iter()
        .filter(|a| a.group_id == *group_id && (a.iso_year, a.iso_week) > current)
    {
        let turn = Turn::new(a.iso_year, a.iso_week, a.shift);
        if (a.source != AssignmentSource::RoundRobin
            || state.completion_for(&group, a.slot_index, turn).is_some())
            && (!proposed
                .turns_in_week(new_group, a.iso_year, a.iso_week)
                .contains(&turn)
                || turn.dates(&group.rhythm) != turn.dates(&rhythm))
        {
            anyhow::bail!("This change would move a protected assignment in {}-W{:02}. Resolve it explicitly before changing rhythm.", a.iso_year, a.iso_week);
        }
    }
    let dropped = take_replannable_assignments(state, &group, Turn::new(next.0, next.1, 0));
    let queue = resolver::rewind_queue(&resolver::reconcile_queue(state, &group), &dropped);
    state.apply_event(DomainEvent::RotationQueueSet {
        group_id: group_id.clone(),
        queue,
    })?;
    state.apply_event(DomainEvent::RhythmSet {
        group_id: group_id.clone(),
        rhythm,
    })?;
    materialize_group_and_apply(
        state,
        group_id,
        ctx.config.schedule.materialize_weeks as usize,
    )
}

/// Add `person_id` to `group_id` and fold them into the group's *next*
/// rotation cycle, re-planning the already-frozen weeks from there on.
/// Returns the first frozen turn whose assignee changed (`None` if none
/// had to).
///
/// Must be called with `state` already locked and `person_id` already a
/// registered `Person`, BEFORE `PersonJoinedGroup` is applied (this function
/// applies it).
///
/// What stays exactly as it was: everything up to and including the active
/// week (frozen first, with the *pre-join* queue), the rest of the cycle
/// that's currently running, and any later week that isn't a plain
/// round-robin pick (`!plan assign`, takeovers, swaps, imports) or is
/// already done/skipped.
///
/// A cycle starts whenever the group's anchor (`resolver::cycle_anchor`,
/// the head of the original rotation) is due again. From that turn on, the
/// round-robin weeks are dropped, their draws rewound onto the queue, and
/// the next cycle is re-seated with every newcomer — this joiner plus anyone
/// who joined earlier and hasn't had a turn before that boundary — spread
/// through it by `resolver::spread_newcomers`. Then the same horizon is
/// filled again (extended only if the joiner would otherwise have no
/// frozen turn yet). No randomness: the same state always yields the same
/// plan, and a restart's additive materialize leaves it alone.
pub(crate) fn apply_group_join(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &GroupId,
    person_id: &PersonId,
) -> anyhow::Result<Option<Turn>> {
    let rotation_was_empty = state
        .group_by_id(group_id)
        .is_some_and(|g| g.member_ids.is_empty());

    // Freeze the active week (if not already frozen) using the OLD queue,
    // before the newcomer can possibly be picked for a week already underway.
    freeze_schedule_before_join(ctx, state, group_id)?;

    state.apply_event(DomainEvent::PersonJoinedGroup {
        person_id: person_id.clone(),
        group_id: group_id.clone(),
    })?;

    if rotation_was_empty {
        // First-ever member: don't let materialize hand them a week that's
        // already underway — their first real turn starts next week.
        keep_active_week_unassigned_for_first_member(ctx, state, group_id)?;
    }

    replan_from_next_cycle(state, group_id, person_id)
}

/// The re-planning half of `apply_group_join` (see there).
fn replan_from_next_cycle(
    state: &mut crate::state::State,
    group_id: &GroupId,
    joiner: &PersonId,
) -> anyhow::Result<Option<Turn>> {
    let Some(group) = state.group_by_id(group_id).cloned() else {
        return Ok(None);
    };
    let current = current_iso_week();
    let horizon = group_horizon_weeks_ahead(state, group_id);

    let anchor = resolver::cycle_anchor(state, &group);
    let boundary = anchor
        .as_ref()
        .and_then(|a| resolver::next_cycle_start(state, &group, a, current));
    let dropped = match boundary {
        Some(from) => take_replannable_assignments(state, &group, from),
        None => Vec::new(),
    };
    let queue = resolver::rewind_queue(&resolver::reconcile_queue(state, &group), &dropped);

    // Whatever is queued ahead of the anchor still finishes the running
    // cycle (only when the anchor's next turn isn't frozen yet); the next
    // cycle starts at the anchor.
    let split = anchor
        .as_ref()
        .and_then(|a| queue.iter().position(|pid| pid == a))
        .unwrap_or(0);
    let (running, next_cycle) = queue.split_at(split);
    let had_turn_before_boundary = |pid: &PersonId| {
        state.slot_assignments.iter().any(|a| {
            a.group_id == group.id
                && a.person_id.as_ref() == Some(pid)
                && boundary.is_none_or(|b| Turn::new(a.iso_year, a.iso_week, a.shift) < b)
        })
    };
    let (newcomers, old): (Vec<PersonId>, Vec<PersonId>) = next_cycle
        .iter()
        .cloned()
        .partition(|pid| pid == joiner || !had_turn_before_boundary(pid));
    let mut queue = running.to_vec();
    queue.extend(resolver::spread_newcomers(&old, &newcomers));
    state.apply_event(DomainEvent::RotationQueueSet {
        group_id: group_id.clone(),
        queue,
    })?;

    // Refill the horizon the group already had; reach further only as far
    // as it takes to give the joiner a frozen turn.
    let max_cycles = horizon + 2 * (group.member_ids.len() + 1);
    let mut cycles = horizon;
    loop {
        refill_replanned(state, group_id, cycles, &dropped)?;
        if first_turn_of(state, group_id, joiner).is_some() || cycles >= max_cycles {
            break;
        }
        cycles += 1;
    }
    Ok(first_changed_turn(state, &dropped))
}

/// Materialize `group_id` for `cycles` due weeks after a re-plan dropped
/// `replaced`, recording who held each refilled turn before (audit only).
fn refill_replanned(
    state: &mut crate::state::State,
    group_id: &GroupId,
    cycles: usize,
    replaced: &[crate::domain::SlotAssignment],
) -> anyhow::Result<()> {
    let Some(group) = state.group_by_id(group_id).cloned() else {
        return Ok(());
    };
    for mut ev in resolver::materialize_group(state, &group, cycles) {
        if let DomainEvent::SlotAssigned {
            slot_index,
            iso_year,
            iso_week,
            shift,
            previous_person_id,
            ..
        } = &mut ev
        {
            *previous_person_id = replaced
                .iter()
                .find(|a| {
                    (a.slot_index, a.iso_year, a.iso_week, a.shift)
                        == (*slot_index, *iso_year, *iso_week, *shift)
                })
                .and_then(|a| a.person_id.clone());
        }
        state.apply_event(ev)?;
    }
    Ok(())
}

/// The first of the `replaced` turns whose assignee is different now — the
/// first dropped turn may be redrawn identically (or not be redrawn at all).
fn first_changed_turn(
    state: &crate::state::State,
    replaced: &[crate::domain::SlotAssignment],
) -> Option<Turn> {
    replaced
        .iter()
        .filter(|old| {
            !state.slot_assignments.iter().any(|a| {
                a.group_id == old.group_id
                    && (a.slot_index, a.iso_year, a.iso_week, a.shift)
                        == (old.slot_index, old.iso_year, old.iso_week, old.shift)
                    && a.person_id == old.person_id
            })
        })
        .map(|a| Turn::new(a.iso_year, a.iso_week, a.shift))
        .min()
}

/// Give the group's plain rotation turns after the current week back to the
/// queue, in the order they were drawn — as if they had never been planned.
/// Pinned weeks and anything done stay. Used when a group is disabled
/// (nothing may advance its rotation while nobody cleans) and re-enabled
/// (pick up exactly where it stopped).
pub(crate) fn return_future_rotation_turns(
    state: &mut crate::state::State,
    group_id: &GroupId,
) -> anyhow::Result<Vec<crate::domain::SlotAssignment>> {
    let Some(group) = state.group_by_id(group_id).cloned() else {
        return Ok(Vec::new());
    };
    let (y, w) = current_iso_week();
    let (ny, nw) = add_weeks(y, w, 1);
    let dropped = take_replannable_assignments(state, &group, Turn::new(ny, nw, 0));
    let queue = resolver::rewind_queue(&resolver::reconcile_queue(state, &group), &dropped);
    state.apply_event(DomainEvent::RotationQueueSet {
        group_id: group_id.clone(),
        queue,
    })?;
    Ok(dropped)
}

/// Remove and return the group's round-robin assignments from turn `from`
/// on — the ones a re-plan may redraw. Admin/self-service/imported weeks
/// and anything already done or skipped stay where they are.
fn take_replannable_assignments(
    state: &mut crate::state::State,
    group: &CleaningGroup,
    from: Turn,
) -> Vec<crate::domain::SlotAssignment> {
    let (taken, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut state.slot_assignments)
        .into_iter()
        .partition(|a| {
            let turn = Turn::new(a.iso_year, a.iso_week, a.shift);
            a.group_id == group.id
                && turn >= from
                && a.source == AssignmentSource::RoundRobin
                && state.completion_for(group, a.slot_index, turn).is_none()
        });
    state.slot_assignments = keep;
    taken
}

/// `person_id`'s first frozen turn in `group_id` after the current week.
pub(crate) fn first_turn_of(
    state: &crate::state::State,
    group_id: &GroupId,
    person_id: &PersonId,
) -> Option<Turn> {
    let current = current_iso_week();
    state
        .slot_assignments
        .iter()
        .filter(|a| {
            a.group_id == *group_id
                && a.person_id.as_ref() == Some(person_id)
                && (a.iso_year, a.iso_week) > current
        })
        .map(|a| Turn::new(a.iso_year, a.iso_week, a.shift))
        .min()
}

/// The group's name as configured (whatever case the command used).
/// The reply when a group name matches nothing.
pub(crate) fn group_not_found(name: &str) -> String {
    format!("❌ Group «{name}» not found — !groups lists them.")
}

pub(crate) fn group_name_of(state: &crate::state::State, group_id: &GroupId) -> String {
    state
        .group_by_id(group_id)
        .map(|g| g.name.clone())
        .unwrap_or_default()
}

/// Confirmation lines for a join: the newcomer's first scheduled turn and
/// what happened to the plan around it.
pub(crate) fn join_summary(
    state: &crate::state::State,
    group_id: &GroupId,
    person_id: &PersonId,
    replanned_from: Option<Turn>,
) -> String {
    let Some(group) = state.group_by_id(group_id) else {
        return String::new();
    };
    if !group.is_active {
        return format!(
            "🚫 {} is disabled — turns start once it's enabled again.",
            group.name
        );
    }
    let first = match first_turn_of(state, group_id, person_id) {
        Some(turn) => format!(
            "📅 First turn: {}",
            crate::view::turn_label(turn, &group.rhythm)
        ),
        None => "📅 First turn: not planned yet.".to_owned(),
    };
    let plan = match replanned_from {
        Some(turn) => format!(
            "🔄 Re-planned from week {} on; earlier weeks unchanged.",
            turn.week
        ),
        None => "No planned week had to change.".to_owned(),
    };
    format!("{first}\n{plan}")
}

/// What a departure did to the plan, for the confirmation message.
#[derive(Default)]
pub(crate) struct Departure {
    /// The leaver's future turns, all handed on to others.
    pub(crate) vacated: Vec<crate::domain::SlotAssignment>,
    /// The first turn whose assignee changed.
    pub(crate) changed_from: Option<Turn>,
}

/// Take `person_id` (already removed via `PersonLeftGroup`) out of
/// `group_id`'s plan from the current week on.
///
/// Must be called with `state` already locked, AFTER `PersonLeftGroup` is
/// applied (so `reconcile_queue` naturally drops the leaver).
///
/// Everything up to and including the current week stays as it was (an open
/// duty this week blocks leaving in the first place), and so does every turn
/// before the leaver's first future one. From that turn on the plain
/// round-robin weeks are redrawn from the rewound queue without the leaver:
/// everyone after them simply moves up one turn per vacated turn, the
/// mirror image of `apply_group_join`. Other members' pinned weeks
/// (`!plan assign`, takeovers, swaps, imports) and anything already done stay;
/// the leaver's own pinned weeks are refilled too and reported back via
/// `Departure::vacated`, never dropped silently.
pub(crate) fn apply_group_departure(
    _ctx: &BotContext,
    state: &mut crate::state::State,
    person_id: &PersonId,
    group_id: &GroupId,
) -> anyhow::Result<Departure> {
    let Some(group) = state.group_by_id(group_id).cloned() else {
        return Ok(Departure::default());
    };
    let horizon = group_horizon_weeks_ahead(state, group_id);
    let vacated = remove_future_assignments_for_person(state, person_id, group_id);
    let mut replaced = match vacated
        .iter()
        .map(|a| Turn::new(a.iso_year, a.iso_week, a.shift))
        .min()
    {
        Some(from) => take_replannable_assignments(state, &group, from),
        None => Vec::new(),
    };
    let queue = resolver::rewind_queue(&resolver::reconcile_queue(state, &group), &replaced);
    state.apply_event(DomainEvent::RotationQueueSet {
        group_id: group_id.clone(),
        queue,
    })?;

    // Refill within whatever horizon this group already had — a leave
    // creates gaps, it never needs to extend the horizon further out.
    replaced.extend(vacated.iter().cloned());
    refill_replanned(state, group_id, horizon, &replaced)?;
    Ok(Departure {
        changed_from: first_changed_turn(state, &replaced),
        vacated,
    })
}

/// Confirmation lines for a departure: what happened to the leaver's
/// upcoming turns, naming each pinned one and who holds it now.
pub(crate) fn departure_summary(
    state: &crate::state::State,
    group_id: &GroupId,
    departure: &Departure,
) -> String {
    let Some(group) = state.group_by_id(group_id) else {
        return String::new();
    };
    if departure.vacated.is_empty() {
        return "No upcoming turns to hand on — nothing else changed.".to_owned();
    }
    let turns = crate::view::plural(departure.vacated.len(), "upcoming turn", "upcoming turns");
    let mut lines = vec![match departure.changed_from {
        Some(turn) => format!(
            "🔄 {turns} handed on — from week {}, everyone after moves up one turn.",
            turn.week
        ),
        None => format!("🔄 {turns} handed on."),
    }];
    let mut pinned: Vec<&crate::domain::SlotAssignment> = departure
        .vacated
        .iter()
        .filter(|a| a.source != AssignmentSource::RoundRobin)
        .collect();
    pinned.sort_by_key(|a| (a.iso_year, a.iso_week, a.shift, a.slot_index));
    for a in &pinned {
        let turn = Turn::new(a.iso_year, a.iso_week, a.shift);
        let now = state
            .slot_assignee(group, a.slot_index, turn)
            .map(|p| p.display_name.clone())
            .unwrap_or_else(|| "nobody".into());
        let duty = Duty {
            group: group.clone(),
            slot_index: a.slot_index,
            turn,
        };
        lines.push(format!(
            "⚠️ {} week {} ({}) → now {now}",
            capitalize(crate::view::source_note(&a.source).unwrap_or("planned")),
            turn.week,
            duty.label(),
        ));
    }
    if !pinned.is_empty() {
        lines.push("Use !plan assign if that was arranged differently.".into());
    }
    lines.join("\n")
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Remove and return `person_id`'s assignments in `group_id` after the
/// current week, whatever their source.
pub(crate) fn remove_future_assignments_for_person(
    state: &mut crate::state::State,
    person_id: &str,
    group_id: &str,
) -> Vec<crate::domain::SlotAssignment> {
    let current = current_iso_week();
    let (removed, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut state.slot_assignments)
        .into_iter()
        .partition(|a| {
            a.group_id == group_id
                && a.person_id.as_deref() == Some(person_id)
                && (a.iso_year, a.iso_week) > current
        });
    state.slot_assignments = keep;
    removed
}

/// Which turn a command means: `week <1-53>` and/or `on <weekday>`,
/// anywhere in the arguments.
pub(crate) struct TurnArgs<'a> {
    /// The arguments without those clauses.
    pub(crate) rest: Vec<&'a str>,
    /// The named week, or the current one — rolling into next year when the
    /// week number has already passed this year.
    pub(crate) week: (i32, u32),
    /// Weekday (0 = Monday) selecting a shift in a group split into shifts.
    pub(crate) day: Option<u8>,
}

/// Extract `week <N>` and `on <weekday>` clauses. `None` means a keyword was
/// not followed by a valid value; the caller shows its usage message.
pub(crate) fn extract_turn_args<'a>(args: &[&'a str]) -> Option<TurnArgs<'a>> {
    let (cur_y, cur_w) = current_iso_week();
    let mut out = TurnArgs {
        rest: Vec::new(),
        week: (cur_y, cur_w),
        day: None,
    };
    let mut i = 0;
    while i < args.len() {
        let token = args[i];
        if token.eq_ignore_ascii_case("week") {
            let n: u32 = args
                .get(i + 1)
                .and_then(|s| s.parse().ok())
                .filter(|n| (1..=53).contains(n))?;
            out.week = (if n < cur_w { cur_y + 1 } else { cur_y }, n);
            i += 2;
        } else if token.eq_ignore_ascii_case("on") {
            out.day = Some(args.get(i + 1).and_then(|d| parse_weekday(d))?);
            i += 2;
        } else {
            out.rest.push(token);
            i += 1;
        }
    }
    Some(out)
}

/// The turns of `group` a command refers to in `week`: the shift containing
/// `day`, or every turn of the week. An error names the group's rhythm when
/// it isn't cleaned that week.
pub(crate) fn turns_for(
    state: &crate::state::State,
    group: &CleaningGroup,
    week: (i32, u32),
    day: Option<u8>,
) -> std::result::Result<Vec<Turn>, String> {
    let turns = state.turns_in_week(group, week.0, week.1);
    if turns.is_empty() {
        return Err(format!(
            "«{}» is not cleaned in week {} (it is cleaned {}).",
            group.name,
            week.1,
            group.rhythm.for_week(week.0, week.1).describe()
        ));
    }
    if day.is_some_and(|day| !group.rhythm.for_week(week.0, week.1).contains_weekday(day)) {
        return Err("That day is outside the cleaning windows.".into());
    }
    Ok(match day {
        Some(day) => vec![Turn::new(
            week.0,
            week.1,
            group.rhythm.for_week(week.0, week.1).shift_for_weekday(day),
        )],
        None => turns,
    })
}

/// Exactly one turn of `week`: the shift named by `day`, or the only shift
/// of a group that isn't split. Otherwise asks for `on <day>`.
pub(crate) fn single_turn(
    state: &crate::state::State,
    group: &CleaningGroup,
    week: (i32, u32),
    day: Option<u8>,
) -> std::result::Result<Turn, String> {
    let turns = turns_for(state, group, week, day)?;
    match turns.as_slice() {
        [turn] => Ok(*turn),
        _ => Err(shift_hint(group, week)),
    }
}

/// "«Bathroom» is cleaned in shifts (Mon–Wed, Thu–Sun) — add `on <day>`, e.g. `on thu`."
pub(crate) fn shift_hint(group: &CleaningGroup, week: (i32, u32)) -> String {
    let rhythm = group.rhythm.for_week(week.0, week.1);
    let shifts: Vec<String> = rhythm.shifts().iter().map(|s| s.label()).collect();
    let example = rhythm.shifts().get(1).map_or("mon", |s| {
        ["mon", "tue", "wed", "thu", "fri", "sat", "sun"][s.start as usize]
    });
    format!(
        "«{}» is cleaned in shifts ({}) — add `on <day>`, e.g. `on {example}`.",
        group.name,
        shifts.join(", ")
    )
}

/// Resolve `<group> [<slot>]` against `state`, matching the convention used
/// by `!groups room add`/`!groups room remove`: the token right after the group name is only
/// treated as a slot name when the group is multi-slot and it actually
/// matches one of its slots.  Returns the group id, the slot index to use in
/// a `SlotAssignment` (0 for single-slot groups), and the remaining args.
pub(crate) fn resolve_group_and_slot<'r, 'a>(
    state: &crate::state::State,
    group_name: &str,
    rest: &'r [&'a str],
) -> std::result::Result<(GroupId, usize, &'r [&'a str]), String> {
    let group = state
        .group_by_name(group_name)
        .ok_or_else(|| group_not_found(group_name))?;
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
/// defaults instead of requiring the full explicit syntax:
///
/// - No group given → the sender's own group, provided they're in exactly
///   one (never guessed among several — that returns a message listing them).
/// - A single token that isn't a group name is read as a slot of that group.
/// - Turn and slot are picked automatically when exactly one candidate
///   (not done, not already the sender's) is left; in the current week the
///   running turn wins. Otherwise the choices are listed instead of guessed.
pub(crate) fn resolve_takeover_target(
    state: &crate::state::State,
    sender_person_id: &PersonId,
    rest: &[&str],
    week: (i32, u32),
    day: Option<u8>,
) -> std::result::Result<(CleaningGroup, usize, Turn), String> {
    fn own_group(
        state: &crate::state::State,
        sender_person_id: &PersonId,
    ) -> std::result::Result<CleaningGroup, String> {
        match state.groups_for_person(sender_person_id).as_slice() {
            [] => Err("You are not in any cleaning group. Specify one: !takeover <group> [<slot>]".into()),
            [g] => Ok((*g).clone()),
            many => Err(format!(
                "You are in multiple groups — specify one: {}\nUsage: !takeover <group> [<slot>] [week <N>] [on <day>]",
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

    let slots: Vec<usize> = match slot_token {
        Some(name) => {
            if !group.is_multi_slot() {
                return Err(format!("«{}» does not have slots.", group.name));
            }
            match group
                .slots
                .iter()
                .position(|s| s.name.eq_ignore_ascii_case(name))
            {
                Some(idx) => vec![idx],
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
        None => crate::state::State::slot_indices(&group).collect(),
    };

    let turns = turns_for(state, &group, week, day)?;
    let candidates: Vec<(usize, Turn)> = turns
        .iter()
        .filter(|t| !state.turn_over(&group, **t))
        .flat_map(|t| slots.iter().map(move |i| (*i, *t)))
        .filter(|(i, t)| {
            !state.is_turn_slot_done(&group, *i, *t)
                && state
                    .slot_assignee(&group, *i, *t)
                    .is_none_or(|p| &p.id != sender_person_id)
        })
        .collect();
    let running = state.current_turn(&group);
    let preferred: Vec<(usize, Turn)> = if day.is_none() {
        candidates
            .iter()
            .copied()
            .filter(|(_, t)| Some(*t) == running)
            .collect()
    } else {
        Vec::new()
    };
    let pick = match (preferred.as_slice(), candidates.as_slice()) {
        ([one], _) | ([], [one]) => *one,
        (_, []) => {
            return Err(format!(
                "Nothing to take over in «{}» then — already completed or skipped, over, or yours.",
                group.name
            ))
        }
        _ => {
            let options: Vec<String> = candidates
                .iter()
                .map(|(i, t)| {
                    Duty {
                        group: group.clone(),
                        slot_index: *i,
                        turn: *t,
                    }
                    .label()
                })
                .collect();
            let how = if group.is_multi_slot() && group.rhythm.is_split() {
                "a slot and `on <day>`"
            } else if group.is_multi_slot() {
                "a slot"
            } else {
                "`on <day>`"
            };
            return Err(format!(
                "Several open turns: {} — specify {how}: !takeover {} …",
                options.join(", "),
                group.name
            ));
        }
    };
    Ok((group, pick.0, pick.1))
}

pub(crate) fn validate_matrix_user_id(mxid: &str) -> std::result::Result<(), String> {
    OwnedUserId::try_from(mxid)
        .map(|_| ())
        .map_err(|_| format!("«{mxid}» is not a valid Matrix user ID. Use @user:server."))
}

/// Freeze the group's current week (if due and not frozen yet) using the
/// *pre-join* queue, so nobody's task this week changes because someone
/// else just joined.
pub(crate) fn freeze_schedule_before_join(
    _ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &GroupId,
) -> anyhow::Result<()> {
    let Some(group) = state.group_by_id(group_id).cloned() else {
        return Ok(());
    };
    let current = current_iso_week();
    if state.next_due_week(&group, current) != current {
        return Ok(());
    }
    // One cycle = exactly this week; apply its picks AND the queue
    // advancement that produced them (else the picked people would stay at
    // the queue's front and be reused for the next week too).
    for event in resolver::materialize_group(state, &group, 1) {
        state.apply_event(event)?;
    }
    Ok(())
}

/// Freezes the current week as explicitly unassigned for a group that just
/// got its first member, so their real first turn starts next time instead
/// of in a week already underway. Leaves the queue untouched (them at the
/// front) so they're picked, for real, next.
pub(crate) fn keep_active_week_unassigned_for_first_member(
    _ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &GroupId,
) -> anyhow::Result<()> {
    let Some(group) = state.group_by_id(group_id).cloned() else {
        return Ok(());
    };
    let (year, week) = current_iso_week();
    for turn in state.turns_in_week(&group, year, week) {
        for slot_index in crate::state::State::slot_indices(&group) {
            let frozen = state.slot_assignments.iter().any(|a| {
                a.group_id == group.id
                    && a.slot_index == slot_index
                    && (a.iso_year, a.iso_week, a.shift) == (turn.year, turn.week, turn.shift)
            });
            if !frozen {
                state.apply_event(DomainEvent::SlotAssigned {
                    group_id: group.id.clone(),
                    slot_index,
                    iso_year: turn.year,
                    iso_week: turn.week,
                    shift: turn.shift,
                    person_id: None,
                    source: AssignmentSource::RoundRobin,
                    actor_id: None,
                    previous_person_id: None,
                })?;
            }
        }
    }
    Ok(())
}

/// Open duties of `person_id` in `group_id` this week, as labels (all
/// shifts, also ones that haven't started yet).
pub(crate) fn current_open_assignments(
    state: &crate::state::State,
    group_id: &GroupId,
    person_id: &PersonId,
) -> Vec<String> {
    let (year, week) = current_iso_week();
    let Some(group) = state.group_by_id(group_id) else {
        return Vec::new();
    };
    state
        .turns_in_week(group, year, week)
        .into_iter()
        .flat_map(|turn| {
            state
                .held_slots(group, person_id, turn)
                .into_iter()
                .filter(move |i| !state.is_turn_slot_done(group, *i, turn))
                .map(move |slot_index| {
                    Duty {
                        group: group.clone(),
                        slot_index,
                        turn,
                    }
                    .label()
                })
        })
        .collect()
}

/// Resolve a person argument (see `State::find_persons`): `Ok(None)` when
/// nobody matches — callers word that themselves — and an error message
/// listing the candidates when a display name is ambiguous, rather than
/// silently picking one of them.
pub(crate) fn lookup_person<'a>(
    state: &'a crate::state::State,
    query: &str,
) -> std::result::Result<Option<&'a Person>, String> {
    match state.find_persons(query).as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one)),
        several => {
            // The candidates' IDs as pills that ping nobody — they're only
            // being looked up.
            let who: Vec<String> = several
                .iter()
                .map(|p| match p.matrix_id.as_deref() {
                    Some(mxid) => {
                        format!("{} ({})", p.display_name, crate::view::user_id_link(mxid))
                    }
                    None => format!("{} (no Matrix)", p.display_name),
                })
                .collect();
            Err(format!(
                "«{query}» matches {} people: {} — use the Matrix ID instead.",
                several.len(),
                who.join(", ")
            ))
        }
    }
}

pub(crate) fn person_label(person: &Person) -> String {
    match person.matrix_id.as_deref() {
        Some(mxid) if person.display_name != mxid => format!("{} ({mxid})", person.display_name),
        Some(mxid) => mxid.to_owned(),
        None => format!("{} (no Matrix)", person.display_name),
    }
}

/// "Next: Bob · Thu–Sun 25 – 28 Sep (week 39)." — the group's first turn
/// that starts after today.
pub(crate) fn next_assignment_summary(state: &crate::state::State, group_id: &GroupId) -> String {
    let Some(group) = state.group_by_id(group_id) else {
        return "Next: unavailable.".to_owned();
    };
    if group.member_ids.is_empty() {
        return "Next: rotation is empty.".to_owned();
    }
    let today = crate::state::today();
    let next = state
        .slot_assignments
        .iter()
        .filter(|a| a.group_id == *group_id)
        .map(|a| Turn::new(a.iso_year, a.iso_week, a.shift))
        .filter(|t| t.dates(&group.rhythm).0 > today)
        .min();
    let Some(turn) = next else {
        return "Next: no future assignment is materialized.".to_owned();
    };
    let mut names: Vec<String> = state
        .turn_assignees(group, turn)
        .into_iter()
        .filter_map(|(_, p)| p.map(person_label))
        .collect();
    names.dedup();
    let who = if names.is_empty() {
        "unassigned".to_owned()
    } else {
        names.join(", ")
    };
    format!(
        "Next: {who} · {} (week {}).",
        turn.period_label(&group.rhythm),
        turn.week
    )
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

/// One slot of one turn someone is responsible for.
#[derive(Clone)]
pub(crate) struct Duty {
    pub(crate) group: CleaningGroup,
    pub(crate) slot_index: usize,
    pub(crate) turn: Turn,
}

impl Duty {
    /// "Bathroom", "Bathroom · Mon–Wed", "Floor / Kitchen · Thu–Sun".
    pub(crate) fn label(&self) -> String {
        let mut label = self.group.name.clone();
        if let Some(slot) = self.group.slots.get(self.slot_index) {
            label.push_str(&format!(" / {}", slot.name));
        }
        if let Some(shift) = self.turn.shift_label(&self.group.rhythm) {
            label.push_str(&format!(" · {shift}"));
        }
        label
    }
}

/// `person_id`'s open duties in one week (optionally one group): those whose
/// turn has started — or, if none has yet, the week's next one, since
/// cleaning a little early is fine. Shared by `!done` and the ✅ reaction.
pub(crate) fn markable_duties(
    state: &crate::state::State,
    person_id: &PersonId,
    (year, week): (i32, u32),
    only_group: Option<&GroupId>,
) -> Vec<Duty> {
    let mut open: Vec<Duty> = Vec::new();
    for group in state.cleaning_groups.iter().filter(|g| g.is_active) {
        if only_group.is_some_and(|id| id != &group.id) {
            continue;
        }
        for turn in state.turns_in_week(group, year, week) {
            for slot_index in state.held_slots(group, person_id, turn) {
                if !state.is_turn_slot_done(group, slot_index, turn)
                    && (group.rhythm.for_week(year, week).shift_ends.is_empty()
                        || state.turn_started(group, turn))
                {
                    open.push(Duty {
                        group: group.clone(),
                        slot_index,
                        turn,
                    });
                }
            }
        }
    }
    let started: Vec<Duty> = open
        .iter()
        .filter(|d| state.turn_started(&d.group, d.turn))
        .cloned()
        .collect();
    if !started.is_empty() {
        return started;
    }
    let first = open.iter().map(|d| d.turn.dates(&d.group.rhythm).0).min();
    open.into_iter()
        .filter(|d| Some(d.turn.dates(&d.group.rhythm).0) == first)
        .collect()
}

/// Take back what a ✅ reaction marked (its redaction), through the event
/// log like `!undo`. Only marks still made by the reacting person go — a
/// skip or someone else's mark on the same duty since then stays. Returns
/// whether anything changed.
pub(crate) fn undo_reaction_done(
    state: &mut crate::state::State,
    rd: &crate::state::ReactionDone,
) -> anyhow::Result<bool> {
    let marked: Vec<crate::state::MarkedDuty> = if rd.marked.is_empty() {
        // Older record: every mark of that person in that group and week.
        state
            .completions
            .iter()
            .filter(|c| {
                c.group_id == rd.group_id
                    && (c.iso_year, c.iso_week) == (rd.iso_year, rd.iso_week)
                    && c.completed_by_id == rd.completed_by_id
                    && !c.skipped
            })
            .map(|c| crate::state::MarkedDuty {
                group_id: c.group_id.clone(),
                slot_id: c.slot_id.clone(),
                shift: c.shift,
            })
            .collect()
    } else {
        rd.marked.clone()
    };
    let mut changed = false;
    for duty in marked {
        let still_theirs = state.completions.iter().any(|c| {
            c.group_id == duty.group_id
                && (c.iso_year, c.iso_week, c.shift) == (rd.iso_year, rd.iso_week, duty.shift)
                && c.slot_id == duty.slot_id
                && c.completed_by_id == rd.completed_by_id
                && !c.skipped
        });
        if still_theirs {
            state.apply_event(DomainEvent::CleaningUndone {
                group_id: duty.group_id,
                iso_year: rd.iso_year,
                iso_week: rd.iso_week,
                slot_id: duty.slot_id,
                shift: Some(duty.shift),
            })?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Record `duties` as cleaned by `person_id`.
pub(crate) fn mark_duties_done(
    state: &mut crate::state::State,
    person_id: &PersonId,
    duties: &[Duty],
) -> anyhow::Result<()> {
    for duty in duties {
        let responsible = state
            .slot_assignee(&duty.group, duty.slot_index, duty.turn)
            .map(|p| vec![p.id.clone()])
            .unwrap_or_default();
        state.apply_event(DomainEvent::CleaningCompleted {
            group_id: duty.group.id.clone(),
            slot_id: duty.group.slots.get(duty.slot_index).map(|s| s.id.clone()),
            person_id: person_id.clone(),
            responsible_person_ids: responsible,
            iso_year: duty.turn.year,
            iso_week: duty.turn.week,
            shift: duty.turn.shift,
        })?;
    }
    Ok(())
}
