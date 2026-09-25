//! Slot assignment resolver.
//!
//! `materialize` computes frozen assignments for the upcoming turns of every
//! group (one turn = one shift of one due week, in the group's own rhythm —
//! see `rhythm`) and returns them as `SlotAssigned` (+ `RotationQueueSet`)
//! events. It is a pure function — reads state, returns events to apply.
//! Idempotent: already-assigned turns are skipped and never recomputed or
//! overwritten.
//!
//! # Rotation queue
//!
//! Each `CleaningGroup` carries a persisted `rotation_queue: Vec<PersonId>` —
//! the literal turn order, front = next up. For every not-yet-frozen turn,
//! `materialize` pops one member per slot from the front of the queue and
//! pushes them to the back (a classic round-robin turn queue), then freezes
//! that pick as a `SlotAssigned` event. All turns and slots of the same week
//! draw from the same shared queue in sequence, so nobody is drawn twice in
//! one week — including when there are *fewer* members than turns × slots,
//! in which case the leftovers are left unassigned (`person_id: None`)
//! rather than silently double-booking someone.
//!
//! This deliberately replaces a stateless `index % member_count` formula.
//! That formula has to recompute *every* future week's assignee from
//! scratch whenever the member count changes — join or leave one person and
//! the whole rotation shifts. A persisted queue only has to say where a
//! *new* member enters the line: everyone already queued keeps their
//! existing relative order, and every already-frozen week is left alone.
//!
//! `reconcile_queue` keeps the queue in sync with `member_ids`: it drops
//! anyone no longer a member and appends anyone missing. It is also the only
//! "migration" a pre-queue `state.json` needs — see its docs below.
//! `commands::apply_group_join` / `apply_group_departure` call it too, to
//! insert a joiner at the front of the queue or drop a leaver from it.

use std::collections::HashSet;

use crate::{
    analytics::DomainEvent,
    domain::{AssignmentSource, CleaningGroup, PersonId},
    rhythm::Turn,
    state::{add_weeks, current_iso_week, weeks_between, State},
};

/// Reconcile a group's persisted `rotation_queue` against its current
/// `member_ids`: drop ids that are no longer members, append members that
/// aren't queued yet (in `member_ids` order, so a freshly created group
/// starts out in a sensible order), and otherwise preserve everyone's
/// relative position untouched.
///
/// When the queue is completely empty but the group already has members, it
/// is seeded from `member_ids`. If the group already has recorded
/// assignments (i.e. it ran under a pre-queue version of the bot, upgrading
/// in place), the seed is rotated by however many assignments already
/// happened, so the upgrade continues the existing cycle instead of
/// restarting it at `member_ids[0]`.
pub fn reconcile_queue(state: &State, group: &CleaningGroup) -> Vec<PersonId> {
    let known: HashSet<&PersonId> = group.member_ids.iter().collect();
    let mut queue: Vec<PersonId> = group
        .rotation_queue
        .iter()
        .filter(|id| known.contains(id))
        .cloned()
        .collect();

    if queue.is_empty() && !group.member_ids.is_empty() {
        let prior = state
            .slot_assignments
            .iter()
            .filter(|a| a.group_id == group.id && a.person_id.is_some())
            .count();
        let shift = prior % group.member_ids.len();
        queue = group
            .member_ids
            .iter()
            .cloned()
            .cycle()
            .skip(shift)
            .take(group.member_ids.len())
            .collect();
    } else {
        for id in &group.member_ids {
            if !queue.contains(id) {
                queue.push(id.clone());
            }
        }
    }
    queue
}

/// Fill all unassigned turns of every group for its next `cycles_ahead` due
/// weeks (from the current week on, each group in its own rhythm).
///
/// Every shift of a due week is its own turn, and every slot of a turn draws
/// the next person from the group's rotation queue — so a group cleaned
/// twice a week hands its two shifts to two consecutive people in the
/// rotation. Nobody is drawn twice within one week.
///
/// Returns `SlotAssigned` events plus, for each group that had at least one
/// turn filled, a trailing `RotationQueueSet` capturing the queue's new
/// state. Already-stored assignments are skipped so this is safe to call
/// repeatedly, and it never revisits or changes a turn it already froze —
/// callers rely on that to make join/leave additive rather than destructive.
pub fn materialize(state: &State, cycles_ahead: usize) -> Vec<DomainEvent> {
    let mut events = Vec::new();
    for group in &state.cleaning_groups {
        events.extend(materialize_group(state, group, cycles_ahead));
    }
    events
}

/// `materialize` for one group.
pub fn materialize_group(
    state: &State,
    group: &CleaningGroup,
    cycles_ahead: usize,
) -> Vec<DomainEvent> {
    if cycles_ahead == 0 || group.member_ids.is_empty() {
        return vec![];
    }
    let mut events: Vec<DomainEvent> = Vec::new();
    let num_slots = group.slots.len().max(1);
    let every = group.rhythm.every_weeks() as i64;
    let first_due = state.next_due_week(group, current_iso_week());
    let mut queue = reconcile_queue(state, group);
    let mut any_pop = false;

    for i in 0..cycles_ahead as i64 {
        let (dy, dw) = add_weeks(first_due.0, first_due.1, i * every);

        // Track who's already been drawn for *this* week so nobody is
        // picked twice — skipping absent members means a draw can land past
        // the front, so identity (not count) is what has to be deduplicated.
        let mut used_this_week: HashSet<PersonId> = state
            .slot_assignments
            .iter()
            .filter(|a| a.group_id == group.id && (a.iso_year, a.iso_week) == (dy, dw))
            .filter_map(|a| a.person_id.clone())
            .collect();

        for turn in state.turns_in_week(group, dy, dw) {
            for si in 0..num_slots {
                // Skip if already frozen — this is what makes materialize additive.
                if state.slot_assignments.iter().any(|a| {
                    a.group_id == group.id
                        && a.slot_index == si
                        && (a.iso_year, a.iso_week, a.shift) == (turn.year, turn.week, turn.shift)
                }) {
                    continue;
                }

                // First queue member not already used this week and not on
                // record absence. Removed from wherever they sit and pushed
                // to the back like a normal draw — an absent member in front
                // of them is skipped over, not touched, so they keep their
                // place in line and lose no turn.
                let pick_pos = queue.iter().position(|pid| {
                    !used_this_week.contains(pid) && !state.is_absent(pid, &group.id, dy, dw)
                });
                let person_id = pick_pos.map(|pos| {
                    let picked = queue.remove(pos);
                    queue.push(picked.clone());
                    any_pop = true;
                    used_this_week.insert(picked.clone());
                    picked
                });

                events.push(DomainEvent::SlotAssigned {
                    group_id: group.id.clone(),
                    slot_index: si,
                    iso_year: turn.year,
                    iso_week: turn.week,
                    shift: turn.shift,
                    person_id,
                    source: AssignmentSource::RoundRobin,
                    // Automatic — nobody "did" this, and materialize only
                    // ever fills a not-yet-frozen turn, so there is no prior
                    // occupant to record either.
                    actor_id: None,
                    previous_person_id: None,
                });
            }
        }
    }

    if any_pop {
        events.push(DomainEvent::RotationQueueSet {
            group_id: group.id.clone(),
            queue,
        });
    }
    events
}

/// Preview who `materialize` would assign to one (group, slot, turn) beyond
/// the already-materialized horizon, without persisting anything. Used as
/// the fallback in `State::slot_assignee` for turns nobody has frozen yet,
/// so a preview always agrees with what actually gets frozen later.
pub fn preview_slot_assignee(
    state: &State,
    group: &CleaningGroup,
    slot_index: usize,
    turn: Turn,
) -> Option<PersonId> {
    let first_due = state.next_due_week(group, current_iso_week());
    let offset = weeks_between(first_due, turn.week());
    if offset < 0 {
        return None;
    }
    let cycles = (offset as usize) / (group.rhythm.every_weeks() as usize) + 1;

    materialize_group(state, group, cycles)
        .into_iter()
        .find_map(|e| match e {
            DomainEvent::SlotAssigned {
                slot_index: si,
                iso_year,
                iso_week,
                shift,
                person_id,
                ..
            } if si == slot_index && Turn::new(iso_year, iso_week, shift) == turn => person_id,
            _ => None,
        })
}

/// Put the people drawn for `dropped` assignments back at the front of the
/// queue, in the order they were drawn — undoing those draws, so clearing
/// future turns (e.g. after a rhythm change) costs nobody their place.
pub fn rewind_queue(
    queue: &[PersonId],
    dropped: &[crate::domain::SlotAssignment],
) -> Vec<PersonId> {
    let mut drawn: Vec<&crate::domain::SlotAssignment> = dropped.iter().collect();
    drawn.sort_by_key(|a| (a.iso_year, a.iso_week, a.shift, a.slot_index));
    let mut front: Vec<PersonId> = Vec::new();
    for a in drawn {
        if let Some(pid) = &a.person_id {
            if !front.contains(pid) {
                front.push(pid.clone());
            }
        }
    }
    let rest: Vec<PersonId> = queue
        .iter()
        .filter(|pid| !front.contains(pid))
        .cloned()
        .collect();
    front.retain(|pid| queue.contains(pid));
    front.extend(rest);
    front
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CleaningGroup, CleaningSlot, Person},
        state::State,
    };
    use chrono::Utc;

    fn two_person_state() -> (State, String, String) {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let p1 = Person::new_named("Alice");
        let p2 = Person::new_named("Bob");
        let (id1, id2) = (p1.id.clone(), p2.id.clone());
        st.persons.extend([p1, p2]);
        let mut g = CleaningGroup::new("Kitchen");
        g.member_ids.extend([id1.clone(), id2.clone()]);
        st.cleaning_groups.push(g);
        (st, id1, id2)
    }

    #[test]
    fn first_materialization_assigns_in_order() {
        let (st, id1, id2) = two_person_state();
        let evs = materialize(&st, 2);
        // Cycle 1 → Alice (front of queue), Cycle 2 → Bob.
        if let DomainEvent::SlotAssigned { person_id, .. } = &evs[0] {
            assert_eq!(person_id.as_deref(), Some(id1.as_str()));
        }
        if let DomainEvent::SlotAssigned { person_id, .. } = &evs[1] {
            assert_eq!(person_id.as_deref(), Some(id2.as_str()));
        }
    }

    #[test]
    fn already_stored_slots_are_skipped() {
        let (mut st, id1, _id2) = two_person_state();
        // Store the first assignment manually.
        let first_evs = materialize(&st, 1);
        for ev in &first_evs {
            st.apply_event(ev.clone()).unwrap();
        }
        // Running materialize again should produce no new events for week 1.
        let second_evs = materialize(&st, 1);
        assert!(
            second_evs.is_empty(),
            "should skip already-stored assignment"
        );
        // The stored assignment should still be Alice.
        let stored = &st.slot_assignments[0];
        assert_eq!(stored.person_id.as_deref(), Some(id1.as_str()));
    }

    #[test]
    fn rotation_stable_after_member_leaves() {
        let (mut st, id1, id2) = two_person_state();
        // Materialize week 1 (Alice) and week 2 (Bob).
        let evs = materialize(&st, 2);
        for ev in evs {
            st.apply_event(ev).unwrap();
        }

        // Alice leaves.
        let gid = st.cleaning_groups[0].id.clone();
        st.apply_event(DomainEvent::PersonLeftGroup {
            person_id: id1.clone(),
            group_id: gid,
        })
        .unwrap();

        // Materialize week 3: only Bob remains in member_ids, so reconcile_queue
        // drops Alice even though nothing explicitly touched the stored queue.
        let new_evs = materialize(&st, 3);
        let unfrozen: Vec<_> = new_evs
            .iter()
            .filter(|e| matches!(e, DomainEvent::SlotAssigned { .. }))
            .collect();
        assert!(!unfrozen.is_empty());
        if let DomainEvent::SlotAssigned { person_id, .. } = &unfrozen[0] {
            assert_eq!(
                person_id.as_deref(),
                Some(id2.as_str()),
                "Bob should be next"
            );
        }
    }

    #[test]
    fn multi_slot_round_robin() {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let people: Vec<Person> = (0..4)
            .map(|i| Person::new_named(&format!("P{i}")))
            .collect();
        let ids: Vec<_> = people.iter().map(|p| p.id.clone()).collect();
        st.persons.extend(people);

        let mut g = CleaningGroup::new("Floor");
        g.member_ids.extend(ids.clone());
        let mut s0 = CleaningSlot::new("Scharni");
        s0.id = "s0".into();
        let mut s1 = CleaningSlot::new("Colbe");
        s1.id = "s1".into();
        g.slots.extend([s0, s1]);
        st.cleaning_groups.push(g);

        let evs = materialize(&st, 2);
        let slot_evs: Vec<_> = evs
            .iter()
            .filter(|e| matches!(e, DomainEvent::SlotAssigned { .. }))
            .collect();
        assert_eq!(slot_evs.len(), 4);
        // Week1/slot0 → P0, Week1/slot1 → P1, Week2/slot0 → P2, Week2/slot1 → P3.
        if let (
            DomainEvent::SlotAssigned { person_id: p0, .. },
            DomainEvent::SlotAssigned { person_id: p1, .. },
        ) = (slot_evs[0], slot_evs[1])
        {
            assert_eq!(p0.as_deref(), Some(ids[0].as_str()));
            assert_eq!(p1.as_deref(), Some(ids[1].as_str()));
        }
        if let (
            DomainEvent::SlotAssigned { person_id: p2, .. },
            DomainEvent::SlotAssigned { person_id: p3, .. },
        ) = (slot_evs[2], slot_evs[3])
        {
            assert_eq!(p2.as_deref(), Some(ids[2].as_str()));
            assert_eq!(p3.as_deref(), Some(ids[3].as_str()));
        }
    }

    #[test]
    fn three_person_rotation_cycles_stably() {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let people: Vec<Person> = ["Anna", "Bob", "Carla"]
            .iter()
            .map(|n| Person::new_named(n))
            .collect();
        let ids: Vec<_> = people.iter().map(|p| p.id.clone()).collect();
        st.persons.extend(people);
        let mut g = CleaningGroup::new("Floor");
        g.member_ids.extend(ids.clone());
        st.cleaning_groups.push(g);

        let evs = materialize(&st, 6);
        let names: Vec<Option<String>> = evs
            .iter()
            .filter_map(|e| match e {
                DomainEvent::SlotAssigned { person_id, .. } => Some(person_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            vec![
                Some(ids[0].clone()),
                Some(ids[1].clone()),
                Some(ids[2].clone()),
                Some(ids[0].clone()),
                Some(ids[1].clone()),
                Some(ids[2].clone()),
            ]
        );
    }

    #[test]
    fn materialize_is_deterministic() {
        let (st, ..) = two_person_state();
        let a = materialize(&st, 8);
        let b = materialize(&st, 8);
        let render = |evs: &[DomainEvent]| format!("{evs:?}");
        assert_eq!(render(&a), render(&b));
    }

    #[test]
    fn reconcile_seeds_fresh_queue_from_member_ids() {
        let (st, id1, id2) = two_person_state();
        let group = &st.cleaning_groups[0];
        assert_eq!(reconcile_queue(&st, group), vec![id1, id2]);
    }

    #[test]
    fn reconcile_continues_the_cycle_after_an_upgrade_from_a_queueless_state() {
        // Simulate a pre-queue installation: history exists (3 prior
        // assignments to a 2-person group) but `rotation_queue` was never
        // populated (as if this state.json predates the field).
        let (mut st, id1, id2) = two_person_state();
        let gid = st.cleaning_groups[0].id.clone();
        for (i, pid) in [id1.clone(), id2.clone(), id1.clone()]
            .into_iter()
            .enumerate()
        {
            st.slot_assignments.push(crate::domain::SlotAssignment {
                group_id: gid.clone(),
                slot_index: 0,
                iso_year: 2020,
                iso_week: (i + 1) as u32,
                shift: 0,
                person_id: Some(pid),
                source: Default::default(),
            });
        }
        st.cleaning_groups[0].rotation_queue.clear();

        // 3 prior assignments, 2 members → shift by 1 → queue continues [Bob, Alice].
        let queue = reconcile_queue(&st, &st.cleaning_groups[0]);
        assert_eq!(queue, vec![id2, id1]);
    }

    fn make_group(
        name: &str,
        member_count: usize,
        slot_count: usize,
    ) -> (State, crate::domain::GroupId, Vec<PersonId>) {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let people: Vec<Person> = (0..member_count)
            .map(|i| Person::new_named(&format!("P{i}")))
            .collect();
        let ids: Vec<_> = people.iter().map(|p| p.id.clone()).collect();
        st.persons.extend(people);
        let mut g = CleaningGroup::new(name);
        let gid = g.id.clone();
        g.member_ids.extend(ids.clone());
        for i in 0..slot_count {
            let mut slot = CleaningSlot::new(&format!("Slot{i}"));
            slot.id = format!("s{i}");
            g.slots.push(slot);
        }
        st.cleaning_groups.push(g);
        (st, gid, ids)
    }

    fn week_slot_picks(
        evs: &[DomainEvent],
        week_index: usize,
        num_slots: usize,
    ) -> Vec<Option<PersonId>> {
        let slot_evs: Vec<_> = evs
            .iter()
            .filter(|e| matches!(e, DomainEvent::SlotAssigned { .. }))
            .collect();
        (0..num_slots)
            .map(|si| match slot_evs[week_index * num_slots + si] {
                DomainEvent::SlotAssigned { person_id, .. } => person_id.clone(),
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn one_member_two_slots_leaves_the_second_slot_unassigned_not_double_booked() {
        let (st, _gid, ids) = make_group("Floor", 1, 2);
        let evs = materialize(&st, 1);
        let week0 = week_slot_picks(&evs, 0, 2);
        assert_eq!(week0[0], Some(ids[0].clone()), "the one member gets slot 0");
        assert_eq!(
            week0[1], None,
            "slot 1 must stay unassigned, not double-book P0"
        );
    }

    #[test]
    fn two_members_three_slots_leaves_the_third_slot_unassigned_not_double_booked() {
        let (st, _gid, ids) = make_group("Floor", 2, 3);
        let evs = materialize(&st, 1);
        let week0 = week_slot_picks(&evs, 0, 3);
        assert_eq!(week0[0], Some(ids[0].clone()));
        assert_eq!(week0[1], Some(ids[1].clone()));
        assert_eq!(
            week0[2], None,
            "no third distinct member — must not repeat P0"
        );

        // Next week: with only 2 people ever available, both are drawn
        // again (2 picks exactly returns a 2-length queue to its starting
        // order) — same pairing, still no repeat *within* the week, and
        // still no third pick.
        let (st, _gid, ids) = make_group("Floor", 2, 3);
        let evs = materialize(&st, 2);
        let week1 = week_slot_picks(&evs, 1, 3);
        assert_eq!(week1[0], Some(ids[0].clone()));
        assert_eq!(week1[1], Some(ids[1].clone()));
        assert_eq!(week1[2], None);
    }

    #[test]
    fn two_members_two_slots_and_three_members_two_slots_never_double_book() {
        // 2 members / 2 slots: exact fit, both slots filled, no None.
        let (st, ..) = make_group("Floor", 2, 2);
        let evs = materialize(&st, 1);
        let week0 = week_slot_picks(&evs, 0, 2);
        assert!(week0.iter().all(|p| p.is_some()));
        assert_ne!(week0[0], week0[1], "must not double-book the same person");

        // 3 members / 2 slots: surplus member, both slots filled, no repeats within the week.
        let (st, ..) = make_group("Floor", 3, 2);
        let evs = materialize(&st, 1);
        let week0 = week_slot_picks(&evs, 0, 2);
        assert!(week0.iter().all(|p| p.is_some()));
        assert_ne!(week0[0], week0[1]);
    }

    #[test]
    fn repeated_materialize_and_apply_with_the_same_horizon_is_a_no_op() {
        // Simulates dashboard refreshes / repeated bot restarts hitting the
        // same already-materialized range: must never advance the queue.
        let (mut st, id1, id2) = two_person_state();
        for ev in materialize(&st, 4) {
            st.apply_event(ev).unwrap();
        }
        let queue_after_first = st.cleaning_groups[0].rotation_queue.clone();
        let assignments_after_first: Vec<_> = st
            .slot_assignments
            .iter()
            .map(|a| (a.iso_week, a.person_id.clone()))
            .collect();

        // "Refresh" three more times with the identical horizon.
        for _ in 0..3 {
            let evs = materialize(&st, 4);
            assert!(
                evs.is_empty(),
                "re-materializing the same horizon must produce zero events"
            );
            for ev in evs {
                st.apply_event(ev).unwrap();
            }
        }

        assert_eq!(
            st.cleaning_groups[0].rotation_queue, queue_after_first,
            "queue must not drift"
        );
        let assignments_after: Vec<_> = st
            .slot_assignments
            .iter()
            .map(|a| (a.iso_week, a.person_id.clone()))
            .collect();
        assert_eq!(assignments_after, assignments_after_first);
        let _ = (id1, id2);
    }

    #[test]
    fn reconcile_continues_correctly_with_a_realistic_mix_of_past_and_future_assignments() {
        // A production-like state: 2 already-completed past weeks and 2
        // already-materialized future weeks for a 3-person group, then
        // `rotation_queue` is cleared to simulate a pre-queue upgrade.
        let (mut st, gid, ids) = make_group("Floor", 3, 0);
        let (cur_y, cur_w) = crate::state::current_iso_week();
        let weeks: Vec<(i32, u32)> = (-2..2i64).map(|n| add_weeks(cur_y, cur_w, n)).collect();
        for (i, &(y, w)) in weeks.iter().enumerate() {
            st.slot_assignments.push(crate::domain::SlotAssignment {
                group_id: gid.clone(),
                slot_index: 0,
                iso_year: y,
                iso_week: w,
                shift: 0,
                person_id: Some(ids[i % 3].clone()),
                source: Default::default(),
            });
        }
        st.cleaning_groups[0].rotation_queue.clear();

        // 4 prior assignments (2 past, 2 future — reconcile doesn't
        // distinguish), 3 members → shift by 1 → queue continues at P1.
        let queue = reconcile_queue(&st, &st.cleaning_groups[0]);
        assert_eq!(queue, vec![ids[1].clone(), ids[2].clone(), ids[0].clone()]);

        // Materializing forward must not touch the already-frozen weeks and
        // must be deterministic across repeated calls.
        let a = materialize(&st, 4);
        let b = materialize(&st, 4);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        for ev in a {
            if let DomainEvent::SlotAssigned {
                iso_year, iso_week, ..
            } = &ev
            {
                assert!(
                    weeks.iter().all(|&(y, w)| (*iso_year, *iso_week) != (y, w)),
                    "must not re-touch an already-frozen week"
                );
            }
        }
    }

    #[test]
    fn preview_beyond_the_horizon_agrees_with_materialize() {
        let (st, id1, id2) = two_person_state();
        // Week 5 is beyond what's been materialized (nothing has yet).
        let (y, w) = add_weeks(current_iso_week().0, current_iso_week().1, 4);
        let group = &st.cleaning_groups[0];
        let preview = preview_slot_assignee(&st, group, 0, Turn::new(y, w, 0));

        let evs = materialize(&st, 5);
        let expected = evs
            .iter()
            .find_map(|e| match e {
                DomainEvent::SlotAssigned {
                    iso_year,
                    iso_week,
                    shift: 0,
                    person_id,
                    ..
                } if *iso_year == y && *iso_week == w => Some(person_id.clone()),
                _ => None,
            })
            .flatten();
        assert_eq!(preview, expected);
        assert!(preview == Some(id1) || preview == Some(id2));
    }

    // ── !member away eligibility ──────────────────────────────────────────────

    fn absence(
        person_id: &PersonId,
        group_id: &str,
        from: (i32, u32),
        duration_weeks: u32,
    ) -> crate::state::Absence {
        crate::state::Absence {
            person_id: person_id.clone(),
            group_id: group_id.to_owned(),
            from_year: from.0,
            from_week: from.1,
            duration_weeks,
        }
    }

    #[test]
    fn absent_member_is_skipped_but_keeps_their_queue_position() {
        // Queue: Anna → Bob → Carla. Anna is absent for the due week.
        let (mut st, gid, ids) = make_group("Floor", 3, 0);
        let (anna, bob, carla) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
        let due = current_iso_week();
        st.absences.push(absence(&anna, &gid, due, 1));

        let evs = materialize(&st, 1);
        let picked = week_slot_picks(&evs, 0, 1);
        assert_eq!(
            picked[0],
            Some(bob.clone()),
            "Bob is next in line after Anna is skipped"
        );

        for ev in evs {
            st.apply_event(ev).unwrap();
        }
        assert_eq!(
            st.cleaning_groups[0].rotation_queue,
            vec![anna, carla, bob],
            "Anna keeps her place at the front instead of losing her turn or being pushed to the back"
        );
    }

    #[test]
    fn returning_from_absence_continues_the_rotation_fairly() {
        // Anna is absent for weeks 1-2, back for week 3. Over three weeks
        // everyone should clean exactly once — Anna's own turn deferred to
        // her return, never lost, never doubled up.
        let (mut st, gid, ids) = make_group("Floor", 3, 0);
        let (anna, bob, carla) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
        let due = current_iso_week();
        st.absences.push(absence(&anna, &gid, due, 2));

        let evs = materialize(&st, 3);
        let picks: Vec<_> = (0..3)
            .map(|i| week_slot_picks(&evs, i, 1)[0].clone())
            .collect();
        assert_eq!(picks, vec![Some(bob), Some(carla), Some(anna)]);
    }

    #[test]
    fn several_members_absent_at_once_are_all_skipped() {
        let (mut st, gid, ids) = make_group("Floor", 3, 0);
        let (anna, bob, carla) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
        let due = current_iso_week();
        st.absences.push(absence(&anna, &gid, due, 1));
        st.absences.push(absence(&bob, &gid, due, 1));

        let evs = materialize(&st, 1);
        assert_eq!(
            week_slot_picks(&evs, 0, 1),
            vec![Some(carla)],
            "only Carla is eligible"
        );
    }

    #[test]
    fn all_members_absent_leaves_the_slot_unassigned_not_a_fallback_pick() {
        let (mut st, gid, ids) = make_group("Floor", 2, 0);
        let due = current_iso_week();
        for id in &ids {
            st.absences.push(absence(id, &gid, due, 1));
        }

        let evs = materialize(&st, 1);
        assert_eq!(
            week_slot_picks(&evs, 0, 1),
            vec![None],
            "nobody eligible — must stay unassigned, not fall back to an absent member"
        );
        // Nobody was actually drawn, so the queue is untouched — no RotationQueueSet.
        assert!(!evs
            .iter()
            .any(|e| matches!(e, DomainEvent::RotationQueueSet { .. })));
    }

    #[test]
    fn multi_slot_with_one_member_absent_does_not_double_book_the_rest() {
        // 3 members, 2 slots, one absent: the other two fill both slots —
        // no repeat, and the absent member is never drawn as a fallback.
        let (mut st, gid, ids) = make_group("Floor", 3, 2);
        let (anna, bob, carla) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
        let due = current_iso_week();
        st.absences.push(absence(&anna, &gid, due, 1));

        let evs = materialize(&st, 1);
        let picked = week_slot_picks(&evs, 0, 2);
        assert!(
            picked.iter().all(|p| p.is_some()),
            "both slots must be filled from the remaining two members"
        );
        assert_ne!(picked[0], picked[1], "must not double-book the same person");
        assert!(
            picked.iter().all(|p| p != &Some(anna.clone())),
            "the absent member must never be drawn"
        );
        let picked_set: std::collections::HashSet<_> = picked.into_iter().flatten().collect();
        assert_eq!(picked_set, std::collections::HashSet::from([bob, carla]));
    }

    #[test]
    fn materialize_with_absences_is_deterministic_across_repeated_runs() {
        // Same guarantee as `materialize_is_deterministic`, but exercising
        // the absence-skip path — matters for restart/event-replay, which
        // must reproduce the identical queue and assignments every time.
        let (mut st, gid, ids) = make_group("Floor", 3, 0);
        let due = current_iso_week();
        st.absences.push(absence(&ids[0], &gid, due, 2));

        let a = materialize(&st, 6);
        let b = materialize(&st, 6);
        assert_eq!(
            format!("{a:?}"),
            format!("{b:?}"),
            "must be a pure function of state"
        );

        for ev in a {
            st.apply_event(ev).unwrap();
        }
        let queue_after_first_pass = st.cleaning_groups[0].rotation_queue.clone();
        let assignments_after_first_pass: Vec<_> = st
            .slot_assignments
            .iter()
            .map(|a| (a.iso_week, a.person_id.clone()))
            .collect();

        // Simulate a restart: materialize again over the same now-frozen
        // horizon (idempotent — every slot is already stored).
        let replay_evs = materialize(&st, 6);
        assert!(
            replay_evs.is_empty(),
            "already-frozen weeks must not be revisited on a restart"
        );
        assert_eq!(st.cleaning_groups[0].rotation_queue, queue_after_first_pass);
        let assignments_after_replay: Vec<_> = st
            .slot_assignments
            .iter()
            .map(|a| (a.iso_week, a.person_id.clone()))
            .collect();
        assert_eq!(assignments_after_first_pass, assignments_after_replay);
    }
}
