//! Schedule build pipeline — single source of truth for all exports.
//!
//! `build_schedule()` is a pure function over immutable state.
//! All exports (PDF, ICS, Matrix messages) must derive from a `ScheduleSnapshot`.
//!
//! Determinism guarantee:
//! Given identical state.json content and identical `weeks` count,
//! `build_schedule()` always returns structurally identical output.
//! The only non-deterministic field is `generated_at`, which is set to
//! `state.last_modified` (or `state.created_at`) so it is also stable for
//! unchanged state.

use crate::{
    domain::{assignment_uid, slot_assignment_uid, GroupId, PersonId, SlotId},
    state::{add_weeks, current_iso_week, State},
};
use chrono::{NaiveDate, Utc};

// ── Sub-types ─────────────────────────────────────────────────────────────────

/// Resolved person details, pre-fetched so export code needs no further lookups.
#[derive(Clone, Debug, PartialEq)]
pub struct PersonDetails {
    pub id: PersonId,
    pub name: String,
    pub mxid: Option<String>,
}

/// A single resolved assignment: one person responsible for one group (or slot) in one week.
#[derive(Clone, Debug)]
pub struct AssignmentInstance {
    /// Stable UUID v5 — identical for (group_id × [slot_id ×] year × week × assignee_id).
    pub uid: String,
    pub group_id: GroupId,
    pub group_name: String,
    /// `Some` for multi-slot groups; `None` for single-slot groups.
    #[allow(dead_code)]
    pub slot_id: Option<SlotId>,
    pub slot_name: Option<String>,
    pub room_names: Vec<String>,
    pub iso_year: i32,
    pub iso_week: u32,
    /// Shift within the week (see `rhythm`).
    #[allow(dead_code)]
    pub shift: u8,
    /// First and last day of the turn.
    pub start: NaiveDate,
    pub end: NaiveDate,
    /// "22 – 28 Sep", or "Thu–Sun 25 – 28 Sep" for a shift.
    pub period_label: String,
    /// "Mon–Wed" for a group split into shifts, else `None`.
    pub shift_label: Option<String>,
    /// The group's rhythm, e.g. "weekly" or "2× per week (Mon–Wed, Thu–Sun)".
    pub rhythm: String,
    /// `None` means the group/slot exists but has no members this cycle.
    pub assignee: Option<PersonDetails>,
    pub is_completed: bool,
    pub is_skipped: bool,
    /// Display name of whoever marked it done (may differ from assignee).
    pub completed_by: Option<String>,
    /// Date the cleaning was marked done (for the PDF Date column).
    pub completed_at: Option<chrono::NaiveDate>,
}

impl AssignmentInstance {
    /// The assignee display name, or "(nobody assigned)" fallback.
    pub fn assignee_name(&self) -> &str {
        self.assignee
            .as_ref()
            .map(|p| p.name.as_str())
            .unwrap_or("(nobody assigned)")
    }

    /// The MXID of the assignee if they have one, else `None`.
    pub fn assignee_mxid(&self) -> Option<&str> {
        self.assignee.as_ref().and_then(|p| p.mxid.as_deref())
    }
}

// ── Snapshot ──────────────────────────────────────────────────────────────────

/// Immutable, fully-resolved snapshot of the cleaning schedule over a time range.
/// All exports are pure functions over this type.
#[derive(Clone, Debug)]
pub struct ScheduleSnapshot {
    /// Timestamp from state.last_modified (or state.created_at).
    /// Deterministic for unchanged state — used as DTSTAMP in ICS.
    pub state_timestamp: chrono::DateTime<Utc>,
    pub assignments: Vec<AssignmentInstance>,
}

impl ScheduleSnapshot {
    /// All assignments where `person_id` is the assignee, in chronological order.
    pub fn for_person(&self, person_id: &PersonId) -> Vec<&AssignmentInstance> {
        self.assignments
            .iter()
            .filter(|a| {
                a.assignee
                    .as_ref()
                    .map(|p| &p.id == person_id)
                    .unwrap_or(false)
            })
            .collect()
    }

    /// All assignments for a specific group, in chronological order.
    #[allow(dead_code)]
    pub fn for_group(&self, group_id: &GroupId) -> Vec<&AssignmentInstance> {
        self.assignments
            .iter()
            .filter(|a| &a.group_id == group_id)
            .collect()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.assignments.is_empty()
    }

    /// All assignments for a given (year, week) pair.
    pub fn for_group_in_week(&self, year: i32, week: u32) -> Vec<&AssignmentInstance> {
        self.assignments
            .iter()
            .filter(|a| a.iso_year == year && a.iso_week == week)
            .collect()
    }

    /// Unique (iso_year, iso_week) pairs in order.
    pub fn weeks(&self) -> Vec<(i32, u32)> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for a in &self.assignments {
            if seen.insert((a.iso_year, a.iso_week)) {
                result.push((a.iso_year, a.iso_week));
            }
        }
        result
    }
}

// ── Build ─────────────────────────────────────────────────────────────────────

/// Build a deterministic schedule snapshot of the next `weeks` calendar
/// weeks (from the current one): every turn of every active group in that
/// range — each group in its own rhythm, one entry per slot of each turn —
/// ordered by turn start, then group.
///
/// Does NOT mutate state. Reads completions for status, then returns a fully
/// resolved, immutable snapshot.
pub fn build_schedule(state: &State, weeks: usize) -> ScheduleSnapshot {
    let first = current_iso_week();
    let last = add_weeks(first.0, first.1, weeks.max(1) as i64 - 1);
    let mut assignments = Vec::new();

    for group in state.cleaning_groups.iter().filter(|g| g.is_active) {
        for turn in state.turns_between(group, first, last) {
            let (start, end) = turn.dates(&group.rhythm);
            for slot_index in State::slot_indices(group) {
                let slot = group.slots.get(slot_index);
                let assignee =
                    state
                        .slot_assignee(group, slot_index, turn)
                        .map(|p| PersonDetails {
                            id: p.id.clone(),
                            name: p.display_name.clone(),
                            mxid: p.matrix_id.clone(),
                        });
                let person_part = assignee.as_ref().map(|p| p.id.as_str()).unwrap_or("none");
                // Whole-week turns keep the UID they always had, so existing
                // calendar subscriptions don't see every event replaced.
                let uid = match (slot, turn.shift) {
                    (Some(slot), 0) => {
                        slot_assignment_uid(&group.id, &slot.id, turn.year, turn.week, person_part)
                    }
                    (None, 0) => assignment_uid(&group.id, turn.year, turn.week, person_part),
                    (slot, shift) => slot_assignment_uid(
                        &group.id,
                        &format!("{}#{shift}", slot.map_or("", |s| s.id.as_str())),
                        turn.year,
                        turn.week,
                        person_part,
                    ),
                };

                let completion = state.completion_for(group, slot_index, turn);
                assignments.push(AssignmentInstance {
                    uid,
                    group_id: group.id.clone(),
                    group_name: group.name.clone(),
                    slot_id: slot.map(|s| s.id.clone()),
                    slot_name: slot.map(|s| s.name.clone()),
                    room_names: slot
                        .map_or_else(|| group.room_names.clone(), |s| s.room_names.clone()),
                    iso_year: turn.year,
                    iso_week: turn.week,
                    shift: turn.shift,
                    start,
                    end,
                    period_label: turn.period_label(&group.rhythm),
                    shift_label: turn.shift_label(&group.rhythm),
                    rhythm: group.rhythm.describe(),
                    assignee,
                    is_completed: completion.is_some(),
                    is_skipped: completion.is_some_and(|c| c.skipped),
                    completed_by: completion
                        .and_then(|c| state.person_by_id(&c.completed_by_id))
                        .map(|p| p.display_name.clone()),
                    completed_at: completion
                        .filter(|c| !c.skipped)
                        .map(|c| c.completed_at.date_naive()),
                });
            }
        }
    }
    let order: Vec<&GroupId> = state.cleaning_groups.iter().map(|g| &g.id).collect();
    assignments.sort_by_key(|a| {
        (
            a.start,
            order.iter().position(|id| **id == a.group_id),
            a.slot_name.clone(),
        )
    });

    // Use state timestamp for deterministic DTSTAMP in ICS.
    let state_timestamp = state
        .last_modified
        .or(state.created_at)
        .unwrap_or_else(Utc::now);

    ScheduleSnapshot {
        state_timestamp,
        assignments,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CleaningGroup, Person},
        state::State,
    };

    fn simple_state() -> State {
        let mut st = State::default();
        st.created_at = Some(Utc::now());

        let p = Person::new_matrix("@alice:example.org");
        let pid = p.id.clone();
        st.persons.push(p);

        let mut g = CleaningGroup::new("Kitchen");
        g.member_ids.push(pid);
        st.cleaning_groups.push(g);
        st
    }

    #[test]
    fn snapshot_is_deterministic() {
        let st = simple_state();
        let s1 = build_schedule(&st, 4);
        let s2 = build_schedule(&st, 4);

        assert_eq!(s1.assignments.len(), s2.assignments.len());
        for (a, b) in s1.assignments.iter().zip(s2.assignments.iter()) {
            assert_eq!(a.uid, b.uid, "UIDs must be stable");
            assert_eq!(a.iso_year, b.iso_year);
            assert_eq!(a.iso_week, b.iso_week);
            assert_eq!(a.group_id, b.group_id);
        }
    }

    #[test]
    fn uid_is_stable_across_rebuilds() {
        let st = simple_state();
        let s1 = build_schedule(&st, 2);
        let s2 = build_schedule(&st, 2);
        assert_eq!(s1.assignments[0].uid, s2.assignments[0].uid);
    }

    #[test]
    fn state_timestamp_is_deterministic() {
        let st = simple_state();
        let s1 = build_schedule(&st, 2);
        let s2 = build_schedule(&st, 2);
        assert_eq!(s1.state_timestamp, s2.state_timestamp);
    }

    #[test]
    fn no_assignee_when_group_is_empty() {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        st.cleaning_groups.push(CleaningGroup::new("EmptyGroup"));

        let snap = build_schedule(&st, 1);
        assert_eq!(snap.assignments.len(), 1);
        assert!(snap.assignments[0].assignee.is_none());
    }

    #[test]
    fn for_person_filters_correctly() {
        let st = simple_state();
        let snap = build_schedule(&st, 4);
        let person_id = st.persons[0].id.clone();
        let filtered = snap.for_person(&person_id);
        assert_eq!(
            filtered.len(),
            snap.assignments
                .iter()
                .filter(|a| a.assignee.is_some())
                .count()
        );
    }
}
