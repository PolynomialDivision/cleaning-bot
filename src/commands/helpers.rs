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
        "!member" => matches!(sub, Some("remove" | "away" | "back")),
        _ => false,
    }
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
) -> anyhow::Result<usize> {
    let cleared = drop_future_assignments(state, group_id).len();
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
    state.apply_event(DomainEvent::RhythmSet {
        group_id: group_id.clone(),
        rhythm: rhythm.clone(),
    })?;
    let dropped = drop_future_assignments(state, group_id);
    // This week's turns that no longer exist (fewer shifts now).
    let current = current_iso_week();
    let shifts = rhythm.shift_count() as u8;
    state.slot_assignments.retain(|a| {
        !(a.group_id == *group_id && (a.iso_year, a.iso_week) == current && a.shift >= shifts)
    });
    if let Some(group) = state.group_by_id(group_id).cloned() {
        let queue = resolver::rewind_queue(&resolver::reconcile_queue(state, &group), &dropped);
        state.apply_event(DomainEvent::RotationQueueSet {
            group_id: group_id.clone(),
            queue,
        })?;
    }
    let cycles = ctx.config.schedule.materialize_weeks as usize;
    materialize_group_and_apply(state, group_id, cycles)
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
    // join. (Deeper horizons still get filled by bot startup or !plan reset.)
    let materialize_weeks = ctx.config.schedule.materialize_weeks as usize;
    let horizon = group_horizon_weeks_ahead(state, group_id);
    let cycles_ahead = (horizon + 1).min(materialize_weeks.max(horizon));
    materialize_group_and_apply(state, group_id, cycles_ahead)
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
    _ctx: &BotContext,
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
    let horizon = group_horizon_weeks_ahead(state, group_id);
    materialize_group_and_apply(state, group_id, horizon)?;
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
            group.rhythm.describe()
        ));
    }
    Ok(match day {
        Some(day) => vec![Turn::new(
            week.0,
            week.1,
            group.rhythm.shift_for_weekday(day),
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
        _ => Err(shift_hint(group)),
    }
}

/// "«Bathroom» is cleaned in shifts (Mon–Wed, Thu–Sun) — add `on <day>`, e.g. `on thu`."
pub(crate) fn shift_hint(group: &CleaningGroup) -> String {
    let shifts: Vec<String> = group.rhythm.shifts().iter().map(|s| s.label()).collect();
    let example = group.rhythm.shifts().get(1).map_or("mon", |s| {
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
                if !state.is_turn_slot_done(group, slot_index, turn) {
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
