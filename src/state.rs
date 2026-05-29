use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc, Weekday};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, HashSet}, path::Path};

use crate::domain::{CleaningGroup, GroupId, Person, PersonId};

// ── Scheduling records ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Completion {
    pub group_id:               GroupId,
    pub completed_by_id:        PersonId,
    #[serde(default)]
    pub responsible_person_ids: Vec<PersonId>,
    pub iso_year:     i32,
    pub iso_week:     u32,
    pub completed_at: DateTime<Utc>,
    #[serde(default)]
    pub skipped: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReactionDone {
    pub group_id:        GroupId,
    pub completed_by_id: PersonId,
    pub iso_year: i32,
    pub iso_week: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Absence {
    pub person_id:      PersonId,
    pub group_id:       GroupId,
    pub from_year:      i32,
    pub from_week:      u32,
    pub duration_weeks: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SwapRequest {
    pub id:         u64,
    /// Requester / target are Matrix MXIDs — swaps are Matrix-only.
    pub requester:  String,
    pub target:     String,
    pub group_id:   GroupId,
    pub iso_year:   i32,
    pub iso_week:   u32,
    pub created_at: DateTime<Utc>,
    pub status:     SwapStatus,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SwapStatus { Pending, Accepted, Rejected, Cancelled }

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReminderKind { Initial, Final, WeeklySummary }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SentReminder {
    pub group_id: GroupId,
    pub iso_year: i32,
    pub iso_week: u32,
    pub kind:     ReminderKind,
    #[serde(default)]
    pub sent_at:  Option<DateTime<Utc>>,
}

// ── Greeting ──────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GreetingChoice {
    pub emoji:      String,
    pub group_id:   GroupId,
    pub group_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GreetingInfo {
    pub for_user: String,
    pub choices:  Vec<GreetingChoice>,
}

pub use crate::domain::CalendarToken;

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    #[serde(default)]
    pub persons:         Vec<Person>,
    #[serde(default)]
    pub cleaning_groups: Vec<CleaningGroup>,
    #[serde(default)]
    pub calendar_tokens: Vec<CalendarToken>,

    #[serde(default)]
    pub completions:    Vec<Completion>,
    #[serde(default)]
    pub swap_requests:  Vec<SwapRequest>,
    #[serde(default)]
    pub absences:       Vec<Absence>,
    #[serde(default)]
    pub sent_reminders: Vec<SentReminder>,

    #[serde(default)]
    pub next_id:       u64,
    pub created_at:    Option<DateTime<Utc>>,
    /// Updated on every save; used as deterministic DTSTAMP in ICS exports.
    #[serde(default)]
    pub last_modified: Option<DateTime<Utc>>,
    /// event_id → group_id
    #[serde(default)]
    pub reminder_event_ids: HashMap<String, GroupId>,
    #[serde(default)]
    pub reaction_dones:     HashMap<String, ReactionDone>,
    #[serde(default)]
    pub greeting_event_ids: HashMap<String, GreetingInfo>,
    #[serde(default)]
    pub greeted_users:      HashSet<String>,
}

// ── Load / Save ───────────────────────────────────────────────────────────────

impl State {
    pub async fn load(path: &Path) -> Result<Self> {
        if tokio::fs::metadata(path).await.is_ok() {
            let s = tokio::fs::read_to_string(path).await?;
            Ok(serde_json::from_str(&s)?)
        } else {
            Ok(Self::default())
        }
    }

    pub async fn save(&mut self, path: &Path) -> Result<()> {
        self.last_modified = Some(Utc::now());
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

// ── Domain lookups ────────────────────────────────────────────────────────────

impl State {
    pub fn person_by_matrix_id(&self, mxid: &str) -> Option<&Person> {
        self.persons.iter().find(|p| p.matrix_id.as_deref() == Some(mxid))
    }
    pub fn person_by_id(&self, id: &PersonId) -> Option<&Person> {
        self.persons.iter().find(|p| &p.id == id)
    }
    pub fn find_person(&self, query: &str) -> Option<&Person> {
        self.persons.iter().find(|p| p.id == query || p.matches(query))
    }
    pub fn group_by_name(&self, name: &str) -> Option<&CleaningGroup> {
        self.cleaning_groups.iter().find(|g| g.name.eq_ignore_ascii_case(name))
    }
    pub fn group_by_name_mut(&mut self, name: &str) -> Option<&mut CleaningGroup> {
        self.cleaning_groups.iter_mut().find(|g| g.name.eq_ignore_ascii_case(name))
    }
    pub fn group_by_id(&self, id: &GroupId) -> Option<&CleaningGroup> {
        self.cleaning_groups.iter().find(|g| &g.id == id)
    }
    pub fn groups_for_person(&self, person_id: &PersonId) -> Vec<&CleaningGroup> {
        self.cleaning_groups.iter().filter(|g| g.member_ids.contains(person_id)).collect()
    }
    pub fn members_of<'a>(&'a self, group: &CleaningGroup) -> Vec<&'a Person> {
        group.member_ids.iter().filter_map(|id| self.person_by_id(id)).collect()
    }
}

// ── Scheduling queries ────────────────────────────────────────────────────────

impl State {
    pub fn is_completed(&self, group_id: &GroupId, year: i32, week: u32) -> bool {
        self.completions.iter().any(|c| &c.group_id == group_id && c.iso_year == year && c.iso_week == week)
    }
    pub fn is_cleaned(&self, group_id: &GroupId, year: i32, week: u32) -> bool {
        self.completions.iter().any(|c| &c.group_id == group_id && c.iso_year == year && c.iso_week == week && !c.skipped)
    }
    pub fn is_due(&self, group_id: &GroupId, year: i32, week: u32, interval: u32) -> bool {
        (0..interval).all(|w| {
            let (y, wk) = weeks_ago(year, week, w);
            !self.is_completed(group_id, y, wk)
        })
    }
    pub fn is_absent(&self, person_id: &PersonId, group_id: &GroupId, year: i32, week: u32) -> bool {
        self.absences.iter().any(|a| {
            if &a.person_id != person_id || &a.group_id != group_id { return false; }
            let end = add_weeks(a.from_year, a.from_week, a.duration_weeks as i64);
            (year, week) >= (a.from_year, a.from_week) && (year, week) < end
        })
    }
    pub fn reminder_sent(&self, group_id: &str, year: i32, week: u32, kind: &ReminderKind) -> bool {
        self.sent_reminders.iter().any(|r| r.group_id == group_id && r.iso_year == year && r.iso_week == week && &r.kind == kind)
    }
    pub fn mark_reminder_sent(&mut self, group_id: &str, year: i32, week: u32, kind: ReminderKind) {
        self.sent_reminders.push(SentReminder {
            group_id: group_id.to_owned(),
            iso_year: year,
            iso_week: week,
            kind,
            sent_at:  Some(Utc::now()),
        });
    }

    pub fn responsible_person(&self, group: &CleaningGroup, year: i32, week: u32, interval: u32) -> Option<&Person> {
        if group.member_ids.is_empty() { return None; }
        if let Some(swap) = self.swap_requests.iter().find(|s| {
            s.group_id == group.id && s.iso_year == year && s.iso_week == week && s.status == SwapStatus::Accepted
        }) {
            return self.person_by_matrix_id(&swap.target);
        }
        let start = self.tracking_start();
        let n = all_due_weeks_in_range(start, (year, week), interval).len();
        let idx = n.saturating_sub(1) % group.member_ids.len();
        self.person_by_id(&group.member_ids[idx])
    }

    pub fn last_completion(&self, group_id: &GroupId) -> Option<&Completion> {
        self.completions.iter().filter(|c| &c.group_id == group_id).max_by_key(|c| c.completed_at)
    }
}

// ── Statistics ────────────────────────────────────────────────────────────────

impl State {
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
    pub fn all_due_weeks(&self, interval: u32, end: (i32, u32)) -> Vec<(i32, u32)> {
        all_due_weeks_in_range(self.tracking_start(), end, interval)
    }
    pub fn missed_weeks_for(&self, group_id: &GroupId, interval: u32) -> Vec<(i32, u32)> {
        let cur = current_iso_week();
        self.all_due_weeks(interval, cur).into_iter()
            .filter(|&(y, w)| (y, w) != cur && !self.is_completed(group_id, y, w))
            .collect()
    }
    pub fn streak_for(&self, group_id: &GroupId, interval: u32) -> u32 {
        let cur = current_iso_week();
        let mut streak = 0u32;
        for &(y, w) in self.all_due_weeks(interval, cur).iter().rev() {
            if (y, w) == cur && !self.is_cleaned(group_id, y, w) { continue; }
            if self.is_completed(group_id, y, w) && !self.is_cleaned(group_id, y, w) { continue; }
            if self.is_cleaned(group_id, y, w) { streak += 1; } else { break; }
        }
        streak
    }
}

// ── Week arithmetic ───────────────────────────────────────────────────────────

pub fn weeks_ago(year: i32, week: u32, n: u32) -> (i32, u32) {
    if n == 0 { return (year, week); }
    let mon = NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 4).unwrap());
    let d = mon - chrono::Duration::weeks(n as i64);
    (d.iso_week().year(), d.iso_week().week())
}
pub fn add_weeks(year: i32, week: u32, n: i64) -> (i32, u32) {
    let mon = NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 4).unwrap());
    let d = mon + chrono::Duration::weeks(n);
    (d.iso_week().year(), d.iso_week().week())
}
pub fn weeks_between(from: (i32, u32), to: (i32, u32)) -> i64 {
    let a = NaiveDate::from_isoywd_opt(from.0, from.1, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(from.0, 1, 4).unwrap());
    let b = NaiveDate::from_isoywd_opt(to.0, to.1, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(to.0, 1, 4).unwrap());
    (b - a).num_weeks()
}
pub fn all_due_weeks_in_range(start: (i32, u32), end: (i32, u32), interval: u32) -> Vec<(i32, u32)> {
    let total = weeks_between(start, end);
    if total < 0 { return vec![]; }
    (0..).map(|n| add_weeks(start.0, start.1, n * interval as i64))
         .take_while(|&w| weeks_between(start, w) <= total)
         .collect()
}
pub fn current_iso_week() -> (i32, u32) {
    let d = Utc::now().date_naive();
    (d.iso_week().year(), d.iso_week().week())
}
pub fn week_dates(year: i32, week: u32) -> String {
    let mon = NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 4).unwrap());
    let sun = mon + chrono::Duration::days(6);
    if mon.month() == sun.month() {
        format!("{} – {} {}", mon.format("%-d"), sun.format("%-d"), mon.format("%b"))
    } else {
        format!("{} – {}", mon.format("%-d %b"), sun.format("%-d %b"))
    }
}
