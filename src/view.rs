//! How things read in Matrix. One wording for weeks, dates, people and
//! duties across every message, laid out for narrow phone screens
//! (FluffyChat, Element mobile): short lines, details on their own line,
//! nothing that only works as a wide table.
//!
//! Icons: one per concept, the same in every message, at most one leading
//! a line — and every line still reads without it.
//!
//! | 🧹 this week's cleaning | 📋 the plan ahead | 📅 dates, weeks |
//! | 🔄 rotation changes, swaps | 🏠 a group | 👥 members |
//! | 🚽 🚿 🍳 🧽 rooms (`rooms`) | ⬜ ✅ ⏭️ ❌ 🌴 a turn's state | ✨ cleaned |
//! | 🔔 reminder | ⏰ deadline | ❌ couldn't do that · ⚠️ heads-up |

use chrono::{Datelike, NaiveDate};

use crate::{
    domain::{AssignmentSource, Person},
    rhythm::{date_range, Rhythm, Turn},
    state::current_iso_week,
};

/// Indentation for a detail line under an item. An em space — unlike
/// ordinary spaces, HTML rendering keeps it at the start of a line.
pub const INDENT: &str = "\u{2003}";

/// "Week 42 · 12 – 18 Oct", with the year once it isn't the current one.
pub fn week_label(year: i32, week: u32) -> String {
    let monday = crate::rhythm::week_monday(year, week);
    let sunday = monday + chrono::Duration::days(6);
    format!(
        "Week {week} · {}{}",
        date_range(monday, sunday),
        year_suffix(year)
    )
}

/// A turn: "Week 42 · 12 – 18 Oct", or "Week 42 · Thu–Sun 15 – 18 Oct" for
/// one shift of a week split into shifts.
pub fn turn_label(turn: Turn, rhythm: &Rhythm) -> String {
    format!(
        "Week {} · {}{}",
        turn.week,
        turn.period_label(rhythm),
        year_suffix(turn.year)
    )
}

fn year_suffix(year: i32) -> String {
    if year == current_iso_week().0 {
        String::new()
    } else {
        format!(" {year}")
    }
}

/// When a turn is, relative to `today`: "now", "later this week",
/// "next week", "in 3 weeks".
pub fn relative(turn: Turn, rhythm: &Rhythm, today: NaiveDate) -> String {
    let (start, end) = turn.dates(rhythm);
    if start <= today && today <= end {
        return "now".into();
    }
    let this_week = (today.iso_week().year(), today.iso_week().week());
    match crate::state::weeks_between(this_week, turn.week()) {
        0 => "later this week".into(),
        1 => "next week".into(),
        n => format!("in {n} weeks"),
    }
}

/// A person in a read-only view (`!plan`, `!next`, `!status`, `!groups`):
/// a `[name](https://matrix.to/#/@user:server)` link, which the formatter
/// renders as a user pill (clickable, avatar in Element and FluffyChat) but
/// *without* an `m.mentions` entry, so looking at the plan pings nobody.
/// Without Matrix — or with an ID that can't sit in a link — the plain name.
pub fn user_link(person: &Person) -> String {
    match person.matrix_id.as_deref() {
        Some(mxid) => link_to(mxid, &person.display_name),
        None => person.display_name.clone(),
    }
}

/// A Matrix ID shown as itself — for "use the Matrix ID instead" — as a
/// pill that pings nobody, like `user_link`.
pub fn user_id_link(mxid: &str) -> String {
    link_to(mxid, mxid)
}

fn link_to(mxid: &str, label: &str) -> String {
    let label = label.replace(']', "］").replace('\n', " ");
    if mxid.contains([')', ' ', '\n']) {
        // Can't sit in a link; the plain label at least doesn't ping.
        return label;
    }
    format!("[{label}](https://matrix.to/#/{mxid})")
}

/// How a pinned week came about ("imported", …); `None` for a plain
/// rotation pick.
pub fn source_note(source: &AssignmentSource) -> Option<&'static str> {
    match source {
        AssignmentSource::RoundRobin => None,
        AssignmentSource::Manual | AssignmentSource::Assign => Some("assigned"),
        AssignmentSource::Takeover => Some("taken over"),
        AssignmentSource::Swap => Some("swapped"),
        AssignmentSource::Import => Some("imported"),
    }
}

/// Rooms as one line, by kind, one icon per kind:
/// "🚽 Scharni Toilet, Colbe Toilet · 🚿 Shower Room".
pub fn rooms(names: &[String]) -> String {
    let mut kinds: Vec<(&str, Vec<&str>)> = Vec::new();
    for name in names {
        let icon = room_icon(name);
        match kinds.iter_mut().find(|(i, _)| *i == icon) {
            Some((_, same)) => same.push(name),
            None => kinds.push((icon, vec![name])),
        }
    }
    kinds
        .iter()
        .map(|(icon, names)| format!("{icon} {}", names.join(", ")))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// What kind of room a name is, as an icon (English or German names).
fn room_icon(name: &str) -> &'static str {
    let name = name.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| name.contains(w));
    if any(&["toilet", "wc", "klo"]) {
        "🚽"
    } else if any(&["shower", "dusch"]) {
        "🚿"
    } else if any(&["kitchen", "küche", "kueche"]) {
        "🍳"
    } else {
        "🧽"
    }
}

/// "3 weeks", "1 week".
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_users_become_links_and_everyone_else_stays_plain() {
        let mut mia = Person::new_matrix("@mia:example.org");
        assert_eq!(
            user_link(&mia),
            "[mia](https://matrix.to/#/@mia:example.org)"
        );
        // A name can't end the link label early.
        mia.display_name = "Mia [away]".into();
        assert_eq!(
            user_link(&mia),
            "[Mia [away］](https://matrix.to/#/@mia:example.org)"
        );
        assert_eq!(user_link(&Person::new_named("Dan")), "Dan");
        assert_eq!(
            user_id_link("@mia:example.org"),
            "[@mia:example.org](https://matrix.to/#/@mia:example.org)"
        );
    }

    #[test]
    fn rooms_are_grouped_by_kind_with_one_icon_each() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            rooms(&names(&["Scharni Toilet", "Shower Room", "Colbe Toilet"])),
            "🚽 Scharni Toilet, Colbe Toilet · 🚿 Shower Room"
        );
        assert_eq!(
            rooms(&names(&["Küche", "Dusche", "Flur", "Bad"])),
            "🍳 Küche · 🚿 Dusche · 🧽 Flur, Bad"
        );
        assert_eq!(rooms(&[]), "");
    }

    #[test]
    fn week_labels_name_the_year_only_when_it_differs() {
        let (y, _) = current_iso_week();
        assert_eq!(week_label(y, 1).split(" · ").next(), Some("Week 1"));
        assert!(!week_label(y, 10).ends_with(&y.to_string()));
        assert!(week_label(y + 1, 10).ends_with(&format!(" {}", y + 1)));
    }

    #[test]
    fn relative_times_read_naturally() {
        let rhythm = Rhythm::weekly();
        let today = NaiveDate::from_isoywd_opt(2026, 40, chrono::Weekday::Wed).unwrap();
        let at = |w: u32| relative(Turn::new(2026, w, 0), &rhythm, today);
        assert_eq!(at(40), "now");
        assert_eq!(at(41), "next week");
        assert_eq!(at(43), "in 3 weeks");
        let split = Rhythm {
            every_weeks: Some(1),
            shift_starts: vec![0, 3],
            ..Default::default()
        };
        // The Thu–Sun shift of this week hasn't started on Wednesday.
        assert_eq!(
            relative(Turn::new(2026, 40, 1), &split, today),
            "later this week"
        );
    }
}
