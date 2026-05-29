//! ICS (iCalendar) rendering.
//!
//! `render_ics` is a pure function over a `ScheduleSnapshot`.
//! It performs no state access, no scheduling logic, no mutations.
//!
//! Determinism guarantee: identical snapshot + identical person_id → identical bytes.

use crate::{
    domain::PersonId,
    schedule::ScheduleSnapshot,
};

/// Render an RFC 5545–compliant `.ics` string for a specific person.
///
/// Returns only the events where `person_id` is the assigned person.
/// Each event uses the assignment's pre-computed deterministic UID.
/// DTSTAMP is the snapshot's `state_timestamp` (stable for unchanged state).
pub fn render_ics(snapshot: &ScheduleSnapshot, person_id: &PersonId) -> String {
    let assignments = snapshot.for_person(person_id);

    let dtstamp = snapshot.state_timestamp.format("%Y%m%dT%H%M%SZ").to_string();

    let person_name = assignments.first()
        .and_then(|a| a.assignee.as_ref())
        .map(|p| p.name.as_str())
        .unwrap_or("?");

    let mut events = String::new();
    for a in &assignments {
        let dtstart = a.week_monday.format("%Y%m%d").to_string();
        let dtend   = (a.week_sunday + chrono::Duration::days(1)).format("%Y%m%d").to_string();
        let summary = ical_text(&format!("🧹 {}", a.group_name));

        let mut desc_parts = vec![a.week_label.clone()];
        if !a.room_names.is_empty() {
            desc_parts.push(format!("Rooms: {}", a.room_names.join(", ")));
        }
        // Show other assignees from the same group this week (siblings in snapshot).
        let others: Vec<&str> = snapshot.assignments.iter()
            .filter(|b| b.group_id == a.group_id && b.iso_year == a.iso_year && b.iso_week == a.iso_week
                && b.assignee.as_ref().map(|p| &p.id != person_id).unwrap_or(false))
            .filter_map(|b| b.assignee.as_ref().map(|p| p.name.as_str()))
            .collect();
        if !others.is_empty() {
            desc_parts.push(format!("Others: {}", others.join(", ")));
        }
        if a.is_completed && !a.is_skipped {
            if let Some(by) = &a.completed_by {
                desc_parts.push(format!("✓ Done by {by}"));
            } else {
                desc_parts.push("✓ Done".to_owned());
            }
        }
        let description = ical_text(&desc_parts.join("\n"));

        events.push_str("BEGIN:VEVENT\r\n");
        prop(&mut events, "UID",                &a.uid);
        prop(&mut events, "DTSTAMP",            &dtstamp);
        prop(&mut events, "DTSTART;VALUE=DATE", &dtstart);
        prop(&mut events, "DTEND;VALUE=DATE",   &dtend);
        prop(&mut events, "SUMMARY",            &summary);
        prop(&mut events, "DESCRIPTION",        &description);
        if a.is_completed { prop(&mut events, "STATUS", "COMPLETED"); }
        events.push_str("END:VEVENT\r\n");
    }

    let mut out = String::new();
    out.push_str("BEGIN:VCALENDAR\r\n");
    prop(&mut out, "VERSION",       "2.0");
    prop(&mut out, "PRODID",        "-//Cleaning Bot//EN");
    prop(&mut out, "CALSCALE",      "GREGORIAN");
    prop(&mut out, "METHOD",        "PUBLISH");
    prop(&mut out, "X-WR-CALNAME", &ical_text(&format!("Putzplan – {person_name}")));
    out.push_str(&events);
    out.push_str("END:VCALENDAR\r\n");
    out
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn ical_text(s: &str) -> String {
    s.replace('\\', "\\\\")
     .replace(';', "\\;")
     .replace(',', "\\,")
     .replace('\n', "\\n")
     .replace('\r', "")
}

fn prop(out: &mut String, name: &str, value: &str) {
    fold_line(out, &format!("{name}:{value}"));
}

fn fold_line(out: &mut String, line: &str) {
    if line.len() <= 75 {
        out.push_str(line);
        out.push_str("\r\n");
        return;
    }
    let first = char_boundary(line, 75);
    out.push_str(&line[..first]);
    out.push_str("\r\n");
    let mut rest = &line[first..];
    while !rest.is_empty() {
        let cut = char_boundary(rest, 74);
        out.push(' ');
        out.push_str(&rest[..cut]);
        out.push_str("\r\n");
        rest = &rest[cut..];
    }
}

fn char_boundary(s: &str, max: usize) -> usize {
    let n = max.min(s.len());
    (0..=n).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crate::{
        domain::{CleaningGroup, Person},
        schedule::build_schedule,
        state::State,
    };

    fn make_state() -> State {
        let mut st = State::default();
        st.created_at  = Some(Utc::now());
        st.last_modified = st.created_at;
        let p  = Person::new_matrix("@alice:example.org");
        let id = p.id.clone();
        st.persons.push(p);
        let mut g = CleaningGroup::new("Kitchen");
        g.member_ids.push(id);
        st.cleaning_groups.push(g);
        st
    }

    #[test]
    fn ics_output_is_deterministic() {
        let st  = make_state();
        let sn1 = build_schedule(&st, 1, 4);
        let sn2 = build_schedule(&st, 1, 4);
        let id  = &st.persons[0].id;
        assert_eq!(render_ics(&sn1, id), render_ics(&sn2, id));
    }

    #[test]
    fn ics_contains_stable_uids() {
        let st  = make_state();
        let sn1 = build_schedule(&st, 1, 2);
        let sn2 = build_schedule(&st, 1, 2);
        let id  = &st.persons[0].id;
        let body1 = render_ics(&sn1, id);
        let body2 = render_ics(&sn2, id);
        // Extract all UID lines and compare.
        let uids1: Vec<&str> = body1.lines().filter(|l| l.starts_with("UID:")).collect();
        let uids2: Vec<&str> = body2.lines().filter(|l| l.starts_with("UID:")).collect();
        assert_eq!(uids1, uids2, "UIDs must be stable");
        assert!(!uids1.is_empty(), "should have at least one VEVENT");
    }

    #[test]
    fn ics_empty_for_unknown_person() {
        let st = make_state();
        let sn = build_schedule(&st, 1, 4);
        let ics = render_ics(&sn, &"not-a-real-uuid".to_owned());
        assert!(!ics.contains("BEGIN:VEVENT"), "no events for unknown person");
    }

    #[test]
    fn fold_short_line_unchanged() {
        let mut out = String::new();
        fold_line(&mut out, "SUMMARY:Hello");
        assert_eq!(out, "SUMMARY:Hello\r\n");
    }

    #[test]
    fn fold_long_line_wraps() {
        let long = format!("DESCRIPTION:{}", "x".repeat(100));
        let mut out = String::new();
        fold_line(&mut out, &long);
        for physical in out.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(physical.len() <= 75, "line too long: {physical:?}");
        }
    }

    #[test]
    fn ical_text_escapes() {
        assert_eq!(ical_text("a,b;c\\d\ne"), "a\\,b\\;c\\\\d\\ne");
    }
}
