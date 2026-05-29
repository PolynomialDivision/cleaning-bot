//! Generate iCalendar (.ics) data for a person's cleaning schedule.
//!
//! Produces RFC 5545-compliant VCALENDAR output with one VEVENT per due week
//! where the given person is the `responsible_user` for a cleaning area.
//! Lines are folded at 75 octets as required by the spec.

use chrono::{Datelike, NaiveDate, Utc, Weekday};
use crate::state::{State, add_weeks, current_iso_week, weeks_between};

/// Generate an `.ics` file for `person_id` covering the next `weeks` due occurrences.
///
/// `person_id` is either a Matrix MXID (`@user:server`) or the display name of a
/// non-Matrix person — matched case-insensitively via `PersonRef::matches_str`.
pub fn generate_ical(state: &State, interval: u32, person_id: &str, weeks: usize) -> String {
    let (cur_y, cur_w) = current_iso_week();
    let (start_y, start_w) = state.tracking_start();
    let iv = interval as i64;

    // First due week >= current week (same logic as !cleanplan / pdf).
    let elapsed = weeks_between((start_y, start_w), (cur_y, cur_w));
    let first_due = if elapsed < 0 {
        (start_y, start_w)
    } else {
        let past = elapsed / iv;
        if elapsed % iv == 0 { (cur_y, cur_w) }
        else { add_weeks(start_y, start_w, (past + 1) * iv) }
    };

    let units = state.units();
    let dtstamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut events = String::new();

    for i in 0..(weeks as i64) {
        let (dy, dw) = add_weeks(first_due.0, first_due.1, i * iv);

        for unit in &units {
            let responsible = state.responsible_user(unit, dy, dw, interval);
            if !responsible.as_ref().map(|p| p.matches_str(person_id)).unwrap_or(false) {
                continue;
            }

            let monday = NaiveDate::from_isoywd_opt(dy, dw, Weekday::Mon)
                .unwrap_or_else(|| NaiveDate::from_ymd_opt(dy, 1, 4).unwrap());
            // DTEND is exclusive — one week after DTSTART = all-day spanning the full week.
            let next_mon = monday + chrono::Duration::days(7);
            let dtstart = monday.format("%Y%m%d").to_string();
            let dtend   = next_mon.format("%Y%m%d").to_string();

            let uid = format!("cleaning-{}-{}-W{}@cleaning-bot", slug(&unit.name), dy, dw);
            let summary = format!("🧹 {}", unit.name);

            // Build description: week dates, rooms, other members.
            let sunday = monday + chrono::Duration::days(6);
            let mut desc_parts = vec![
                format!("KW {} ({}.{}. – {}.{}.{})",
                    dw,
                    monday.day(), monday.month(),
                    sunday.day(), sunday.month(), sunday.year()),
            ];
            if let Some(rt) = unit.rooms_text() {
                desc_parts.push(rt);
            }
            let others: Vec<&str> = unit.members.iter()
                .filter(|p| !p.matches_str(person_id))
                .map(|p| p.display())
                .collect();
            if !others.is_empty() {
                desc_parts.push(format!("Weitere: {}", others.join(", ")));
            }
            if state.is_completed(&unit.name, dy, dw) {
                desc_parts.push("✓ Bereits erledigt".to_owned());
            }
            let description = ical_text(&desc_parts.join("\n"));

            events.push_str("BEGIN:VEVENT\r\n");
            prop(&mut events, "UID",                    &uid);
            prop(&mut events, "DTSTAMP",                &dtstamp);
            prop(&mut events, "DTSTART;VALUE=DATE",     &dtstart);
            prop(&mut events, "DTEND;VALUE=DATE",       &dtend);
            prop(&mut events, "SUMMARY",                &ical_text(&summary));
            prop(&mut events, "DESCRIPTION",            &description);
            if state.is_completed(&unit.name, dy, dw) {
                prop(&mut events, "STATUS", "COMPLETED");
            }
            events.push_str("END:VEVENT\r\n");
        }
    }

    // Friendly calendar name using the person's canonical display form.
    let person_name = state.units().iter()
        .flat_map(|u| u.members.iter())
        .find(|p| p.matches_str(person_id))
        .map(|p| p.display().to_owned())
        .unwrap_or_else(|| person_id.to_owned());

    let mut out = String::new();
    out.push_str("BEGIN:VCALENDAR\r\n");
    prop(&mut out, "VERSION",       "2.0");
    prop(&mut out, "PRODID",        "-//Cleaning Bot//EN");
    prop(&mut out, "CALSCALE",      "GREGORIAN");
    prop(&mut out, "METHOD",        "PUBLISH");
    prop(&mut out, "X-WR-CALNAME", &ical_text(&format!("Putzplan – {}", person_name)));
    out.push_str(&events);
    out.push_str("END:VCALENDAR\r\n");
    out
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Escape TEXT values per RFC 5545 §3.3.11.
fn ical_text(s: &str) -> String {
    s.replace('\\', "\\\\")
     .replace(';', "\\;")
     .replace(',', "\\,")
     .replace('\n', "\\n")
     .replace('\r', "")
}

/// Write one property line with RFC 5545 line folding at 75 octets.
fn prop(out: &mut String, name: &str, value: &str) {
    let line = format!("{name}:{value}");
    fold_line(out, &line);
}

/// Fold a single iCal line: first chunk ≤ 75 bytes, subsequent chunks ≤ 74 bytes
/// (the leading SPACE counts as 1 byte).
fn fold_line(out: &mut String, line: &str) {
    if line.len() <= 75 {
        out.push_str(line);
        out.push_str("\r\n");
        return;
    }
    let first_cut = char_boundary(line, 75);
    out.push_str(&line[..first_cut]);
    out.push_str("\r\n");
    let mut rest = &line[first_cut..];
    while !rest.is_empty() {
        let cut = char_boundary(rest, 74);
        out.push(' ');
        out.push_str(&rest[..cut]);
        out.push_str("\r\n");
        rest = &rest[cut..];
    }
}

/// Largest byte index ≤ `max` that is a valid UTF-8 char boundary in `s`.
fn char_boundary(s: &str, max: usize) -> usize {
    let n = max.min(s.len());
    (0..=n).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0)
}

/// Turn an arbitrary string into an ASCII-safe identifier for UIDs.
fn slug(s: &str) -> String {
    s.chars()
     .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
     .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Every logical line (split on \r\n) must be ≤ 75 bytes.
        for physical_line in out.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(physical_line.len() <= 75, "line too long: {physical_line:?}");
        }
        // Reassembled value should equal original
        let reassembled: String = out
            .split("\r\n")
            .filter(|l| !l.is_empty())
            .enumerate()
            .map(|(i, l)| if i == 0 { l.to_owned() } else { l[1..].to_owned() }) // strip leading space
            .collect();
        assert_eq!(reassembled, long);
    }

    #[test]
    fn ical_text_escapes() {
        assert_eq!(ical_text("a,b;c\\d\ne"), "a\\,b\\;c\\\\d\\ne");
    }
}
