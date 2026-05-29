use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc, Weekday};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, HashSet}, path::Path};

use crate::domain::{CleaningGroup, GroupId, Person, PersonId};

// ── Scheduling records ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Completion {
    // ── New stable fields ────────────────────────────────────────────────────
    #[serde(default)]
    pub group_id: GroupId,
    #[serde(default)]
    pub completed_by_id: PersonId,
    #[serde(default)]
    pub responsible_person_ids: Vec<PersonId>,

    pub iso_year:     i32,
    pub iso_week:     u32,
    pub completed_at: DateTime<Utc>,
    #[serde(default)]
    pub skipped: bool,

    // ── Legacy (read from old JSON; empty after migration + save) ────────────
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit_name:         String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub completed_by:      String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub responsible_users: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReactionDone {
    #[serde(default)]
    pub group_id:        GroupId,
    #[serde(default)]
    pub completed_by_id: PersonId,
    pub iso_year:  i32,
    pub iso_week:  u32,

    // Legacy
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit_name:    String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub completed_by: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Absence {
    #[serde(default)]
    pub person_id: PersonId,
    #[serde(default)]
    pub group_id:  GroupId,
    pub from_year:      i32,
    pub from_week:      u32,
    pub duration_weeks: u32,

    // Legacy
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user_id:    String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub floor_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SwapRequest {
    pub id:        u64,
    /// Requester / target are Matrix MXIDs — swaps are Matrix-only.
    pub requester: String,
    pub target:    String,
    #[serde(default)]
    pub group_id:  GroupId,
    pub iso_year:  i32,
    pub iso_week:  u32,
    pub created_at: DateTime<Utc>,
    pub status:     SwapStatus,

    // Legacy
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SwapStatus { Pending, Accepted, Rejected, Cancelled }

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReminderKind { Initial, Final, WeeklySummary }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SentReminder {
    #[serde(default)]
    pub group_id: GroupId,
    pub iso_year: i32,
    pub iso_week: u32,
    pub kind:     ReminderKind,
    #[serde(default)]
    pub sent_at:  Option<DateTime<Utc>>,

    // Legacy
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit_name: String,
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

// Re-export CalendarToken so it lives next to State.
pub use crate::domain::CalendarToken;

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    // ── New domain model ─────────────────────────────────────────────────────
    #[serde(default)]
    pub persons:         Vec<Person>,
    #[serde(default)]
    pub cleaning_groups: Vec<CleaningGroup>,
    #[serde(default)]
    pub calendar_tokens: Vec<CalendarToken>,

    // ── Scheduling records ────────────────────────────────────────────────────
    #[serde(default)]
    pub completions:    Vec<Completion>,
    #[serde(default)]
    pub swap_requests:  Vec<SwapRequest>,
    #[serde(default)]
    pub absences:       Vec<Absence>,
    #[serde(default)]
    pub sent_reminders: Vec<SentReminder>,

    // ── Bot bookkeeping ───────────────────────────────────────────────────────
    #[serde(default)]
    pub next_id:    u64,
    pub created_at:    Option<DateTime<Utc>>,
    /// Timestamp of the last successful `save()` call.
    /// Used by the schedule pipeline as a deterministic DTSTAMP for ICS events.
    #[serde(default)]
    pub last_modified: Option<DateTime<Utc>>,
    /// event_id → group_id  (was unit_name before migration)
    #[serde(default)]
    pub reminder_event_ids: HashMap<String, String>,
    #[serde(default)]
    pub reaction_dones:     HashMap<String, ReactionDone>,
    #[serde(default)]
    pub greeting_event_ids: HashMap<String, GreetingInfo>,
    #[serde(default)]
    pub greeted_users:      HashSet<String>,

    // ── Legacy (Floor/FloorGroup) — read-only for migration ──────────────────
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub floors: Vec<LegacyFloor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<LegacyFloorGroup>,
}

// ── Legacy types (deserialization only) ──────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct LegacyFloor {
    pub name: String,
    #[serde(default)]
    pub members: Vec<LegacyPersonRef>,
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub rooms: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LegacyPersonRef {
    Matrix { mxid: String },
    Named  { name: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LegacyFloorGroup {
    pub name:        String,
    pub floor_names: Vec<String>,
}

// ── Load / Save / Migrate ─────────────────────────────────────────────────────

impl State {
    pub async fn load(path: &Path) -> Result<Self> {
        if tokio::fs::metadata(path).await.is_ok() {
            let s = tokio::fs::read_to_string(path).await?;
            let mut st: Self = serde_json::from_str(&s)?;
            st.migrate();
            Ok(st)
        } else {
            Ok(Self::default())
        }
    }

    /// Atomically persist state to disk, updating `last_modified` automatically.
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

    fn migrate(&mut self) {
        // Step 1: normalise legacy Floor.users → Floor.members.
        for floor in &mut self.floors {
            if floor.members.is_empty() && !floor.users.is_empty() {
                floor.members = floor.users.drain(..)
                    .map(|u| LegacyPersonRef::Matrix { mxid: u })
                    .collect();
            }
        }

        // Step 2: floors + floor-groups → Person + CleaningGroup.
        if self.persons.is_empty() && !self.floors.is_empty() {
            // Create Person records for every unique member ref.
            for floor in self.floors.clone().iter() {
                for r in &floor.members {
                    let already = match r {
                        LegacyPersonRef::Matrix { mxid } =>
                            self.persons.iter().any(|p| p.matrix_id.as_deref() == Some(mxid)),
                        LegacyPersonRef::Named { name } =>
                            self.persons.iter().any(|p| p.display_name.eq_ignore_ascii_case(name)),
                    };
                    if !already {
                        let person = match r {
                            LegacyPersonRef::Matrix { mxid } => Person::new_matrix(mxid),
                            LegacyPersonRef::Named  { name  } => Person::new_named(name),
                        };
                        self.persons.push(person);
                    }
                }
            }

            let grouped: HashSet<String> = self.groups.iter()
                .flat_map(|g| g.floor_names.iter().cloned())
                .collect();

            // Stand-alone floors → one CleaningGroup each.
            for floor in self.floors.clone().iter().filter(|f| !grouped.contains(&f.name)) {
                let mut cg = CleaningGroup::new(&floor.name);
                cg.room_names = floor.rooms.clone();
                cg.member_ids = floor.members.iter()
                    .filter_map(|r| self.person_by_legacy_ref(r).map(|p| p.id.clone()))
                    .collect();
                self.cleaning_groups.push(cg);
            }

            // FloorGroups → one merged CleaningGroup.
            for lg in self.groups.clone().iter() {
                let mut cg = CleaningGroup::new(&lg.name);
                let mut seen: HashSet<String> = HashSet::new();
                for floor_name in &lg.floor_names {
                    if let Some(floor) = self.floors.iter().find(|f| &f.name == floor_name) {
                        for room in &floor.rooms {
                            if !cg.room_names.contains(room) { cg.room_names.push(room.clone()); }
                        }
                        for r in &floor.members {
                            if let Some(p) = self.person_by_legacy_ref(r) {
                                if seen.insert(p.id.clone()) { cg.member_ids.push(p.id.clone()); }
                            }
                        }
                    }
                }
                self.cleaning_groups.push(cg);
            }

            self.floors.clear();
            self.groups.clear();
        }

        // Step 3–8: migrate string-keyed records to UUID-keyed records.
        // Clone groups/persons vecs to avoid borrow conflict inside loops.
        let grps = self.cleaning_groups.clone();
        let prs  = self.persons.clone();

        let find_group  = |name: &str| grps.iter().find(|g| g.name == name).map(|g| g.id.clone());
        let find_person = |s: &str| prs.iter().find(|p| p.matrix_id.as_deref() == Some(s) || p.display_name.eq_ignore_ascii_case(s)).map(|p| p.id.clone());

        for c in &mut self.completions {
            if c.group_id.is_empty()        { if let Some(id) = find_group(&c.unit_name)       { c.group_id        = id; c.unit_name.clear(); } }
            if c.completed_by_id.is_empty() { if let Some(id) = find_person(&c.completed_by)   { c.completed_by_id = id; c.completed_by.clear(); } }
            if c.responsible_person_ids.is_empty() {
                c.responsible_person_ids = c.responsible_users.iter().filter_map(|s| find_person(s)).collect();
                if !c.responsible_person_ids.is_empty() { c.responsible_users.clear(); }
            }
        }
        for s in &mut self.swap_requests {
            if s.group_id.is_empty() { if let Some(id) = find_group(&s.unit_name) { s.group_id = id; s.unit_name.clear(); } }
        }
        for a in &mut self.absences {
            if a.person_id.is_empty() { if let Some(id) = find_person(&a.user_id)    { a.person_id = id; a.user_id.clear(); } }
            if a.group_id.is_empty()  { if let Some(id) = find_group(&a.floor_name)  { a.group_id  = id; a.floor_name.clear(); } }
        }
        for r in &mut self.sent_reminders {
            if r.group_id.is_empty() { if let Some(id) = find_group(&r.unit_name) { r.group_id = id; r.unit_name.clear(); } }
        }
        for val in self.reminder_event_ids.values_mut() {
            // Heuristic: if value doesn't look like a UUID, it's a group name.
            if !val.contains('-') || val.len() < 32 {
                if let Some(id) = find_group(val) { *val = id; }
            }
        }
        for rd in self.reaction_dones.values_mut() {
            if rd.group_id.is_empty()        { if let Some(id) = find_group(&rd.unit_name)      { rd.group_id        = id; rd.unit_name.clear(); } }
            if rd.completed_by_id.is_empty() { if let Some(id) = find_person(&rd.completed_by)  { rd.completed_by_id = id; rd.completed_by.clear(); } }
        }
    }

    fn person_by_legacy_ref(&self, r: &LegacyPersonRef) -> Option<&Person> {
        match r {
            LegacyPersonRef::Matrix { mxid } =>
                self.persons.iter().find(|p| p.matrix_id.as_deref() == Some(mxid)),
            LegacyPersonRef::Named { name } =>
                self.persons.iter().find(|p| p.display_name.eq_ignore_ascii_case(name)),
        }
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
    /// Find by PersonId, MXID, or display name.
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
            group_id: group_id.to_owned(), iso_year: year, iso_week: week,
            kind, sent_at: Some(Utc::now()), unit_name: String::new(),
        });
    }

    /// Round-robin responsible person. Accepted swaps override the rotation.
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
