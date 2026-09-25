use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc, Weekday};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::OnceLock,
};

use crate::{
    domain::{CleaningGroup, CleaningSlot, GroupId, Person, PersonId, SlotAssignment, SlotId},
    rhythm::Turn,
};

// ── Scheduling records ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Completion {
    pub group_id: GroupId,
    /// `Some` for multi-slot groups; `None` for single-slot groups.
    #[serde(default)]
    pub slot_id: Option<SlotId>,
    pub completed_by_id: PersonId,
    #[serde(default)]
    pub responsible_person_ids: Vec<PersonId>,
    pub iso_year: i32,
    pub iso_week: u32,
    /// Shift within the week (see `rhythm`); 0 for whole-week rhythms.
    #[serde(default)]
    pub shift: u8,
    pub completed_at: DateTime<Utc>,
    #[serde(default)]
    pub skipped: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReactionDone {
    pub group_id: GroupId,
    pub completed_by_id: PersonId,
    pub iso_year: i32,
    pub iso_week: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Absence {
    pub person_id: PersonId,
    pub group_id: GroupId,
    pub from_year: i32,
    pub from_week: u32,
    pub duration_weeks: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SwapRequest {
    pub id: u64,
    /// Requester / target are Matrix MXIDs — swaps are Matrix-only.
    pub requester: String,
    pub target: String,
    pub group_id: GroupId,
    pub iso_year: i32,
    pub iso_week: u32,
    pub created_at: DateTime<Utc>,
    pub status: SwapStatus,
    /// Slot being swapped in a group with slots (0 otherwise).
    #[serde(default)]
    pub slot_index: usize,
    /// Shift within the week being swapped.
    #[serde(default)]
    pub shift: u8,
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
    WeeklySummary,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SentReminder {
    /// A group id, or `*` for the consolidated weekly plan.
    pub group_id: GroupId,
    pub iso_year: i32,
    pub iso_week: u32,
    /// Shift the reminder was for (per-turn reminders).
    #[serde(default)]
    pub shift: u8,
    pub kind: ReminderKind,
    #[serde(default)]
    pub sent_at: Option<DateTime<Utc>>,
}

// ── Greeting ──────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GreetingChoice {
    pub emoji: String,
    pub group_id: GroupId,
    pub group_name: String,
    /// Set when this choice links the joining user to an existing non-Matrix person.
    #[serde(default)]
    pub person_id: Option<PersonId>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GreetingInfo {
    pub for_user: String,
    pub choices: Vec<GreetingChoice>,
    /// True during the identity-linking step (choosing which non-Matrix placeholder you are).
    #[serde(default)]
    pub is_linking: bool,
}

pub use crate::domain::CalendarToken;

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    #[serde(default)]
    pub persons: Vec<Person>,
    #[serde(default)]
    pub cleaning_groups: Vec<CleaningGroup>,
    #[serde(default)]
    pub calendar_tokens: Vec<CalendarToken>,

    /// Frozen per-slot assignments produced by the resolver.
    /// These are the authoritative source for who cleans which slot each week.
    #[serde(default)]
    pub slot_assignments: Vec<SlotAssignment>,
    #[serde(default)]
    pub completions: Vec<Completion>,
    #[serde(default)]
    pub swap_requests: Vec<SwapRequest>,
    #[serde(default)]
    pub absences: Vec<Absence>,
    #[serde(default)]
    pub sent_reminders: Vec<SentReminder>,
    /// Append-only domain event log.  Backfilled from completions/swaps on first load.
    #[serde(default)]
    pub event_log: Vec<crate::analytics::LoggedEvent>,

    #[serde(default)]
    pub next_id: u64,
    pub created_at: Option<DateTime<Utc>>,
    /// Updated on every save; used as deterministic DTSTAMP in ICS exports.
    #[serde(default)]
    pub last_modified: Option<DateTime<Utc>>,
    /// event_id → (iso_year, iso_week)  (consolidated weekly plan / final-reminder messages)
    #[serde(default)]
    pub weekly_plan_event_ids: HashMap<String, (i32, u32)>,
    /// "YEAR-Www" → canonical weekly plan event_id (initial plan only; used for pin/unpin and idempotency)
    #[serde(default)]
    pub weekly_plan_canonical: HashMap<String, String>,
    /// "YEAR-Www" → the exact message body last successfully sent/edited into
    /// that week's canonical plan message. Used to detect drift between
    /// persisted state and the live Matrix message (e.g. after a crash
    /// between applying a domain event and editing the message) without
    /// having to parse Matrix edit-aggregation data back out of the room —
    /// state stays the sole source of truth; Matrix is only ever read to
    /// confirm the event still exists. `#[serde(default)]` so old
    /// `state.json` files without it just re-render (and re-check) on the
    /// next startup instead of failing to load.
    #[serde(default)]
    pub weekly_plan_rendered: HashMap<String, String>,
    #[serde(default)]
    pub reaction_dones: HashMap<String, ReactionDone>,
    #[serde(default)]
    pub greeting_event_ids: HashMap<String, GreetingInfo>,
    #[serde(default)]
    pub greeted_users: HashSet<String>,
}

// ── Load / Save ───────────────────────────────────────────────────────────────

impl State {
    pub async fn load(path: &Path) -> Result<Self> {
        let mut st: Self = mxbot_common::persist::load_json_or_default(path).await?;
        // Backfill event log from existing data on first load after upgrade.
        if st.event_log.is_empty() && !st.completions.is_empty() {
            st.event_log = crate::analytics::backfill_events(&st);
        }
        Ok(st)
    }

    /// Apply a domain event: mutate state **and** append to the event log.
    ///
    /// This is the **only** permitted way to mutate domain state (persons,
    /// groups, completions, swaps, absences).  Bot-infrastructure state
    /// (reaction_dones, greeted_users, calendar_tokens)
    /// may still be mutated directly.
    ///
    /// Returns `Ok(())` whether the event caused a state change or was a
    /// no-op (idempotent).  Returns `Err` only for structural violations
    /// such as referencing a non-existent group or person.
    /// Low-level: append an already-applied backfill event without mutating state.
    /// Only called from `analytics::backfill_events` bootstrapping path.
    #[allow(dead_code)]
    pub(crate) fn append_event(&mut self, event: crate::analytics::LoggedEvent) {
        self.event_log.push(event);
    }

    pub fn apply_event(&mut self, event: crate::analytics::DomainEvent) -> anyhow::Result<()> {
        use crate::analytics::DomainEvent as E;

        let changed = match &event {
            // ── Groups ────────────────────────────────────────────────────────
            E::GroupCreated { group_id, name } => {
                if self.cleaning_groups.iter().any(|g| &g.id == group_id) {
                    return Ok(());
                }
                let mut g = CleaningGroup::new(name);
                g.id = group_id.clone();
                self.cleaning_groups.push(g);
                true
            }
            E::GroupDeleted { group_id } => {
                let before = self.cleaning_groups.len();
                self.cleaning_groups.retain(|g| &g.id != group_id);
                if self.cleaning_groups.len() == before {
                    anyhow::bail!("Group not found: {group_id}");
                }
                // Nothing may keep pointing at the deleted group: its plan,
                // history, absences, swaps and reminder bookkeeping go with it
                // (`!groups disable` is the way to keep them).
                self.slot_assignments.retain(|a| &a.group_id != group_id);
                self.completions.retain(|c| &c.group_id != group_id);
                self.absences.retain(|a| &a.group_id != group_id);
                self.swap_requests.retain(|s| &s.group_id != group_id);
                self.sent_reminders.retain(|r| &r.group_id != group_id);
                self.reaction_dones.retain(|_, rd| &rd.group_id != group_id);
                true
            }
            E::GroupDisabled { group_id } => {
                let g = self
                    .cleaning_groups
                    .iter_mut()
                    .find(|g| &g.id == group_id)
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                if !g.is_active {
                    return Ok(());
                }
                g.is_active = false;
                true
            }
            E::GroupEnabled { group_id } => {
                let g = self
                    .cleaning_groups
                    .iter_mut()
                    .find(|g| &g.id == group_id)
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                if g.is_active {
                    return Ok(());
                }
                g.is_active = true;
                true
            }
            E::SlotAdded {
                group_id,
                slot_id,
                slot_name,
            } => {
                let g = self
                    .cleaning_groups
                    .iter_mut()
                    .find(|g| &g.id == group_id)
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                if g.slots.iter().any(|s| &s.id == slot_id) {
                    return Ok(());
                }
                let mut s = CleaningSlot::new(slot_name);
                s.id = slot_id.clone();
                g.slots.push(s);
                true
            }
            E::SlotRemoved { group_id, slot_id } => {
                let removed_index = {
                    let g = self
                        .cleaning_groups
                        .iter_mut()
                        .find(|g| &g.id == group_id)
                        .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                    match g.slots.iter().position(|s| &s.id == slot_id) {
                        Some(idx) => {
                            g.slots.remove(idx);
                            idx
                        }
                        None => return Ok(()),
                    }
                };
                // `SlotAssignment.slot_index` is a raw position into
                // `group.slots`, not a stable id — removing a slot shifts
                // every later slot down by one, so an untouched frozen
                // assignment would silently start referring to the wrong
                // slot. Drop the (now homeless) assignments for the removed
                // slot FIRST, using their original index, then re-point the
                // survivors at the same physical slot — reversing the order
                // would shift a later assignment down onto `removed_index`
                // and have it deleted by the very next step.
                self.slot_assignments
                    .retain(|a| !(a.group_id == *group_id && a.slot_index == removed_index));
                for a in self.slot_assignments.iter_mut() {
                    if a.group_id == *group_id && a.slot_index > removed_index {
                        a.slot_index -= 1;
                    }
                }
                true
            }
            E::SlotAssigned {
                group_id,
                slot_index,
                iso_year,
                iso_week,
                shift,
                person_id,
                source,
                actor_id: _,
                previous_person_id: _,
            } => {
                // Upsert: replace any existing assignment for this (group, slot, turn).
                self.slot_assignments.retain(|a| {
                    !(a.group_id == *group_id
                        && a.slot_index == *slot_index
                        && (a.iso_year, a.iso_week, a.shift) == (*iso_year, *iso_week, *shift))
                });
                self.slot_assignments.push(SlotAssignment {
                    group_id: group_id.clone(),
                    slot_index: *slot_index,
                    iso_year: *iso_year,
                    iso_week: *iso_week,
                    shift: *shift,
                    person_id: person_id.clone(),
                    source: source.clone(),
                });
                true
            }
            E::RhythmSet { group_id, rhythm } => {
                let g = self
                    .cleaning_groups
                    .iter_mut()
                    .find(|g| &g.id == group_id)
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                if &g.rhythm == rhythm {
                    return Ok(());
                }
                g.rhythm = rhythm.clone();
                true
            }
            E::RoomAdded {
                group_id,
                slot_id,
                room_name,
            } => {
                let g = self
                    .cleaning_groups
                    .iter_mut()
                    .find(|g| &g.id == group_id)
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                match slot_id {
                    Some(sid) => {
                        let s = g
                            .slots
                            .iter_mut()
                            .find(|s| &s.id == sid)
                            .ok_or_else(|| anyhow::anyhow!("Slot not found: {sid}"))?;
                        if s.room_names
                            .iter()
                            .any(|r| r.eq_ignore_ascii_case(room_name))
                        {
                            return Ok(());
                        }
                        s.room_names.push(room_name.clone());
                    }
                    None => {
                        if g.room_names
                            .iter()
                            .any(|r| r.eq_ignore_ascii_case(room_name))
                        {
                            return Ok(());
                        }
                        g.room_names.push(room_name.clone());
                    }
                }
                true
            }
            E::RoomRemoved {
                group_id,
                slot_id,
                room_name,
            } => {
                let g = self
                    .cleaning_groups
                    .iter_mut()
                    .find(|g| &g.id == group_id)
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                match slot_id {
                    Some(sid) => {
                        let s = g
                            .slots
                            .iter_mut()
                            .find(|s| &s.id == sid)
                            .ok_or_else(|| anyhow::anyhow!("Slot not found: {sid}"))?;
                        let before = s.room_names.len();
                        s.room_names.retain(|r| !r.eq_ignore_ascii_case(room_name));
                        s.room_names.len() < before
                    }
                    None => {
                        let before = g.room_names.len();
                        g.room_names.retain(|r| !r.eq_ignore_ascii_case(room_name));
                        g.room_names.len() < before
                    }
                }
            }

            // ── Persons ───────────────────────────────────────────────────────
            E::PersonCreated {
                person_id,
                display_name,
                matrix_id,
            } => {
                if self.persons.iter().any(|p| &p.id == person_id) {
                    return Ok(());
                }
                if let Some(mxid) = matrix_id {
                    if self
                        .persons
                        .iter()
                        .any(|p| p.matrix_id.as_deref() == Some(mxid))
                    {
                        return Ok(());
                    }
                }
                let mut p = match matrix_id.as_deref() {
                    Some(mxid) => Person::new_matrix(mxid),
                    None => Person::new_named(display_name),
                };
                p.id = person_id.clone();
                self.persons.push(p);
                true
            }
            E::PersonJoinedGroup {
                person_id,
                group_id,
            } => {
                if !self.persons.iter().any(|p| &p.id == person_id) {
                    anyhow::bail!("Person not found: {person_id}");
                }
                let g = self
                    .cleaning_groups
                    .iter_mut()
                    .find(|g| &g.id == group_id)
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                if g.member_ids.contains(person_id) {
                    return Ok(());
                }
                g.member_ids.push(person_id.clone());
                true
            }
            E::PersonLeftGroup {
                person_id,
                group_id,
            } => {
                let g = self
                    .cleaning_groups
                    .iter_mut()
                    .find(|g| &g.id == group_id)
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                let before = g.member_ids.len();
                g.member_ids.retain(|id| id != person_id);
                g.member_ids.len() < before
            }
            E::RotationQueueSet { group_id, queue } => {
                let g = self
                    .cleaning_groups
                    .iter_mut()
                    .find(|g| &g.id == group_id)
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                if &g.rotation_queue == queue {
                    return Ok(());
                }
                g.rotation_queue = queue.clone();
                true
            }
            E::RoomWeightSet {
                group_id,
                slot_id,
                room_name,
                weight,
            } => {
                let g = self
                    .cleaning_groups
                    .iter_mut()
                    .find(|g| &g.id == group_id)
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                let weights = match slot_id {
                    Some(sid) => {
                        let s = g
                            .slots
                            .iter_mut()
                            .find(|s| &s.id == sid)
                            .ok_or_else(|| anyhow::anyhow!("Slot not found: {sid}"))?;
                        &mut s.room_weights
                    }
                    None => &mut g.room_weights,
                };
                if weights.get(room_name).copied() == Some(*weight) {
                    return Ok(());
                }
                weights.insert(room_name.clone(), *weight);
                true
            }
            E::GroupWeightSet { group_id, weight } => {
                let g = self
                    .cleaning_groups
                    .iter_mut()
                    .find(|g| &g.id == group_id)
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                if (g.weight - weight).abs() < f64::EPSILON {
                    return Ok(());
                }
                g.weight = *weight;
                true
            }
            E::PersonMatrixLinked {
                person_id,
                matrix_id,
            } => {
                if self
                    .persons
                    .iter()
                    .any(|p| p.matrix_id.as_deref() == Some(matrix_id) && &p.id != person_id)
                {
                    anyhow::bail!("{matrix_id} is already linked to another person");
                }
                let p = self
                    .persons
                    .iter_mut()
                    .find(|p| &p.id == person_id)
                    .ok_or_else(|| anyhow::anyhow!("Person not found: {person_id}"))?;
                if p.matrix_id.as_deref() == Some(matrix_id) {
                    return Ok(());
                }
                p.matrix_id = Some(matrix_id.clone());
                true
            }

            // ── Cleaning ──────────────────────────────────────────────────────
            E::CleaningCompleted {
                group_id,
                slot_id,
                person_id,
                responsible_person_ids,
                iso_year,
                iso_week,
                shift,
            } => {
                let turn = Turn::new(*iso_year, *iso_week, *shift);
                // Idempotency: this slot (or, without slots, the group) of
                // this turn already has a record.
                let already_done = self.completions.iter().any(|c| {
                    &c.group_id == group_id
                        && (c.iso_year, c.iso_week, c.shift) == (turn.year, turn.week, turn.shift)
                        && (slot_id.is_none() || c.slot_id == *slot_id)
                });
                if !self.cleaning_groups.iter().any(|g| &g.id == group_id) {
                    anyhow::bail!("Group not found: {group_id}");
                }
                if already_done {
                    return Ok(());
                }
                self.completions.push(Completion {
                    group_id: group_id.clone(),
                    slot_id: slot_id.clone(),
                    completed_by_id: person_id.clone(),
                    responsible_person_ids: responsible_person_ids.clone(),
                    iso_year: *iso_year,
                    iso_week: *iso_week,
                    shift: *shift,
                    completed_at: Utc::now(),
                    skipped: false,
                });
                true
            }
            E::CleaningSkipped {
                group_id,
                skipper_id,
                iso_year,
                iso_week,
                slot_id,
                shift,
            } => {
                let group = self
                    .group_by_id(group_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Group not found: {group_id}"))?;
                // Skip every open (slot, shift) of the week — or just the
                // given slot and/or shift.
                let shifts: Vec<u8> = match shift {
                    Some(s) => vec![*s],
                    None => (0..group.rhythm.shift_count() as u8).collect(),
                };
                let slots: Vec<Option<SlotId>> = if group.is_multi_slot() {
                    group
                        .slots
                        .iter()
                        .map(|s| Some(s.id.clone()))
                        .filter(|id| slot_id.is_none() || id == slot_id)
                        .collect()
                } else {
                    vec![None]
                };
                let mut changed = false;
                for sh in shifts {
                    for sid in &slots {
                        let done = self.completions.iter().any(|c| {
                            &c.group_id == group_id
                                && (c.iso_year, c.iso_week, c.shift) == (*iso_year, *iso_week, sh)
                                && (sid.is_none() || c.slot_id == *sid)
                        });
                        if done {
                            continue;
                        }
                        self.completions.push(Completion {
                            group_id: group_id.clone(),
                            slot_id: sid.clone(),
                            completed_by_id: skipper_id.clone(),
                            responsible_person_ids: vec![],
                            iso_year: *iso_year,
                            iso_week: *iso_week,
                            shift: sh,
                            completed_at: Utc::now(),
                            skipped: true,
                        });
                        changed = true;
                    }
                }
                changed
            }
            E::CleaningUndone {
                group_id,
                iso_year,
                iso_week,
                slot_id,
                shift,
            } => {
                let before = self.completions.len();
                self.completions.retain(|c| {
                    !(&c.group_id == group_id
                        && c.iso_year == *iso_year
                        && c.iso_week == *iso_week
                        && (shift.is_none() || Some(c.shift) == *shift)
                        && (slot_id.is_none() || c.slot_id == *slot_id))
                });
                self.completions.len() < before
            }

            // ── Swaps ─────────────────────────────────────────────────────────
            E::SwapRequested {
                group_id,
                requester_mxid,
                target_mxid,
                iso_year,
                iso_week,
                slot_index,
                shift,
            } => {
                if !self.cleaning_groups.iter().any(|g| &g.id == group_id) {
                    anyhow::bail!("Group not found: {group_id}");
                }
                let id = self.next_id;
                self.next_id += 1;
                self.swap_requests.push(SwapRequest {
                    id,
                    requester: requester_mxid.clone(),
                    target: target_mxid.clone(),
                    group_id: group_id.clone(),
                    iso_year: *iso_year,
                    iso_week: *iso_week,
                    created_at: Utc::now(),
                    status: SwapStatus::Pending,
                    slot_index: *slot_index,
                    shift: *shift,
                });
                true
            }
            E::SwapApproved { swap_id, .. } => {
                let req = self
                    .swap_requests
                    .iter_mut()
                    .find(|r| r.id == *swap_id)
                    .ok_or_else(|| anyhow::anyhow!("Swap not found: {swap_id}"))?;
                if req.status == SwapStatus::Accepted {
                    return Ok(());
                }
                if req.status != SwapStatus::Pending {
                    anyhow::bail!("Swap #{swap_id} is not pending");
                }
                req.status = SwapStatus::Accepted;
                true
            }
            E::SwapRejected { swap_id } => {
                let req = self
                    .swap_requests
                    .iter_mut()
                    .find(|r| r.id == *swap_id)
                    .ok_or_else(|| anyhow::anyhow!("Swap not found: {swap_id}"))?;
                if req.status == SwapStatus::Rejected {
                    return Ok(());
                }
                if req.status != SwapStatus::Pending {
                    anyhow::bail!("Swap #{swap_id} is not pending");
                }
                req.status = SwapStatus::Rejected;
                true
            }

            // ── Absences ──────────────────────────────────────────────────────
            E::AbsenceRecorded {
                person_id,
                group_id,
                from_year,
                from_week,
                duration_weeks,
            } => {
                if !self.persons.iter().any(|p| &p.id == person_id) {
                    anyhow::bail!("Person not found: {person_id}");
                }
                if !self.cleaning_groups.iter().any(|g| &g.id == group_id) {
                    anyhow::bail!("Group not found: {group_id}");
                }
                self.absences
                    .retain(|a| !(&a.person_id == person_id && &a.group_id == group_id));
                self.absences.push(Absence {
                    person_id: person_id.clone(),
                    group_id: group_id.clone(),
                    from_year: *from_year,
                    from_week: *from_week,
                    duration_weeks: *duration_weeks,
                });
                true
            }
            E::AbsenceCancelled { person_id } => {
                let before = self.absences.len();
                self.absences.retain(|a| &a.person_id != person_id);
                self.absences.len() < before
            }
        };

        if changed {
            self.event_log
                .push(crate::analytics::LoggedEvent::now(event));
        }
        Ok(())
    }

    pub async fn save(&mut self, path: &Path) -> Result<()> {
        self.last_modified = Some(Utc::now());
        mxbot_common::persist::save_json_atomic(path, self).await
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
        self.persons
            .iter()
            .find(|p| p.matrix_id.as_deref() == Some(mxid))
    }
    pub fn person_by_id(&self, id: &PersonId) -> Option<&Person> {
        self.persons.iter().find(|p| &p.id == id)
    }
    pub fn find_person(&self, query: &str) -> Option<&Person> {
        self.persons
            .iter()
            .find(|p| p.id == query || p.matches(query))
    }
    pub fn group_by_name(&self, name: &str) -> Option<&CleaningGroup> {
        self.cleaning_groups
            .iter()
            .find(|g| g.name.eq_ignore_ascii_case(name))
    }
    pub fn group_by_name_mut(&mut self, name: &str) -> Option<&mut CleaningGroup> {
        self.cleaning_groups
            .iter_mut()
            .find(|g| g.name.eq_ignore_ascii_case(name))
    }
    pub fn group_by_id(&self, id: &GroupId) -> Option<&CleaningGroup> {
        self.cleaning_groups.iter().find(|g| &g.id == id)
    }
    pub fn groups_for_person(&self, person_id: &PersonId) -> Vec<&CleaningGroup> {
        self.cleaning_groups
            .iter()
            .filter(|g| g.member_ids.contains(person_id))
            .collect()
    }
    pub fn members_of<'a>(&'a self, group: &CleaningGroup) -> Vec<&'a Person> {
        group
            .member_ids
            .iter()
            .filter_map(|id| self.person_by_id(id))
            .collect()
    }
}

// ── Scheduling queries ────────────────────────────────────────────────────────
//
// Everything is expressed in `Turn`s (one shift of one due week, see
// `rhythm`) and each group's own rhythm — nothing here assumes that a turn
// is a calendar week or that all groups share one interval.

impl State {
    /// True when `group` is cleaned in this ISO week: every
    /// `rhythm.every_weeks` weeks, counted from the tracking start.
    pub fn is_due_week(&self, group: &CleaningGroup, year: i32, week: u32) -> bool {
        let every = group.rhythm.every_weeks() as i64;
        let offset = weeks_between(self.tracking_start(), (year, week));
        offset.rem_euclid(every) == 0
    }

    /// The group's turns in this week — one per shift, none in a week it
    /// isn't due.
    pub fn turns_in_week(&self, group: &CleaningGroup, year: i32, week: u32) -> Vec<Turn> {
        if !self.is_due_week(group, year, week) {
            return Vec::new();
        }
        (0..group.rhythm.shift_count() as u8)
            .map(|shift| Turn::new(year, week, shift))
            .collect()
    }

    /// All turns of `group` in the weeks `from..=to`, in order.
    pub fn turns_between(
        &self,
        group: &CleaningGroup,
        from: (i32, u32),
        to: (i32, u32),
    ) -> Vec<Turn> {
        let weeks = weeks_between(from, to);
        (0..=weeks.max(-1))
            .flat_map(|i| {
                let (y, w) = add_weeks(from.0, from.1, i);
                self.turns_in_week(group, y, w)
            })
            .collect()
    }

    /// First week at or after `from` in which `group` is due.
    pub fn next_due_week(&self, group: &CleaningGroup, from: (i32, u32)) -> (i32, u32) {
        let every = group.rhythm.every_weeks() as i64;
        let offset = weeks_between(self.tracking_start(), from);
        let ahead = (every - offset.rem_euclid(every)) % every;
        add_weeks(from.0, from.1, ahead)
    }

    /// The turn running today, if the group is due this week.
    pub fn current_turn(&self, group: &CleaningGroup) -> Option<Turn> {
        let today = today();
        let (year, week) = (today.iso_week().year(), today.iso_week().week());
        if !self.is_due_week(group, year, week) {
            return None;
        }
        let weekday = today.weekday().num_days_from_monday() as u8;
        Some(Turn::new(
            year,
            week,
            group.rhythm.shift_for_weekday(weekday),
        ))
    }

    /// The turn has begun (its first day is today or earlier).
    pub fn turn_started(&self, group: &CleaningGroup, turn: Turn) -> bool {
        turn.dates(&group.rhythm).0 <= today()
    }

    /// The turn is over (its last day was before today).
    pub fn turn_over(&self, group: &CleaningGroup, turn: Turn) -> bool {
        turn.dates(&group.rhythm).1 < today()
    }

    /// The done/skipped record of one slot of one turn.
    pub fn completion_for(
        &self,
        group: &CleaningGroup,
        slot_index: usize,
        turn: Turn,
    ) -> Option<&Completion> {
        let slot_id = group.slots.get(slot_index).map(|s| s.id.as_str());
        self.completions.iter().find(|c| {
            c.group_id == group.id
                && (c.iso_year, c.iso_week, c.shift) == (turn.year, turn.week, turn.shift)
                && (!group.is_multi_slot() || c.slot_id.as_deref() == slot_id)
        })
    }

    pub fn is_turn_slot_done(&self, group: &CleaningGroup, slot_index: usize, turn: Turn) -> bool {
        self.completion_for(group, slot_index, turn).is_some()
    }

    /// Slot indices of a group: `0..slots` (just 0 for a group without slots).
    pub fn slot_indices(group: &CleaningGroup) -> std::ops::Range<usize> {
        0..group.slots.len().max(1)
    }

    /// Every slot of the turn is done or skipped.
    pub fn is_turn_done(&self, group: &CleaningGroup, turn: Turn) -> bool {
        Self::slot_indices(group).all(|i| self.is_turn_slot_done(group, i, turn))
    }

    /// Every slot of the turn was actually cleaned (none skipped).
    pub fn is_turn_cleaned(&self, group: &CleaningGroup, turn: Turn) -> bool {
        Self::slot_indices(group).all(|i| {
            self.completion_for(group, i, turn)
                .is_some_and(|c| !c.skipped)
        })
    }

    /// Every turn of the group in this week is done or skipped (false in a
    /// week the group isn't due).
    pub fn is_completed(&self, group_id: &GroupId, year: i32, week: u32) -> bool {
        let Some(group) = self.group_by_id(group_id) else {
            return false;
        };
        let turns = self.turns_in_week(group, year, week);
        !turns.is_empty() && turns.iter().all(|t| self.is_turn_done(group, *t))
    }

    /// The group appears in that week's plan message.
    pub fn belongs_in_weekly_plan(&self, group: &CleaningGroup, year: i32, week: u32) -> bool {
        group.is_active && self.is_due_week(group, year, week)
    }

    pub fn is_absent(
        &self,
        person_id: &PersonId,
        group_id: &GroupId,
        year: i32,
        week: u32,
    ) -> bool {
        self.absences.iter().any(|a| {
            if &a.person_id != person_id || &a.group_id != group_id {
                return false;
            }
            let end = add_weeks(a.from_year, a.from_week, a.duration_weeks as i64);
            (year, week) >= (a.from_year, a.from_week) && (year, week) < end
        })
    }

    pub fn reminder_sent(
        &self,
        group_id: &str,
        year: i32,
        week: u32,
        shift: u8,
        kind: &ReminderKind,
    ) -> bool {
        self.sent_reminders.iter().any(|r| {
            r.group_id == group_id
                && (r.iso_year, r.iso_week, r.shift) == (year, week, shift)
                && &r.kind == kind
        })
    }

    pub fn mark_reminder_sent(
        &mut self,
        group_id: &str,
        year: i32,
        week: u32,
        shift: u8,
        kind: ReminderKind,
    ) {
        self.sent_reminders.push(SentReminder {
            group_id: group_id.to_owned(),
            iso_year: year,
            iso_week: week,
            shift,
            kind,
            sent_at: Some(Utc::now()),
        });
    }

    /// Who is responsible for one slot (0 without slots) of one turn: the
    /// frozen plan first; past its horizon, a preview of the rotation that
    /// agrees with what `resolver::materialize` will freeze.
    pub fn slot_assignee(
        &self,
        group: &CleaningGroup,
        slot_index: usize,
        turn: Turn,
    ) -> Option<&Person> {
        if let Some(a) = self.slot_assignments.iter().find(|a| {
            a.group_id == group.id
                && a.slot_index == slot_index
                && (a.iso_year, a.iso_week, a.shift) == (turn.year, turn.week, turn.shift)
        }) {
            return a.person_id.as_ref().and_then(|pid| self.person_by_id(pid));
        }
        if group.member_ids.is_empty() {
            return None;
        }
        // Swaps accepted before swaps froze their result as an assignment.
        if let Some(swap) = self.swap_requests.iter().find(|s| {
            s.group_id == group.id
                && s.slot_index == slot_index
                && (s.iso_year, s.iso_week, s.shift) == (turn.year, turn.week, turn.shift)
                && s.status == SwapStatus::Accepted
        }) {
            return self.person_by_matrix_id(&swap.target);
        }
        crate::resolver::preview_slot_assignee(self, group, slot_index, turn)
            .and_then(|pid| self.person_by_id(&pid))
    }

    /// Every slot of the turn with its assignee.
    pub fn turn_assignees<'a>(
        &'a self,
        group: &CleaningGroup,
        turn: Turn,
    ) -> Vec<(usize, Option<&'a Person>)> {
        Self::slot_indices(group)
            .map(|i| (i, self.slot_assignee(group, i, turn)))
            .collect()
    }

    /// The slots of the turn held by `person_id`.
    pub fn held_slots(
        &self,
        group: &CleaningGroup,
        person_id: &PersonId,
        turn: Turn,
    ) -> Vec<usize> {
        Self::slot_indices(group)
            .filter(|&i| {
                self.slot_assignee(group, i, turn)
                    .is_some_and(|p| &p.id == person_id)
            })
            .collect()
    }

    /// Test helper: some shift of the week has a record for this slot.
    #[cfg(test)]
    pub fn is_slot_completed(
        &self,
        group_id: &GroupId,
        slot_id: &SlotId,
        year: i32,
        week: u32,
    ) -> bool {
        self.completions.iter().any(|c| {
            &c.group_id == group_id
                && c.slot_id.as_deref() == Some(slot_id.as_str())
                && (c.iso_year, c.iso_week) == (year, week)
        })
    }

    pub fn last_completion(&self, group_id: &GroupId) -> Option<&Completion> {
        self.completions
            .iter()
            .filter(|c| &c.group_id == group_id)
            .max_by_key(|c| c.completed_at)
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

    /// Turns of the group from the tracking start that are over.
    pub fn closed_turns(&self, group: &CleaningGroup) -> Vec<Turn> {
        self.turns_between(group, self.tracking_start(), current_iso_week())
            .into_iter()
            .filter(|t| self.turn_over(group, *t))
            .collect()
    }

    /// Closed turns in which some slot was neither done nor skipped.
    pub fn missed_turns(&self, group: &CleaningGroup) -> Vec<Turn> {
        self.closed_turns(group)
            .into_iter()
            .filter(|t| !self.is_turn_done(group, *t))
            .collect()
    }

    /// Consecutive most recent turns that were fully cleaned; skipped turns
    /// neither count nor break it, a running turn only counts once cleaned.
    pub fn streak_for(&self, group: &CleaningGroup) -> u32 {
        let mut streak = 0u32;
        for turn in self
            .turns_between(group, self.tracking_start(), current_iso_week())
            .into_iter()
            .rev()
        {
            if self.is_turn_cleaned(group, turn) {
                streak += 1;
            } else if self.is_turn_done(group, turn) || !self.turn_over(group, turn) {
                continue;
            } else {
                break;
            }
        }
        streak
    }
}

// ── Week arithmetic ───────────────────────────────────────────────────────────

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
/// The configured wall-clock timezone (`schedule.timezone`), set once at
/// startup by `main` before any command runs. Everything that decides "what
/// week is it" — `!done`, `!plan skip`, materialize, the scheduler tick, etc. —
/// goes through `current_iso_week()`, so this is the single place that needs
/// to know about local time rather than threading `Config` through every
/// call site. Falls back to UTC when unset (e.g. in unit tests), which keeps
/// existing UTC-based test expectations valid.
static TIMEZONE: OnceLock<chrono_tz::Tz> = OnceLock::new();

/// Must be called at most once, before the first `current_iso_week()` call
/// that should observe it (i.e. very early in `main`, right after the config
/// is parsed). Later calls are ignored — matches the "set once at startup"
/// contract.
pub fn set_timezone(tz: chrono_tz::Tz) {
    let _ = TIMEZONE.set(tz);
}

/// The ISO week for "now", in the configured local timezone rather than raw
/// UTC. This matters right around the weekly rollover: for a UTC+1/+2
/// timezone like Europe/Berlin, local midnight Monday is still Sunday
/// evening in UTC, so a naive `Utc::now()`-based week would consider the
/// week not yet rolled over for up to two hours after users already
/// experience it as the new week (including across a bot restart timed
/// exactly then).
pub fn current_iso_week() -> (i32, u32) {
    let tz = TIMEZONE.get().copied().unwrap_or(chrono_tz::UTC);
    iso_week_at(Utc::now(), tz)
}

/// Today's date in the configured local timezone (see `current_iso_week`).
pub fn today() -> NaiveDate {
    let tz = TIMEZONE.get().copied().unwrap_or(chrono_tz::UTC);
    Utc::now().with_timezone(&tz).date_naive()
}

/// Pure helper behind `current_iso_week()`, split out so the timezone
/// behaviour is unit-testable without touching the process-global
/// `TIMEZONE` (which, being a `OnceLock`, can only ever be set once per test
/// binary run and would otherwise leak into every other test).
fn iso_week_at(now: DateTime<Utc>, tz: chrono_tz::Tz) -> (i32, u32) {
    let d = now.with_timezone(&tz).date_naive();
    (d.iso_week().year(), d.iso_week().week())
}
pub fn week_dates(year: i32, week: u32) -> String {
    let mon = NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 4).unwrap());
    let sun = mon + chrono::Duration::days(6);
    if mon.month() == sun.month() {
        format!(
            "{} – {} {}",
            mon.format("%-d"),
            sun.format("%-d"),
            mon.format("%b")
        )
    } else {
        format!("{} – {}", mon.format("%-d %b"), sun.format("%-d %b"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_week_follows_local_midnight_not_utc_midnight() {
        // Sunday 23:30 UTC in January is already Monday 00:30 CET in
        // Europe/Berlin (UTC+1) — a new ISO week for anyone actually living
        // there, even though UTC itself hasn't rolled over yet. Before this
        // fix, `current_iso_week()` always used raw UTC, so for up to two
        // hours (CEST in summer) around every weekly rollover, `!done`,
        // `!takeover`, materialize, and a bot restart landing in that window
        // would all disagree with what week users experience locally.
        let sunday_2330_utc: DateTime<Utc> = "2024-01-14T23:30:00Z".parse().unwrap();
        assert_eq!(
            iso_week_at(sunday_2330_utc, chrono_tz::UTC),
            (2024, 2),
            "still week 2 in UTC itself"
        );
        assert_eq!(
            iso_week_at(sunday_2330_utc, chrono_tz::Europe::Berlin),
            (2024, 3),
            "already week 3 in Europe/Berlin"
        );
    }

    #[test]
    fn a_group_stays_in_its_weekly_plan_once_done_and_follows_its_rhythm() {
        // A completed group must stay visible (as done) in the pinned plan,
        // and a group cleaned every second week only belongs to its weeks.
        let mut state = State::default();
        state.created_at = Some("2024-03-04T12:00:00Z".parse().unwrap()); // week 10
        let mut group = CleaningGroup::new("Hall");
        let (year, week) = (2024, 10);
        state.cleaning_groups.push(group.clone());
        assert!(state.belongs_in_weekly_plan(&group, year, week));

        state.completions.push(Completion {
            group_id: group.id.clone(),
            slot_id: None,
            completed_by_id: "p1".into(),
            responsible_person_ids: vec!["p1".into()],
            iso_year: year,
            iso_week: week,
            shift: 0,
            completed_at: Utc::now(),
            skipped: false,
        });
        assert!(state.is_completed(&group.id, year, week));
        assert!(state.belongs_in_weekly_plan(&group, year, week));

        group.rhythm.every_weeks = Some(2);
        assert!(state.belongs_in_weekly_plan(&group, year, week));
        assert!(!state.belongs_in_weekly_plan(&group, year, week + 1));
        assert!(state.belongs_in_weekly_plan(&group, year, week + 2));
        assert_eq!(
            state.next_due_week(&group, (year, week + 1)),
            (year, week + 2)
        );
    }
}
