//! Statistics and analytics layer.
//!
//! All metrics are pure functions over immutable state — no side effects,
//! no scheduling logic, no mutations.
//!
//! # Architecture
//!
//! ## Event log
//! `LoggedEvent` records meaningful domain changes.  The log is append-only
//! and persisted in `state.json`.  `backfill_events()` reconstructs historical
//! events from existing completion / swap / absence records so the log is
//! complete from day one even on first deployment.
//!
//! ## Metric structs
//! `PersonStats`, `GroupStats`, `FairnessReport` are derived views built by
//! querying `State` directly.  They are recomputed on demand — no caching,
//! no staleness.
//!
//! ## Extensibility
//! Adding a new metric means adding a new function in this module.
//! Nothing in the scheduling or export layers needs to change.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    domain::{AssignmentSource, GroupId, PersonId, SlotId},
    state::{State, SwapStatus, all_due_weeks_in_range, current_iso_week},
};

// ── Domain event model ────────────────────────────────────────────────────────

/// Every domain state change is recorded as a `DomainEvent`.
/// Events are immutable once written — never update or delete.
///
/// `State::apply_event` is the **only** function that may mutate domain state;
/// it always appends the event to the log atomically with the mutation.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DomainEvent {
    // ── Group management ──────────────────────────────────────────────────────
    GroupCreated { group_id: GroupId, name: String },
    GroupDeleted { group_id: GroupId },
    SlotAdded    { group_id: GroupId, slot_id: SlotId, slot_name: String },
    SlotRemoved  { group_id: GroupId, slot_id: SlotId },
    /// Frozen assignment: one person (or nobody) for one slot in one week.
    /// Upserts — re-applying for the same key replaces the previous assignment.
    SlotAssigned {
        group_id:   GroupId,
        slot_index: usize,
        iso_year:   i32,
        iso_week:   u32,
        person_id:  Option<PersonId>,
        #[serde(default)]
        source:     AssignmentSource,
    },
    RoomAdded    { group_id: GroupId, #[serde(default)] slot_id: Option<SlotId>, room_name: String },
    RoomRemoved  { group_id: GroupId, #[serde(default)] slot_id: Option<SlotId>, room_name: String },

    // ── Person management ─────────────────────────────────────────────────────
    PersonCreated {
        person_id:    PersonId,
        display_name: String,
        #[serde(default)]
        matrix_id:    Option<String>,
    },
    PersonJoinedGroup  { person_id: PersonId, group_id: GroupId },
    PersonLeftGroup    { person_id: PersonId, group_id: GroupId },
    PersonMatrixLinked { person_id: PersonId, matrix_id: String },

    // ── Cleaning operations ───────────────────────────────────────────────────
    CleaningCompleted {
        group_id:               GroupId,
        /// `Some` for multi-slot groups; `None` for single-slot groups.
        #[serde(default)]
        slot_id:                Option<SlotId>,
        person_id:              PersonId,
        #[serde(default)]
        responsible_person_ids: Vec<PersonId>,
        iso_year:               i32,
        iso_week:               u32,
    },
    CleaningSkipped {
        group_id:   GroupId,
        skipper_id: PersonId,
        iso_year:   i32,
        iso_week:   u32,
    },
    CleaningUndone {
        group_id: GroupId,
        iso_year: i32,
        iso_week: u32,
    },

    // ── Swaps ─────────────────────────────────────────────────────────────────
    SwapRequested {
        group_id:       GroupId,
        requester_mxid: String,
        target_mxid:    String,
        iso_year:       i32,
        iso_week:       u32,
    },
    SwapApproved {
        swap_id:        u64,
        group_id:       GroupId,
        requester_id:   PersonId,
        replacement_id: PersonId,
        iso_year:       i32,
        iso_week:       u32,
    },
    SwapRejected { swap_id: u64 },

    // ── Absences ──────────────────────────────────────────────────────────────
    AbsenceRecorded {
        person_id:      PersonId,
        group_id:       GroupId,
        from_year:      i32,
        from_week:      u32,
        duration_weeks: u32,
    },
    /// Cancels all active absences for the given person.
    AbsenceCancelled { person_id: PersonId },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LoggedEvent {
    pub id:        String,
    pub timestamp: DateTime<Utc>,
    pub event:     DomainEvent,
}

impl LoggedEvent {
    pub fn now(event: DomainEvent) -> Self {
        LoggedEvent { id: Uuid::new_v4().to_string(), timestamp: Utc::now(), event }
    }

    /// Reconstruct a historical event with a specific timestamp.
    pub fn at(timestamp: DateTime<Utc>, event: DomainEvent) -> Self {
        LoggedEvent { id: Uuid::new_v4().to_string(), timestamp, event }
    }
}

// ── Backfill ──────────────────────────────────────────────────────────────────

/// Reconstruct the event log from existing state data.
///
/// Called once when `state.event_log` is empty but historical data exists.
/// Produces a chronologically sorted list of events derived from completions,
/// swap requests, and absences.  Person join/leave events are not
/// reconstructible from the current state and are omitted.
pub fn backfill_events(state: &State) -> Vec<LoggedEvent> {
    let mut events: Vec<LoggedEvent> = Vec::new();

    // Completions → CleaningCompleted / CleaningSkipped
    for c in &state.completions {
        let ev = if c.skipped {
            DomainEvent::CleaningSkipped {
                group_id:   c.group_id.clone(),
                skipper_id: c.completed_by_id.clone(),
                iso_year:   c.iso_year,
                iso_week:   c.iso_week,
            }
        } else {
            DomainEvent::CleaningCompleted {
                group_id:               c.group_id.clone(),
                slot_id:                c.slot_id.clone(),
                person_id:              c.completed_by_id.clone(),
                responsible_person_ids: c.responsible_person_ids.clone(),
                iso_year:               c.iso_year,
                iso_week:               c.iso_week,
            }
        };
        events.push(LoggedEvent::at(c.completed_at, ev));
    }

    // Accepted swaps → SwapApproved
    for s in &state.swap_requests {
        if s.status != SwapStatus::Accepted { continue; }
        let requester_id = state.person_by_matrix_id(&s.requester)
            .map(|p| p.id.clone()).unwrap_or_else(|| s.requester.clone());
        let replacement_id = state.person_by_matrix_id(&s.target)
            .map(|p| p.id.clone()).unwrap_or_else(|| s.target.clone());
        events.push(LoggedEvent::at(s.created_at, DomainEvent::SwapApproved {
            swap_id:        s.id,
            group_id:       s.group_id.clone(),
            requester_id,
            replacement_id,
            iso_year:       s.iso_year,
            iso_week:       s.iso_week,
        }));
    }

    // Active absences → AbsenceRecorded (timestamp unknown, use now)
    for a in &state.absences {
        events.push(LoggedEvent::now(DomainEvent::AbsenceRecorded {
            person_id:      a.person_id.clone(),
            group_id:       a.group_id.clone(),
            from_year:      a.from_year,
            from_week:      a.from_week,
            duration_weeks: a.duration_weeks,
        }));
    }

    events.sort_by_key(|e| e.timestamp);
    events
}

// ── Metric structs ────────────────────────────────────────────────────────────

/// Per-person aggregated statistics.
#[derive(Debug, Clone)]
pub struct PersonStats {
    pub person_id:       PersonId,
    pub display_name:    String,
    /// Comma-joined names of all groups this person belongs to.
    pub group_names:     String,
    /// Closed (past) due weeks in the analysis range.
    pub due_weeks:       u32,
    /// Weeks this person actually cleaned.
    pub completed:       u32,
    /// Weeks any of their groups were marked skipped.
    pub skipped:         u32,
    /// Due weeks with no completion and no skip — genuinely missed.
    pub missed:          u32,
    /// completed / (due_weeks − skipped), or 1.0 when no effective duty.
    pub completion_rate: f64,
    /// Swaps they initiated (handed their duty to someone else).
    pub swaps_given:     u32,
    /// Swaps they accepted (took someone else's duty).
    pub swaps_taken:     u32,
    /// Total weeks marked absent (across all groups).
    pub absent_weeks:    u32,
    /// Current consecutive-completed streak for their primary group.
    pub streak:          u32,
}

/// Per-group aggregated statistics.
#[derive(Debug, Clone)]
pub struct GroupStats {
    pub group_id:        GroupId,
    pub group_name:      String,
    pub member_count:    u32,
    pub due_weeks:       u32,
    pub completed:       u32,
    pub skipped:         u32,
    /// due − completed − skipped.
    pub missed:          u32,
    pub completion_rate: f64,
    pub current_streak:  u32,
    /// Total accepted swaps within this group.
    pub swap_count:      u32,
}

/// One person's share in a fairness report.
#[derive(Debug, Clone)]
pub struct FairnessEntry {
    pub person_id:    PersonId,
    pub display_name: String,
    /// Ideal cleanings = due_weeks / member_count.
    pub expected:     f64,
    /// Actual completed cleanings (skipped weeks excluded from duty).
    pub actual:       u32,
    /// actual − expected  (positive = above fair share).
    pub deviation:    f64,
}

/// Fairness distribution for an entire cleaning group.
#[derive(Debug, Clone)]
pub struct FairnessReport {
    pub group_id:       GroupId,
    pub group_name:     String,
    pub due_weeks:      u32,
    /// Sorted by deviation descending (biggest contributor first).
    pub entries:        Vec<FairnessEntry>,
    /// Gini coefficient: 0.0 = perfect equality, 1.0 = one person did everything.
    pub gini:           f64,
    /// 100 − round(gini * 100): human-readable 0–100 fairness score.
    pub fairness_score: u8,
}

// ── Query functions ───────────────────────────────────────────────────────────

/// Compute statistics for one person across all their groups.
///
/// Returns `None` if the person is not registered or not in any group.
pub fn person_stats(state: &State, person_id: &PersonId, interval: u32) -> Option<PersonStats> {
    let person = state.person_by_id(person_id)?;
    let groups = state.groups_for_person(person_id);
    if groups.is_empty() { return None; }

    let (cur_y, cur_w) = current_iso_week();
    let start = state.tracking_start();
    let all_due = all_due_weeks_in_range(start, (cur_y, cur_w), interval);
    let closed: Vec<_> = all_due.iter().copied().filter(|&(y,w)| (y,w) != (cur_y,cur_w)).collect();
    let due_weeks = closed.len() as u32;

    // Completions where this person was the one who cleaned.
    let completed = state.completions.iter()
        .filter(|c| c.completed_by_id == *person_id && !c.skipped)
        .count() as u32;

    // Skipped weeks in any of their groups.
    let skipped = closed.iter()
        .filter(|(y,w)| groups.iter().any(|g| {
            state.completions.iter().any(|c| c.group_id == g.id && c.iso_year == *y && c.iso_week == *w && c.skipped)
        }))
        .count() as u32;

    let effective_due = due_weeks.saturating_sub(skipped);
    let missed = effective_due.saturating_sub(completed);
    let completion_rate = if effective_due > 0 { (completed as f64 / effective_due as f64).min(1.0) } else { 1.0 };

    let mxid = person.matrix_id.as_deref().unwrap_or("");
    let swaps_given = state.swap_requests.iter()
        .filter(|s| s.requester == mxid && s.status == SwapStatus::Accepted)
        .count() as u32;
    let swaps_taken = state.swap_requests.iter()
        .filter(|s| s.target == mxid && s.status == SwapStatus::Accepted)
        .count() as u32;
    let absent_weeks: u32 = state.absences.iter()
        .filter(|a| a.person_id == *person_id)
        .map(|a| a.duration_weeks).sum();

    let primary_group = groups[0];
    let streak = state.streak_for(&primary_group.id, interval);

    let group_names = groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>().join(", ");

    Some(PersonStats {
        person_id:       person_id.clone(),
        display_name:    person.display_name.clone(),
        group_names,
        due_weeks,
        completed,
        skipped,
        missed,
        completion_rate,
        swaps_given,
        swaps_taken,
        absent_weeks,
        streak,
    })
}

/// Compute statistics for a cleaning group.
pub fn group_stats(state: &State, group_id: &GroupId, interval: u32) -> Option<GroupStats> {
    let group = state.group_by_id(group_id)?;
    let (cur_y, cur_w) = current_iso_week();
    let start = state.tracking_start();
    let all_due = all_due_weeks_in_range(start, (cur_y, cur_w), interval);
    let closed: Vec<_> = all_due.iter().copied().filter(|&(y,w)| (y,w) != (cur_y,cur_w)).collect();
    let due_weeks = closed.len() as u32;

    let completed = closed.iter().filter(|(y,w)| state.is_cleaned(group_id, *y, *w)).count() as u32;
    let skipped   = closed.iter().filter(|(y,w)| {
        state.is_completed(group_id, *y, *w) && !state.is_cleaned(group_id, *y, *w)
    }).count() as u32;
    let eff = due_weeks.saturating_sub(skipped);
    let missed = eff.saturating_sub(completed);
    let completion_rate = if eff > 0 { (completed as f64 / eff as f64).min(1.0) } else { 1.0 };
    let swap_count = state.swap_requests.iter()
        .filter(|s| s.group_id == *group_id && s.status == SwapStatus::Accepted)
        .count() as u32;

    Some(GroupStats {
        group_id:        group_id.clone(),
        group_name:      group.name.clone(),
        member_count:    group.member_ids.len() as u32,
        due_weeks,
        completed,
        skipped,
        missed,
        completion_rate,
        current_streak:  state.streak_for(group_id, interval),
        swap_count,
    })
}

/// Compute how equitably cleaning is distributed within a group.
///
/// Returns `None` if the group has no members or no history.
pub fn fairness_report(state: &State, group_id: &GroupId, interval: u32) -> Option<FairnessReport> {
    let group = state.group_by_id(group_id)?;
    if group.member_ids.is_empty() { return None; }

    let (cur_y, cur_w) = current_iso_week();
    let start = state.tracking_start();
    let all_due = all_due_weeks_in_range(start, (cur_y, cur_w), interval);
    let closed: Vec<_> = all_due.iter().copied().filter(|&(y,w)| (y,w) != (cur_y,cur_w)).collect();
    let due_weeks = closed.len() as u32;

    let n = group.member_ids.len() as f64;
    let expected_each = due_weeks as f64 / n;

    let mut entries: Vec<FairnessEntry> = group.member_ids.iter()
        .filter_map(|pid| {
            let person = state.person_by_id(pid)?;
            let actual = state.completions.iter()
                .filter(|c| c.group_id == *group_id && c.completed_by_id == *pid && !c.skipped)
                .count() as u32;
            Some(FairnessEntry {
                person_id:    pid.clone(),
                display_name: person.display_name.clone(),
                expected:     expected_each,
                actual,
                deviation:    actual as f64 - expected_each,
            })
        })
        .collect();

    // Sort: biggest contributor first.
    entries.sort_by(|a, b| b.deviation.partial_cmp(&a.deviation).unwrap_or(std::cmp::Ordering::Equal));

    let gini = gini_coefficient(entries.iter().map(|e| e.actual as f64).collect());
    let fairness_score = (100.0 - (gini * 100.0).round()) as u8;

    Some(FairnessReport {
        group_id:       group_id.clone(),
        group_name:     group.name.clone(),
        due_weeks,
        entries,
        gini,
        fairness_score,
    })
}

/// All persons with stats, sorted by completion rate then streak.
///
/// Only includes persons who belong to at least one group.
pub fn global_leaderboard(state: &State, interval: u32) -> Vec<PersonStats> {
    let mut all: Vec<PersonStats> = state.persons.iter()
        .filter_map(|p| person_stats(state, &p.id, interval))
        .collect();
    all.sort_by(|a, b| {
        b.completion_rate.partial_cmp(&a.completion_rate)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.streak.cmp(&a.streak))
            .then(a.display_name.cmp(&b.display_name))
    });
    all
}

// ── Math ──────────────────────────────────────────────────────────────────────

/// Gini coefficient for a distribution of non-negative values.
///
/// Returns 0.0 for empty or uniform distributions.
/// Formula (sorted ascending): G = Σ (2i − n − 1) xᵢ / (n · Σxᵢ)
fn gini_coefficient(mut values: Vec<f64>) -> f64 {
    let n = values.len();
    if n <= 1 { return 0.0; }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let total: f64 = values.iter().sum();
    if total == 0.0 { return 0.0; }
    let weighted: f64 = values.iter().enumerate()
        .map(|(i, &v)| (2 * (i + 1)) as f64 * v)
        .sum::<f64>();
    ((weighted - (n + 1) as f64 * total) / (n as f64 * total)).abs()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gini_equal_distribution_is_zero() {
        assert_eq!(gini_coefficient(vec![3.0, 3.0, 3.0]), 0.0);
    }

    #[test]
    fn gini_one_person_does_everything() {
        let g = gini_coefficient(vec![0.0, 0.0, 6.0]);
        assert!(g > 0.5, "expected high Gini, got {g}");
    }

    #[test]
    fn gini_empty_is_zero() {
        assert_eq!(gini_coefficient(vec![]), 0.0);
    }

    #[test]
    fn gini_single_value_is_zero() {
        assert_eq!(gini_coefficient(vec![5.0]), 0.0);
    }

    #[test]
    fn backfill_produces_events_from_completions() {
        use crate::{domain::{CleaningGroup, Person}, state::{Completion, State}};
        let mut st = State::default();
        st.created_at = Some(Utc::now());

        let p  = Person::new_matrix("@alice:example.org");
        let pid = p.id.clone();
        st.persons.push(p);
        let mut g = CleaningGroup::new("Kitchen");
        let gid = g.id.clone();
        g.member_ids.push(pid.clone());
        st.cleaning_groups.push(g);

        st.completions.push(Completion {
            group_id:               gid.clone(),
            slot_id:                None,
            completed_by_id:        pid.clone(),
            responsible_person_ids: vec![],
            iso_year: 2025, iso_week: 10,
            completed_at: Utc::now(),
            skipped: false,
        });

        let events = backfill_events(&st);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].event, DomainEvent::CleaningCompleted { .. }));
    }

    #[test]
    fn fairness_report_perfect_equality() {
        use crate::{domain::{CleaningGroup, Person}, state::{Completion, State}};
        let mut st = State::default();
        st.created_at = Some(chrono::DateTime::from_timestamp(0, 0).unwrap());

        let p1 = Person::new_matrix("@alice:example.org");
        let p2 = Person::new_matrix("@bob:example.org");
        let pid1 = p1.id.clone();
        let pid2 = p2.id.clone();
        st.persons.push(p1);
        st.persons.push(p2);

        let mut g = CleaningGroup::new("TestGroup");
        let gid = g.id.clone();
        g.member_ids = vec![pid1.clone(), pid2.clone()];
        st.cleaning_groups.push(g);

        // Each person cleaned once.
        for (pid, week) in [(&pid1, 1u32), (&pid2, 2u32)] {
            st.completions.push(Completion {
                group_id:               gid.clone(),
                slot_id:                None,
                completed_by_id:        pid.clone(),
                responsible_person_ids: vec![],
                iso_year: 2025, iso_week: week,
                completed_at: Utc::now(),
                skipped: false,
            });
        }

        let report = fairness_report(&st, &gid, 1).unwrap();
        assert!(report.gini < 0.01, "expected near-zero Gini, got {}", report.gini);
        assert_eq!(report.fairness_score, 100);
    }
}
