//! When a group is cleaned: its **rhythm** and the **turns** it produces.
//!
//! Every group has a `Rhythm`: it is due every `every_weeks` weeks (aligned
//! to the tracking start), and each due week is split into one or more
//! consecutive **shifts** — e.g. one shift for the whole week (the classic
//! model), or Mon–Wed + Thu–Sun for "twice a week". One shift of one due
//! week is a `Turn`: the unit everything else works with — one person per
//! turn (and slot) is responsible, gets reminded, marks it done, can swap
//! it, take it over, and is counted in stats.
//!
//! Stored records keep their ISO year/week fields and add a `shift` index
//! defaulting to 0, so data from before rhythms existed reads as shift 0 of
//! a whole-week rhythm — exactly what it was.

use chrono::{Datelike, Duration, NaiveDate, Weekday};
use serde::{Deserialize, Serialize};

/// Short weekday names, Monday first.
pub const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// How often a group is cleaned — `CleaningGroup::rhythm`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Rhythm {
    /// Due every N weeks. `None` only in data from before per-group
    /// rhythms; filled from the old global `interval_weeks` at startup.
    #[serde(default)]
    pub every_weeks: Option<u32>,
    /// Weekday (0 = Monday) on which each shift starts; the first is always
    /// Monday and each shift runs until the day before the next one starts
    /// (the last until Sunday). Empty = one shift for the whole week.
    #[serde(default)]
    pub shift_starts: Vec<u8>,
}

/// One shift of a week: weekdays `start..=end` (0 = Monday).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shift {
    pub start: u8,
    pub end: u8,
}

impl Shift {
    /// "Mon–Wed", or "Thu" for a one-day shift.
    pub fn label(&self) -> String {
        if self.start == self.end {
            WEEKDAYS[self.start as usize].to_owned()
        } else {
            format!(
                "{}–{}",
                WEEKDAYS[self.start as usize], WEEKDAYS[self.end as usize]
            )
        }
    }
}

impl Rhythm {
    pub fn weekly() -> Self {
        Rhythm {
            every_weeks: Some(1),
            shift_starts: Vec::new(),
        }
    }

    pub fn every_weeks(&self) -> u32 {
        self.every_weeks.unwrap_or(1).max(1)
    }

    /// The week's shifts in order (at least one).
    pub fn shifts(&self) -> Vec<Shift> {
        let mut starts: Vec<u8> = self
            .shift_starts
            .iter()
            .copied()
            .filter(|d| *d < 7)
            .collect();
        starts.sort_unstable();
        starts.dedup();
        if starts.first() != Some(&0) {
            starts.insert(0, 0);
        }
        starts
            .iter()
            .enumerate()
            .map(|(i, &start)| Shift {
                start,
                end: starts.get(i + 1).map_or(6, |next| next - 1),
            })
            .collect()
    }

    pub fn shift_count(&self) -> usize {
        self.shifts().len()
    }

    pub fn is_split(&self) -> bool {
        self.shift_count() > 1
    }

    pub fn shift(&self, index: u8) -> Option<Shift> {
        self.shifts().get(index as usize).copied()
    }

    /// Index of the shift containing `weekday` (0 = Monday).
    pub fn shift_for_weekday(&self, weekday: u8) -> u8 {
        self.shifts()
            .iter()
            .rposition(|s| s.start <= weekday)
            .unwrap_or(0) as u8
    }

    /// `n` shifts per week, as evenly sized as whole days allow
    /// (2 → Mon–Wed / Thu–Sun, 3 → Mon–Tue / Wed–Thu / Fri–Sun, 7 → daily).
    pub fn times_per_week(n: u8) -> Vec<u8> {
        let n = n.clamp(1, 7) as u32;
        (0..n).map(|i| (i * 7 / n) as u8).collect()
    }

    /// "weekly", "every 2 weeks", "2× per week (Mon–Wed, Thu–Sun)", …
    pub fn describe(&self) -> String {
        let every = match self.every_weeks() {
            1 => None,
            n => Some(format!("every {n} weeks")),
        };
        let shifts = self.shifts();
        if shifts.len() == 1 {
            return every.unwrap_or_else(|| "weekly".to_owned());
        }
        let labels = shifts
            .iter()
            .map(Shift::label)
            .collect::<Vec<_>>()
            .join(", ");
        match every {
            None => format!("{}× per week ({labels})", shifts.len()),
            Some(every) => format!("{}× per due week, {every} ({labels})", shifts.len()),
        }
    }
}

/// One period of responsibility: shift `shift` of ISO week `(year, week)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Turn {
    pub year: i32,
    pub week: u32,
    pub shift: u8,
}

impl Turn {
    pub fn new(year: i32, week: u32, shift: u8) -> Self {
        Turn { year, week, shift }
    }

    pub fn week(&self) -> (i32, u32) {
        (self.year, self.week)
    }

    /// First and last day of this turn under `rhythm`.
    pub fn dates(&self, rhythm: &Rhythm) -> (NaiveDate, NaiveDate) {
        let monday = week_monday(self.year, self.week);
        let shift = rhythm
            .shift(self.shift)
            .unwrap_or(Shift { start: 0, end: 6 });
        (
            monday + Duration::days(shift.start as i64),
            monday + Duration::days(shift.end as i64),
        )
    }

    /// Weekday label within the week ("Mon–Wed"), or `None` for a
    /// whole-week rhythm where the week alone says everything.
    pub fn shift_label(&self, rhythm: &Rhythm) -> Option<String> {
        if rhythm.is_split() {
            rhythm.shift(self.shift).map(|s| s.label())
        } else {
            None
        }
    }

    /// Human period: "22 – 28 Sep" for a whole week, "Thu–Sun 25 – 28 Sep"
    /// for a shift.
    pub fn period_label(&self, rhythm: &Rhythm) -> String {
        let (start, end) = self.dates(rhythm);
        let dates = date_range(start, end);
        match self.shift_label(rhythm) {
            Some(label) => format!("{label} {dates}"),
            None => dates,
        }
    }
}

pub fn week_monday(year: i32, week: u32) -> NaiveDate {
    NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 4).unwrap())
}

/// "22 – 28 Sep", "29 Sep – 5 Oct", or "25 Sep" for a single day.
pub fn date_range(start: NaiveDate, end: NaiveDate) -> String {
    if start == end {
        start.format("%-d %b").to_string()
    } else if start.month() == end.month() {
        format!(
            "{} – {} {}",
            start.format("%-d"),
            end.format("%-d"),
            start.format("%b")
        )
    } else {
        format!("{} – {}", start.format("%-d %b"), end.format("%-d %b"))
    }
}

/// Weekday from a command argument: `mon`…`sun` (English) or `mo`…`so`
/// (German), also spelled out.
pub fn parse_weekday(s: &str) -> Option<u8> {
    let s = s.to_ascii_lowercase();
    const NAMES: [&[&str]; 7] = [
        &["mon", "monday", "mo", "montag"],
        &["tue", "tuesday", "di", "dienstag"],
        &["wed", "wednesday", "mi", "mittwoch"],
        &["thu", "thursday", "do", "donnerstag"],
        &["fri", "friday", "fr", "freitag"],
        &["sat", "saturday", "sa", "samstag"],
        &["sun", "sunday", "so", "sonntag"],
    ];
    NAMES
        .iter()
        .position(|names| names.contains(&s.as_str()))
        .map(|i| i as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rhythm_is_one_whole_week_shift() {
        let r = Rhythm::default();
        assert_eq!(r.every_weeks(), 1);
        assert_eq!(r.shifts(), [Shift { start: 0, end: 6 }]);
        assert_eq!(r.describe(), "weekly");
        assert_eq!(Turn::new(2025, 39, 0).shift_label(&r), None);
    }

    #[test]
    fn twice_a_week_splits_monday_to_wednesday_and_thursday_to_sunday() {
        let r = Rhythm {
            every_weeks: Some(1),
            shift_starts: Rhythm::times_per_week(2),
        };
        assert_eq!(
            r.shifts(),
            [Shift { start: 0, end: 2 }, Shift { start: 3, end: 6 }]
        );
        assert_eq!(r.describe(), "2× per week (Mon–Wed, Thu–Sun)");
        assert_eq!(r.shift_for_weekday(1), 0);
        assert_eq!(r.shift_for_weekday(3), 1);
        assert_eq!(r.shift_for_weekday(6), 1);

        let turn = Turn::new(2025, 39, 1);
        let (start, end) = turn.dates(&r);
        assert_eq!(start, NaiveDate::from_ymd_opt(2025, 9, 25).unwrap());
        assert_eq!(end, NaiveDate::from_ymd_opt(2025, 9, 28).unwrap());
        assert_eq!(turn.period_label(&r), "Thu–Sun 25 – 28 Sep");
    }

    #[test]
    fn explicit_starts_always_begin_on_monday() {
        let r = Rhythm {
            every_weeks: Some(2),
            shift_starts: vec![4],
        };
        assert_eq!(
            r.shifts(),
            [Shift { start: 0, end: 3 }, Shift { start: 4, end: 6 }]
        );
        assert_eq!(
            r.describe(),
            "2× per due week, every 2 weeks (Mon–Thu, Fri–Sun)"
        );
        assert_eq!(Rhythm::times_per_week(7).len(), 7);
        assert_eq!(Rhythm::times_per_week(3), [0, 2, 4]);
    }

    #[test]
    fn weekdays_parse_in_english_and_german() {
        assert_eq!(parse_weekday("thu"), Some(3));
        assert_eq!(parse_weekday("Donnerstag"), Some(3));
        assert_eq!(parse_weekday("so"), Some(6));
        assert_eq!(parse_weekday("x"), None);
    }
}
