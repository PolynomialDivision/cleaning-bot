//! State validation layer.
//!
//! `validate_state()` checks the persisted state for internal inconsistencies:
//! orphaned references, duplicate IDs, broken migrations, etc.
//!
//! Runs on load (logs only, never prevents startup) and is exposed as `!validate`
//! for admin inspection at runtime.

use std::collections::{HashMap, HashSet};
use crate::state::State;

// ── Report ────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct ValidationReport {
    /// Structural problems that should be fixed.
    pub errors:   Vec<String>,
    /// Non-blocking oddities worth knowing about.
    pub warnings: Vec<String>,
}

impl ValidationReport {
    #[allow(dead_code)]
    pub fn is_valid(&self) -> bool { self.errors.is_empty() }

    /// Human-readable summary suitable for a Matrix message or log line.
    pub fn summary(&self) -> String {
        if self.errors.is_empty() && self.warnings.is_empty() {
            return "✅ State is consistent — no issues found.".into();
        }
        let mut lines = Vec::new();
        if !self.errors.is_empty() {
            lines.push(format!("❌ {} error(s):", self.errors.len()));
            for e in &self.errors   { lines.push(format!("  • {e}")); }
        }
        if !self.warnings.is_empty() {
            lines.push(format!("⚠️ {} warning(s):", self.warnings.len()));
            for w in &self.warnings { lines.push(format!("  • {w}")); }
        }
        lines.join("\n")
    }
}

// ── Validator ─────────────────────────────────────────────────────────────────

/// Validate the entire persisted state and return a `ValidationReport`.
///
/// This function is pure (read-only); it never mutates state.
pub fn validate_state(state: &State) -> ValidationReport {
    let mut report = ValidationReport::default();
    let errors   = &mut report.errors;
    let warnings = &mut report.warnings;

    // ── 1. Check for duplicate PersonIds ─────────────────────────────────────
    {
        let mut seen: HashSet<&str> = HashSet::new();
        for p in &state.persons {
            if p.id.is_empty() {
                errors.push(format!("Person «{}» has an empty ID", p.display_name));
            } else if !seen.insert(&p.id) {
                errors.push(format!("Duplicate PersonId: {} ({})", p.id, p.display_name));
            }
        }
    }
    let valid_person_ids: HashSet<&str> = state.persons.iter().map(|p| p.id.as_str()).collect();

    // ── 2. Check for duplicate GroupIds ──────────────────────────────────────
    {
        let mut seen: HashSet<&str> = HashSet::new();
        for g in &state.cleaning_groups {
            if g.id.is_empty() {
                errors.push(format!("Group «{}» has an empty ID", g.name));
            } else if !seen.insert(&g.id) {
                errors.push(format!("Duplicate GroupId: {} ({})", g.id, g.name));
            }
        }
    }
    let valid_group_ids: HashSet<&str> = state.cleaning_groups.iter().map(|g| g.id.as_str()).collect();

    // ── 3. Group member_ids must reference valid persons ──────────────────────
    for g in &state.cleaning_groups {
        for pid in &g.member_ids {
            if !valid_person_ids.contains(pid.as_str()) {
                errors.push(format!("Group «{}» references unknown PersonId: {}", g.name, pid));
            }
        }
        if g.member_ids.is_empty() {
            warnings.push(format!("Group «{}» has no members", g.name));
        }
        // Duplicate member check
        let mut ms: HashSet<&str> = HashSet::new();
        for pid in &g.member_ids {
            if !ms.insert(pid.as_str()) {
                warnings.push(format!("Group «{}» contains duplicate member ID: {}", g.name, pid));
            }
        }
    }

    // ── 4. Completions ────────────────────────────────────────────────────────
    for (i, c) in state.completions.iter().enumerate() {
        if c.group_id.is_empty() {
            warnings.push(format!("Completion #{i} has no group_id (migration incomplete?)"));
        } else if !valid_group_ids.contains(c.group_id.as_str()) {
            warnings.push(format!("Completion #{i} references unknown GroupId: {}", c.group_id));
        }
        if c.completed_by_id.is_empty() {
            warnings.push(format!("Completion #{i} has no completed_by_id (migration incomplete?)"));
        } else if !valid_person_ids.contains(c.completed_by_id.as_str()) {
            warnings.push(format!("Completion #{i} references unknown PersonId: {}", c.completed_by_id));
        }
    }

    // ── 5. Absences ───────────────────────────────────────────────────────────
    for (i, a) in state.absences.iter().enumerate() {
        if a.person_id.is_empty() {
            warnings.push(format!("Absence #{i} has no person_id (migration incomplete?)"));
        } else if !valid_person_ids.contains(a.person_id.as_str()) {
            warnings.push(format!("Absence #{i} references unknown PersonId: {}", a.person_id));
        }
        if a.group_id.is_empty() {
            warnings.push(format!("Absence #{i} has no group_id (migration incomplete?)"));
        } else if !valid_group_ids.contains(a.group_id.as_str()) {
            warnings.push(format!("Absence #{i} references unknown GroupId: {}", a.group_id));
        }
    }

    // ── 6. SwapRequests ───────────────────────────────────────────────────────
    for s in &state.swap_requests {
        if s.group_id.is_empty() {
            warnings.push(format!("Swap #{} has no group_id (migration incomplete?)", s.id));
        } else if !valid_group_ids.contains(s.group_id.as_str()) {
            warnings.push(format!("Swap #{} references unknown GroupId: {}", s.id, s.group_id));
        }
    }

    // ── 7. CalendarTokens ─────────────────────────────────────────────────────
    {
        let mut active_per_person: HashMap<&str, usize> = HashMap::new();
        for ct in &state.calendar_tokens {
            if ct.token_hash.is_empty() {
                errors.push(format!("CalendarToken {} has empty hash", ct.id));
            }
            if !valid_person_ids.contains(ct.person_id.as_str()) {
                warnings.push(format!("CalendarToken {} references unknown PersonId: {}", ct.id, ct.person_id));
            }
            if !ct.revoked {
                *active_per_person.entry(&ct.person_id).or_default() += 1;
            }
        }
        for (pid, count) in active_per_person {
            if count > 1 {
                warnings.push(format!("Person {pid} has {count} active calendar tokens (expected 1)"));
            }
        }
    }

    // ── 8. Legacy data still present ─────────────────────────────────────────
    if !state.floors.is_empty() {
        warnings.push(format!("{} legacy floor(s) still in state — migration may be incomplete", state.floors.len()));
    }
    if !state.groups.is_empty() {
        warnings.push(format!("{} legacy floor-group(s) still in state — migration may be incomplete", state.groups.len()));
    }

    // ── 9. Persons without any group ─────────────────────────────────────────
    let assigned_persons: HashSet<&str> = state.cleaning_groups.iter()
        .flat_map(|g| g.member_ids.iter().map(|id| id.as_str()))
        .collect();
    for p in &state.persons {
        if p.active && !assigned_persons.contains(p.id.as_str()) {
            warnings.push(format!("Person «{}» is active but not in any cleaning group", p.display_name));
        }
    }

    report
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{domain::{CleaningGroup, Person}, state::State};

    #[test]
    fn empty_state_is_valid() {
        let st = State::default();
        let r = validate_state(&st);
        assert!(r.is_valid(), "errors: {:?}", r.errors);
    }

    #[test]
    fn valid_state_passes() {
        let mut st = State::default();
        let p = Person::new_matrix("@alice:example.org");
        let pid = p.id.clone();
        st.persons.push(p);
        let mut g = CleaningGroup::new("Kitchen");
        g.member_ids.push(pid);
        st.cleaning_groups.push(g);
        let r = validate_state(&st);
        assert!(r.is_valid(), "errors: {:?}", r.errors);
        assert!(r.warnings.is_empty(), "warnings: {:?}", r.warnings);
    }

    #[test]
    fn orphan_member_reference_is_error() {
        let mut st = State::default();
        let mut g = CleaningGroup::new("Hallway");
        g.member_ids.push("nonexistent-uuid".to_owned());
        st.cleaning_groups.push(g);
        let r = validate_state(&st);
        assert!(!r.is_valid());
        assert!(r.errors.iter().any(|e| e.contains("unknown PersonId")));
    }

    #[test]
    fn duplicate_person_id_is_error() {
        let mut st = State::default();
        let p1 = Person::new_matrix("@alice:example.org");
        let mut p2 = Person::new_named("Bob");
        p2.id = p1.id.clone();   // force duplicate ID
        st.persons.push(p1);
        st.persons.push(p2);
        let r = validate_state(&st);
        assert!(!r.is_valid());
        assert!(r.errors.iter().any(|e| e.contains("Duplicate PersonId")));
    }

    #[test]
    fn empty_group_is_a_warning() {
        let mut st = State::default();
        st.cleaning_groups.push(CleaningGroup::new("EmptyFloor"));
        let r = validate_state(&st);
        assert!(r.is_valid());   // not an error
        assert!(r.warnings.iter().any(|w| w.contains("no members")));
    }
}
