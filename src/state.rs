use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc, Weekday};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, HashSet}, path::Path};

// ── Persistent state ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    pub floors: Vec<Floor>,
    /// Groups of floors that clean together.  Users from all member floors are
    /// jointly responsible for the group's cleaning period.
    pub groups: Vec<FloorGroup>,
    pub completions: Vec<Completion>,
    pub swap_requests: Vec<SwapRequest>,
    /// Tracks which reminder messages have already been sent so we never
    /// double-post after a reboot or re-sync.
    pub sent_reminders: Vec<SentReminder>,
    /// Monotonically increasing ID counter for swap requests.
    pub next_id: u64,
    /// Set once on first run; used as the baseline for statistics.
    pub created_at: Option<DateTime<Utc>>,
    /// Maps event IDs of bot reminder messages to the unit name they belong to.
    /// Used to treat a ✅ reaction on a reminder as an implicit !done.
    #[serde(default)]
    pub reminder_event_ids: HashMap<String, String>,
    /// Maps reaction event IDs to the completion they created.
    /// When a ✅ reaction is removed (redacted), we look up the matching
    /// completion here and remove it — auto-undo.
    #[serde(default)]
    pub reaction_dones: HashMap<String, ReactionDone>,
    /// Temporary absences: users marked away from their floor for N weeks.
    #[serde(default)]
    pub absences: Vec<Absence>,
    /// Maps the event ID of a greeting message to the floor choices embedded in it.
    /// Used so a ✅-numbered reaction on a greeting triggers !joinfloor automatically.
    #[serde(default)]
    pub greeting_event_ids: HashMap<String, GreetingInfo>,
    /// Tracks which users have already been greeted so we don't spam them.
    #[serde(default)]
    pub greeted_users: HashSet<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Floor {
    pub name: String,
    pub users: Vec<String>,
    /// Rooms on this floor that need to be cleaned (e.g. "Kitchen", "Bathroom").
    #[serde(default)]
    pub rooms: Vec<String>,
}

/// Multiple floors sharing one cleaning schedule.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FloorGroup {
    pub name: String,
    pub floor_names: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Completion {
    /// Name of the cleaning unit (floor or group) that was completed.
    pub unit_name: String,
    pub iso_year: i32,
    pub iso_week: u32,
    pub completed_at: DateTime<Utc>,
    pub completed_by: String,
    /// Users who were responsible for this unit at the time of completion.
    /// Stored so stats remain accurate even if the floor's user list changes later.
    #[serde(default)]
    pub responsible_users: Vec<String>,
    /// When true this week was deliberately skipped (e.g. everyone on holiday).
    /// It counts as "not due" for scheduling but is excluded from completion stats.
    #[serde(default)]
    pub skipped: bool,
}

/// Tracks which reaction event created a particular completion so we can
/// undo it automatically when the user removes their ✅.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReactionDone {
    pub unit_name: String,
    pub iso_year: i32,
    pub iso_week: u32,
    pub completed_by: String,
}

/// One floor/group option in a greeting message, bound to a numbered emoji.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GreetingChoice {
    /// The emoji reaction key (e.g. "1️⃣").
    pub emoji: String,
    /// The cleaning unit name (floor or group) to join.
    pub unit_name: String,
    /// The actual floor names whose user list should be extended.
    pub floor_names: Vec<String>,
}

/// Everything we need to remember about a pending greeting message.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GreetingInfo {
    /// MXID of the newly joined user this greeting was sent for.
    pub for_user: String,
    /// Ordered list of emoji → floor choices shown in the message.
    pub choices: Vec<GreetingChoice>,
}

/// A user is temporarily absent from their floor for `duration_weeks` weeks
/// starting from `(from_year, from_week)`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Absence {
    pub user_id: String,
    pub floor_name: String,
    pub from_year: i32,
    pub from_week: u32,
    pub duration_weeks: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SwapRequest {
    pub id: u64,
    pub requester: String,
    pub target: String,
    pub unit_name: String,
    pub iso_year: i32,
    pub iso_week: u32,
    pub created_at: DateTime<Utc>,
    pub status: SwapStatus,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SwapStatus {
    Pending,
    Accepted,
    Rejected,
    Cancelled,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReminderKind {
    Initial,
    Final,
    /// Sent once per week by the summary scheduler.
    WeeklySummary,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SentReminder {
    /// Empty string for WeeklySummary (not unit-specific).
    pub unit_name: String,
    pub iso_year: i32,
    pub iso_week: u32,
    pub kind: ReminderKind,
    /// When the reminder was actually sent. Allows computing response time later.
    #[serde(default)]
    pub sent_at: Option<DateTime<Utc>>,
}

// ── I/O ───────────────────────────────────────────────────────────────────────

impl State {
    pub async fn load(path: &Path) -> Result<Self> {
        if tokio::fs::metadata(path).await.is_ok() {
            let s = tokio::fs::read_to_string(path).await?;
            Ok(serde_json::from_str(&s)?)
        } else {
            Ok(Self::default())
        }
    }

    /// Atomic write: write to a .tmp file and rename.
    pub async fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, serde_json::to_string_pretty(self)?).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }

    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

// ── Derived views ─────────────────────────────────────────────────────────────

/// A cleaning unit is either a stand-alone floor or a floor group.
#[derive(Clone, Debug)]
pub struct CleaningUnit {
    pub name: String,
    pub floor_names: Vec<String>,
    pub users: Vec<String>,
    /// Merged and deduplicated rooms from all constituent floors.
    pub rooms: Vec<String>,
    /// Per-floor room lists, preserving attribution for groups.
    /// Each entry is (floor_name, rooms).  Single-floor units have one entry.
    pub floor_rooms: Vec<(String, Vec<String>)>,
}

impl CleaningUnit {
    /// Format rooms for display.
    /// Single floor: "Rooms: Kitchen, Bathroom"
    /// Group with multiple floors: "Rooms:\n  Floor3: …\n  Floor4: …"
    /// Returns None when no rooms are configured.
    pub fn rooms_text(&self) -> Option<String> {
        let has_rooms = self.floor_rooms.iter().any(|(_, r)| !r.is_empty());
        if !has_rooms {
            return None;
        }
        if self.floor_rooms.len() == 1 {
            let rooms = &self.floor_rooms[0].1;
            Some(format!("Rooms: {}", rooms.join(", ")))
        } else {
            let lines: Vec<String> = self.floor_rooms.iter()
                .filter(|(_, r)| !r.is_empty())
                .map(|(floor, rooms)| format!("  {floor}: {}", rooms.join(", ")))
                .collect();
            Some(format!("Rooms:\n{}", lines.join("\n")))
        }
    }
}

impl State {
    /// All cleaning units derived from current floor/group configuration.
    /// Floors that are part of a group are not listed individually.
    pub fn units(&self) -> Vec<CleaningUnit> {
        let grouped: std::collections::HashSet<&str> = self
            .groups
            .iter()
            .flat_map(|g| g.floor_names.iter().map(String::as_str))
            .collect();

        let mut units: Vec<CleaningUnit> = self
            .floors
            .iter()
            .filter(|f| !grouped.contains(f.name.as_str()))
            .map(|f| CleaningUnit {
                name: f.name.clone(),
                floor_names: vec![f.name.clone()],
                users: f.users.clone(),
                rooms: f.rooms.clone(),
                floor_rooms: vec![(f.name.clone(), f.rooms.clone())],
            })
            .collect();

        for group in &self.groups {
            let floor_names = group.floor_names.clone();
            let floors: Vec<&Floor> = group
                .floor_names
                .iter()
                .filter_map(|fn_| self.floors.iter().find(|f| &f.name == fn_))
                .collect();
            let users: Vec<String> = floors.iter()
                .flat_map(|f| f.users.iter().cloned())
                .collect();
            // Merge rooms from all member floors, preserving order and deduplicating.
            let mut rooms: Vec<String> = Vec::new();
            for f in &floors {
                for r in &f.rooms {
                    if !rooms.contains(r) {
                        rooms.push(r.clone());
                    }
                }
            }
            let floor_rooms: Vec<(String, Vec<String>)> = floors.iter()
                .map(|f| (f.name.clone(), f.rooms.clone()))
                .collect();
            units.push(CleaningUnit {
                name: group.name.clone(),
                floor_names,
                users,
                rooms,
                floor_rooms,
            });
        }

        units
    }

    /// Find the cleaning unit a user belongs to (first match).
    pub fn unit_for_user(&self, user: &str) -> Option<CleaningUnit> {
        self.units().into_iter().find(|u| u.users.iter().any(|u2| u2 == user))
    }

    pub fn is_completed(&self, unit: &str, year: i32, week: u32) -> bool {
        self.completions
            .iter()
            .any(|c| c.unit_name == unit && c.iso_year == year && c.iso_week == week)
    }

    /// True if the unit was actually cleaned this week (not just skipped).
    pub fn is_cleaned(&self, unit: &str, year: i32, week: u32) -> bool {
        self.completions.iter().any(|c| {
            c.unit_name == unit && c.iso_year == year && c.iso_week == week && !c.skipped
        })
    }

    /// True if `user_id` is currently marked absent from `floor_name` in the
    /// given week.
    pub fn is_absent(&self, user_id: &str, floor_name: &str, year: i32, week: u32) -> bool {
        self.absences.iter().any(|a| {
            if a.user_id != user_id || a.floor_name != floor_name {
                return false;
            }
            let end = add_weeks(a.from_year, a.from_week, a.duration_weeks as i64);
            let start = (a.from_year, a.from_week);
            (year, week) >= start && (year, week) < end
        })
    }

    /// True if the unit is due for cleaning considering interval_weeks.
    /// Due = no completion in the window [current_week - interval_weeks + 1 … current_week].
    pub fn is_due(&self, unit: &str, year: i32, week: u32, interval_weeks: u32) -> bool {
        for w in 0..interval_weeks {
            let (y, wk) = weeks_ago(year, week, w);
            if self.is_completed(unit, y, wk) {
                return false;
            }
        }
        true
    }

    pub fn reminder_sent(&self, unit: &str, year: i32, week: u32, kind: &ReminderKind) -> bool {
        self.sent_reminders.iter().any(|r| {
            r.unit_name == unit && r.iso_year == year && r.iso_week == week && &r.kind == kind
        })
    }

    pub fn mark_reminder_sent(&mut self, unit: &str, year: i32, week: u32, kind: ReminderKind) {
        self.sent_reminders.push(SentReminder {
            unit_name: unit.to_owned(),
            iso_year: year,
            iso_week: week,
            kind,
            sent_at: Some(chrono::Utc::now()),
        });
    }
}

// ── Statistics helpers ────────────────────────────────────────────────────────

impl State {
    /// The (ISO year, ISO week) from which statistics are measured.
    /// Uses `created_at` if set, falls back to the earliest completion, then
    /// current week.
    pub fn tracking_start(&self) -> (i32, u32) {
        if let Some(dt) = self.created_at {
            let d = dt.date_naive();
            return (d.iso_week().year(), d.iso_week().week());
        }
        if let Some(c) = self.completions.iter().min_by_key(|c| c.completed_at) {
            let d = c.completed_at.date_naive();
            return (d.iso_week().year(), d.iso_week().week());
        }
        current_iso_week()
    }

    /// Every (year, week) pair that is a "due week" for a unit with the given
    /// interval, from `tracking_start` up to and including `(end_year, end_week)`.
    pub fn all_due_weeks(&self, interval_weeks: u32, end: (i32, u32)) -> Vec<(i32, u32)> {
        let start = self.tracking_start();
        all_due_weeks_in_range(start, end, interval_weeks)
    }

    /// Due weeks that have already ended (strictly before the current week) and
    /// have no completion recorded — i.e. the unit was skipped.
    pub fn missed_weeks_for(&self, unit: &str, interval_weeks: u32) -> Vec<(i32, u32)> {
        let current = current_iso_week();
        self.all_due_weeks(interval_weeks, current)
            .into_iter()
            .filter(|&(y, w)| {
                // Exclude the current (still ongoing) week and skipped weeks
                (y, w) != current && !self.is_completed(unit, y, w)
            })
            .collect()
    }

    /// Number of consecutive completed due-weeks ending at the most recent one.
    /// Ignores the current week if not yet completed (still ongoing).
    pub fn streak_for(&self, unit: &str, interval_weeks: u32) -> u32 {
        let current = current_iso_week();
        let due = self.all_due_weeks(interval_weeks, current);
        let mut streak = 0u32;
        for &(y, w) in due.iter().rev() {
            // Current week is still open — skip if not yet cleaned
            if (y, w) == current && !self.is_cleaned(unit, y, w) {
                continue;
            }
            // Skipped weeks don't break the streak but don't extend it either
            if self.is_completed(unit, y, w) && !self.is_cleaned(unit, y, w) {
                continue; // skipped — neutral
            }
            if self.is_cleaned(unit, y, w) {
                streak += 1;
            } else {
                break;
            }
        }
        streak
    }

    /// Which single user is responsible for cleaning `unit` in `(year, week)`.
    ///
    /// An accepted swap for that unit/week overrides the rotation.  Otherwise
    /// the responsible user is determined by round-robin through `unit.users`,
    /// indexed by the position of `(year, week)` in the sequence of due weeks
    /// counted from `tracking_start`.  Returns `None` when the unit has no users.
    pub fn responsible_user(
        &self,
        unit: &CleaningUnit,
        year: i32,
        week: u32,
        interval: u32,
    ) -> Option<String> {
        if unit.users.is_empty() {
            return None;
        }
        // An accepted swap overrides the rotation for this specific week.
        if let Some(swap) = self.swap_requests.iter().find(|s| {
            s.unit_name == unit.name
                && s.iso_year == year
                && s.iso_week == week
                && s.status == SwapStatus::Accepted
        }) {
            return Some(swap.target.clone());
        }
        let start = self.tracking_start();
        // Number of due weeks from start up to and including (year, week).
        let n = all_due_weeks_in_range(start, (year, week), interval).len();
        let idx = n.saturating_sub(1); // 0-based index of this due week
        unit.users.get(idx % unit.users.len()).cloned()
    }

    /// Most recent completion for a unit, if any.
    pub fn last_completed(&self, unit: &str) -> Option<&Completion> {
        self.completions
            .iter()
            .filter(|c| c.unit_name == unit)
            .max_by_key(|c| c.completed_at)
    }
}

// ── Week arithmetic ───────────────────────────────────────────────────────────

/// Return the (ISO year, ISO week) that is `n` weeks before (year, week).
pub fn weeks_ago(year: i32, week: u32, n: u32) -> (i32, u32) {
    if n == 0 {
        return (year, week);
    }
    let monday = NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 4).unwrap());
    let earlier = monday - chrono::Duration::weeks(n as i64);
    (earlier.iso_week().year(), earlier.iso_week().week())
}

/// Add `n` weeks to (year, week).
pub fn add_weeks(year: i32, week: u32, n: i64) -> (i32, u32) {
    let monday = NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 4).unwrap());
    let later = monday + chrono::Duration::weeks(n);
    (later.iso_week().year(), later.iso_week().week())
}

/// Number of weeks between two (year, week) pairs (signed; negative if from > to).
pub fn weeks_between(from: (i32, u32), to: (i32, u32)) -> i64 {
    let from_d = NaiveDate::from_isoywd_opt(from.0, from.1, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(from.0, 1, 4).unwrap());
    let to_d = NaiveDate::from_isoywd_opt(to.0, to.1, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(to.0, 1, 4).unwrap());
    (to_d - from_d).num_weeks()
}

/// Every due-week in [start … end] inclusive, stepping by interval_weeks.
fn all_due_weeks_in_range(
    start: (i32, u32),
    end: (i32, u32),
    interval_weeks: u32,
) -> Vec<(i32, u32)> {
    let total = weeks_between(start, end);
    if total < 0 {
        return vec![];
    }
    let mut result = Vec::new();
    let mut n = 0i64;
    while n <= total {
        result.push(add_weeks(start.0, start.1, n));
        n += interval_weeks as i64;
    }
    result
}

/// Return the current (ISO year, ISO week) in UTC.
pub fn current_iso_week() -> (i32, u32) {
    let now = Utc::now().date_naive();
    (now.iso_week().year(), now.iso_week().week())
}

/// Format an ISO week as a human-readable date span, e.g. "26 May – 1 Jun".
/// Useful so users don't have to look up what "week 22/2025" means.
pub fn week_dates(year: i32, week: u32) -> String {
    let mon = NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 4).unwrap());
    let sun = mon + chrono::Duration::days(6);
    // Cross-month: show month on both sides; same-month: abbreviate.
    if mon.month() == sun.month() {
        format!("{} – {} {}", mon.format("%-d"), sun.format("%-d"), mon.format("%b"))
    } else {
        format!("{} – {}", mon.format("%-d %b"), sun.format("%-d %b"))
    }
}
