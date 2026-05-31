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

use chrono::{NaiveDate, Utc, Weekday};
use crate::{
    domain::{assignment_uid, slot_assignment_uid, GroupId, PersonId, SlotId},
    state::{State, add_weeks, current_iso_week, week_dates, weeks_between},
};

// ── Sub-types ─────────────────────────────────────────────────────────────────

/// Resolved person details, pre-fetched so export code needs no further lookups.
#[derive(Clone, Debug, PartialEq)]
pub struct PersonDetails {
    pub id:   PersonId,
    pub name: String,
    pub mxid: Option<String>,
}

/// A single resolved assignment: one person responsible for one group (or slot) in one week.
#[derive(Clone, Debug)]
pub struct AssignmentInstance {
    /// Stable UUID v5 — identical for (group_id × [slot_id ×] year × week × assignee_id).
    pub uid:         String,
    pub group_id:    GroupId,
    pub group_name:  String,
    /// `Some` for multi-slot groups; `None` for single-slot groups.
    #[allow(dead_code)]
    pub slot_id:     Option<SlotId>,
    pub slot_name:   Option<String>,
    pub room_names:  Vec<String>,
    pub iso_year:    i32,
    pub iso_week:    u32,
    pub week_monday: NaiveDate,
    pub week_sunday: NaiveDate,
    /// Pre-formatted "1–7 Jun" style string.
    pub week_label:  String,
    /// `None` means the group/slot exists but has no members this cycle.
    pub assignee:    Option<PersonDetails>,
    pub is_completed:  bool,
    pub is_skipped:    bool,
    /// Display name of whoever marked it done (may differ from assignee).
    pub completed_by:  Option<String>,
    /// Date the cleaning was marked done (for the PDF Date column).
    pub completed_at:  Option<chrono::NaiveDate>,
}

impl AssignmentInstance {
    /// The assignee display name, or "(nobody assigned)" fallback.
    pub fn assignee_name(&self) -> &str {
        self.assignee.as_ref().map(|p| p.name.as_str()).unwrap_or("(nobody assigned)")
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
    pub state_timestamp:  chrono::DateTime<Utc>,
    pub interval_weeks:   u32,
    pub assignments:      Vec<AssignmentInstance>,
}

impl ScheduleSnapshot {
    /// All assignments where `person_id` is the assignee, in chronological order.
    pub fn for_person(&self, person_id: &PersonId) -> Vec<&AssignmentInstance> {
        self.assignments.iter()
            .filter(|a| a.assignee.as_ref().map(|p| &p.id == person_id).unwrap_or(false))
            .collect()
    }

    /// All assignments for a specific group, in chronological order.
    #[allow(dead_code)]
    pub fn for_group(&self, group_id: &GroupId) -> Vec<&AssignmentInstance> {
        self.assignments.iter().filter(|a| &a.group_id == group_id).collect()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool { self.assignments.is_empty() }

    /// All assignments for a given (year, week) pair.
    pub fn for_group_in_week(&self, year: i32, week: u32) -> Vec<&AssignmentInstance> {
        self.assignments.iter().filter(|a| a.iso_year == year && a.iso_week == week).collect()
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

/// Build a deterministic schedule snapshot covering the next `weeks` due cycles.
///
/// Starting point is the first due week >= today, aligned to `state.tracking_start()`
/// by stepping `interval_weeks` at a time.
///
/// Does NOT mutate state. Reads completions for status, then returns a fully
/// resolved, immutable snapshot.
pub fn build_schedule(state: &State, interval: u32, weeks: usize) -> ScheduleSnapshot {
    let (cur_y, cur_w)     = current_iso_week();
    let (start_y, start_w) = state.tracking_start();
    let iv = interval as i64;

    // First due week >= current week.
    let elapsed = weeks_between((start_y, start_w), (cur_y, cur_w));
    let first_due = if elapsed < 0 {
        (start_y, start_w)
    } else {
        let past = elapsed / iv;
        if elapsed % iv == 0 { (cur_y, cur_w) } else { add_weeks(start_y, start_w, (past + 1) * iv) }
    };

    let mut assignments = Vec::with_capacity(weeks * state.cleaning_groups.len());

    for i in 0..(weeks as i64) {
        let (dy, dw) = add_weeks(first_due.0, first_due.1, i * iv);
        let monday   = NaiveDate::from_isoywd_opt(dy, dw, Weekday::Mon)
            .unwrap_or_else(|| NaiveDate::from_ymd_opt(dy, 1, 4).unwrap());
        let sunday   = monday + chrono::Duration::days(6);

        for group in &state.cleaning_groups {
            if group.is_multi_slot() {
                // Emit one AssignmentInstance per slot.
                for (slot_idx, slot) in group.slots.iter().enumerate() {
                    let assignee = state.slot_assignee(group, slot_idx, dy, dw, interval).map(|p| PersonDetails {
                        id:   p.id.clone(),
                        name: p.display_name.clone(),
                        mxid: p.matrix_id.clone(),
                    });
                    let person_part = assignee.as_ref().map(|p| p.id.as_str()).unwrap_or("none");
                    let uid = slot_assignment_uid(&group.id, &slot.id, dy, dw, person_part);

                    let completion = state.completions.iter().find(|c| {
                        c.group_id == group.id && c.slot_id.as_deref() == Some(&slot.id) && c.iso_year == dy && c.iso_week == dw
                    });
                    let is_completed = completion.is_some();
                    let is_skipped   = completion.map(|c| c.skipped).unwrap_or(false);
                    let completed_by = completion
                        .and_then(|c| state.person_by_id(&c.completed_by_id))
                        .map(|p| p.display_name.clone());
                    let completed_at = completion
                        .filter(|c| !c.skipped)
                        .map(|c| c.completed_at.date_naive());

                    assignments.push(AssignmentInstance {
                        uid,
                        group_id:    group.id.clone(),
                        group_name:  group.name.clone(),
                        slot_id:     Some(slot.id.clone()),
                        slot_name:   Some(slot.name.clone()),
                        room_names:  slot.room_names.clone(),
                        iso_year:    dy,
                        iso_week:    dw,
                        week_monday: monday,
                        week_sunday: sunday,
                        week_label:  week_dates(dy, dw),
                        assignee,
                        is_completed,
                        is_skipped,
                        completed_by,
                        completed_at,
                    });
                }
            } else {
                // Single-slot: existing behaviour.
                let assignee = state.responsible_person(group, dy, dw, interval).map(|p| PersonDetails {
                    id:   p.id.clone(),
                    name: p.display_name.clone(),
                    mxid: p.matrix_id.clone(),
                });
                let person_part = assignee.as_ref().map(|p| p.id.as_str()).unwrap_or("none");
                let uid = assignment_uid(&group.id, dy, dw, person_part);

                let completion = state.completions.iter()
                    .find(|c| c.group_id == group.id && c.slot_id.is_none() && c.iso_year == dy && c.iso_week == dw);
                let is_completed = completion.is_some();
                let is_skipped   = completion.map(|c| c.skipped).unwrap_or(false);
                let completed_by = completion
                    .and_then(|c| state.person_by_id(&c.completed_by_id))
                    .map(|p| p.display_name.clone());
                let completed_at = completion
                    .filter(|c| !c.skipped)
                    .map(|c| c.completed_at.date_naive());

                assignments.push(AssignmentInstance {
                    uid,
                    group_id:    group.id.clone(),
                    group_name:  group.name.clone(),
                    slot_id:     None,
                    slot_name:   None,
                    room_names:  group.room_names.clone(),
                    iso_year:    dy,
                    iso_week:    dw,
                    week_monday: monday,
                    week_sunday: sunday,
                    week_label:  week_dates(dy, dw),
                    assignee,
                    is_completed,
                    is_skipped,
                    completed_by,
                    completed_at,
                });
            }
        }
    }

    // Use state timestamp for deterministic DTSTAMP in ICS.
    let state_timestamp = state.last_modified
        .or(state.created_at)
        .unwrap_or_else(Utc::now);

    ScheduleSnapshot { state_timestamp, interval_weeks: interval, assignments }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{domain::{CleaningGroup, Person}, state::State};

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
        let s1 = build_schedule(&st, 1, 4);
        let s2 = build_schedule(&st, 1, 4);

        assert_eq!(s1.assignments.len(), s2.assignments.len());
        for (a, b) in s1.assignments.iter().zip(s2.assignments.iter()) {
            assert_eq!(a.uid,        b.uid,        "UIDs must be stable");
            assert_eq!(a.iso_year,   b.iso_year);
            assert_eq!(a.iso_week,   b.iso_week);
            assert_eq!(a.group_id,   b.group_id);
        }
    }

    #[test]
    fn uid_is_stable_across_rebuilds() {
        let st = simple_state();
        let s1 = build_schedule(&st, 1, 2);
        let s2 = build_schedule(&st, 1, 2);
        assert_eq!(s1.assignments[0].uid, s2.assignments[0].uid);
    }

    #[test]
    fn state_timestamp_is_deterministic() {
        let st = simple_state();
        let s1 = build_schedule(&st, 1, 2);
        let s2 = build_schedule(&st, 1, 2);
        assert_eq!(s1.state_timestamp, s2.state_timestamp);
    }

    #[test]
    fn no_assignee_when_group_is_empty() {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        st.cleaning_groups.push(CleaningGroup::new("EmptyGroup"));

        let snap = build_schedule(&st, 1, 1);
        assert_eq!(snap.assignments.len(), 1);
        assert!(snap.assignments[0].assignee.is_none());
    }

    #[test]
    fn for_person_filters_correctly() {
        let st = simple_state();
        let snap = build_schedule(&st, 1, 4);
        let person_id = st.persons[0].id.clone();
        let filtered = snap.for_person(&person_id);
        assert_eq!(filtered.len(), snap.assignments.iter().filter(|a| a.assignee.is_some()).count());
    }
}
