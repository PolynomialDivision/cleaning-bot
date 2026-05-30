//! Slot assignment resolver.
//!
//! `materialize` computes frozen assignments for future due weeks and returns
//! them as `SlotAssigned` events.  It is a pure function — reads state, returns
//! events to apply.  Idempotent: already-assigned weeks are skipped.
//!
//! # Round-robin formula
//!
//! For a group with members [A, B, C…] and `S` slots:
//!   slot index `s`, prior assignment count `n`  →  member[(n × S + s) % N]
//!
//! `n` = number of previously assigned (non-None) entries in `slot_assignments`
//! for this (group, slot_index) pair.  Counting stored history rather than
//! computing from the calendar date means the rotation is stable when people
//! join or leave: the count continues from where it left off, using the current
//! member list.

use crate::{
    analytics::DomainEvent,
    domain::AssignmentSource,
    state::{State, add_weeks, current_iso_week, weeks_between},
};

/// Fill all unassigned future slots for the next `weeks_ahead` due cycles.
///
/// Returns `SlotAssigned` events ready to hand to `state.apply_event`.
/// Already-stored assignments are skipped so this is safe to call repeatedly.
pub fn materialize(state: &State, interval: u32, weeks_ahead: usize) -> Vec<DomainEvent> {
    if weeks_ahead == 0 { return vec![]; }

    let (cur_y, cur_w) = current_iso_week();
    let (start_y, start_w) = state.tracking_start();
    let iv = interval as i64;

    // First due week >= today.
    let elapsed = weeks_between((start_y, start_w), (cur_y, cur_w));
    let first_due = if elapsed < 0 {
        (start_y, start_w)
    } else {
        let past = elapsed / iv;
        if elapsed % iv == 0 { (cur_y, cur_w) } else { add_weeks(start_y, start_w, (past + 1) * iv) }
    };

    let mut events: Vec<DomainEvent> = Vec::new();

    for group in &state.cleaning_groups {
        if group.member_ids.is_empty() { continue; }
        let num_slots = group.slots.len().max(1);

        for i in 0..weeks_ahead as i64 {
            let (dy, dw) = add_weeks(first_due.0, first_due.1, i * iv);

            for si in 0..num_slots {
                // Skip if already frozen.
                if state.slot_assignments.iter().any(|a| {
                    a.group_id == group.id && a.slot_index == si
                        && a.iso_year == dy && a.iso_week == dw
                }) {
                    continue;
                }

                // Count prior non-None assignments for this (group, slot_index)
                // including ones we've already queued in this batch.
                let stored = state.slot_assignments.iter()
                    .filter(|a| a.group_id == group.id && a.slot_index == si && a.person_id.is_some())
                    .count();
                let queued = events.iter()
                    .filter(|e| matches!(e,
                        DomainEvent::SlotAssigned { group_id, slot_index, person_id: Some(_), .. }
                        if group_id == &group.id && *slot_index == si
                    ))
                    .count();
                let n = stored + queued;

                let idx = (n * num_slots + si) % group.member_ids.len();
                let person_id = group.member_ids.get(idx).cloned();

                events.push(DomainEvent::SlotAssigned {
                    group_id:   group.id.clone(),
                    slot_index: si,
                    iso_year:   dy,
                    iso_week:   dw,
                    person_id,
                    source:     AssignmentSource::RoundRobin,
                });
            }
        }
    }

    events
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crate::{domain::{CleaningGroup, CleaningSlot, Person}, state::State};

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
        let evs = materialize(&st, 1, 2);
        assert_eq!(evs.len(), 2);
        // Cycle 1 → Alice (index 0), Cycle 2 → Bob (index 1).
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
        let first_evs = materialize(&st, 1, 1);
        for ev in &first_evs { st.apply_event(ev.clone()).unwrap(); }
        // Running materialize again should produce no new events for week 1.
        let second_evs = materialize(&st, 1, 1);
        assert!(second_evs.is_empty(), "should skip already-stored assignment");
        // The stored assignment should still be Alice.
        let stored = &st.slot_assignments[0];
        assert_eq!(stored.person_id.as_deref(), Some(id1.as_str()));
    }

    #[test]
    fn rotation_stable_after_member_leaves() {
        let (mut st, id1, id2) = two_person_state();
        // Materialize week 1 (Alice) and week 2 (Bob).
        let evs = materialize(&st, 1, 2);
        for ev in evs { st.apply_event(ev).unwrap(); }

        // Alice leaves.
        let gid = st.cleaning_groups[0].id.clone();
        st.apply_event(DomainEvent::PersonLeftGroup { person_id: id1.clone(), group_id: gid }).unwrap();
        // Unassign Alice's future stored assignment (week 2 belongs to Bob, not Alice).
        // (Alice had week 1 stored — that's in the past and stays.)

        // Materialize week 3: only Bob remains; stored count for slot0 = 2 (Alice + Bob).
        // (2 * 1 + 0) % 1 = 0 → member[0] = Bob (only member left).
        let new_evs = materialize(&st, 1, 3);
        // Should only produce week 3 (weeks 1+2 already stored).
        let unfrozen: Vec<_> = new_evs.iter()
            .filter(|e| matches!(e, DomainEvent::SlotAssigned { .. }))
            .collect();
        assert!(!unfrozen.is_empty());
        if let DomainEvent::SlotAssigned { person_id, .. } = &unfrozen[0] {
            assert_eq!(person_id.as_deref(), Some(id2.as_str()), "Bob should be next");
        }
    }

    #[test]
    fn multi_slot_round_robin() {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let people: Vec<Person> = (0..4).map(|i| Person::new_named(&format!("P{i}"))).collect();
        let ids: Vec<_> = people.iter().map(|p| p.id.clone()).collect();
        st.persons.extend(people);

        let mut g = CleaningGroup::new("Floor");
        g.member_ids.extend(ids.clone());
        let mut s0 = CleaningSlot::new("Scharni"); s0.id = "s0".into();
        let mut s1 = CleaningSlot::new("Colbe");   s1.id = "s1".into();
        g.slots.extend([s0, s1]);
        st.cleaning_groups.push(g);

        let evs = materialize(&st, 1, 2);
        // 2 weeks × 2 slots = 4 events.
        assert_eq!(evs.len(), 4);
        // Week1/slot0 → P0 (0*2+0=0), Week1/slot1 → P1 (0*2+1=1),
        // Week2/slot0 → P2 (1*2+0=2), Week2/slot1 → P3 (1*2+1=3).
        let week1_slot0 = &evs[0];
        let week1_slot1 = &evs[1];
        if let (DomainEvent::SlotAssigned { person_id: p0, .. }, DomainEvent::SlotAssigned { person_id: p1, .. }) = (week1_slot0, week1_slot1) {
            assert_eq!(p0.as_deref(), Some(ids[0].as_str()));
            assert_eq!(p1.as_deref(), Some(ids[1].as_str()));
        }
    }
}
