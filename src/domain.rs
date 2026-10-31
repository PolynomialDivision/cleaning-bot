//! Stable domain types.
//!
//! All scheduling references use opaque UUIDs (PersonId, GroupId).
//! Display names and Matrix IDs are metadata only, never used as identity keys.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Type aliases ──────────────────────────────────────────────────────────────

pub type PersonId = String;
pub type GroupId = String;
pub type SlotId = String;

// ── Person ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Person {
    pub id: PersonId,
    pub display_name: String,
    pub active: bool,
    /// If set, this person has a Matrix account and can send commands. With
    /// `[directory]`, the account the bot writes to them at: their chosen one
    /// once they chose (`directory::reconcile`).
    #[serde(default)]
    pub matrix_id: Option<String>,
    /// Who this person is in the house's directory (the identity provider's
    /// id of them, Authentik's `uuid`), once the bot matched them
    /// (`directory`). `None`: never followed automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_id: Option<String>,
}

impl Person {
    pub fn new_matrix(mxid: &str) -> Self {
        // Use localpart as the initial display name so PDF/ICS show "bomberdomme"
        // instead of "@bomberdomme:matrix.org".  Updated to the real Matrix display
        // name when the person first sends a command.
        let display_name = mxid
            .strip_prefix('@')
            .and_then(|s| s.split(':').next())
            .map(str::to_owned)
            .unwrap_or_else(|| mxid.to_owned());
        Person {
            id: Uuid::new_v4().to_string(),
            display_name,
            active: true,
            matrix_id: Some(mxid.to_owned()),
            directory_id: None,
        }
    }
    pub fn new_named(name: &str) -> Self {
        Person {
            id: Uuid::new_v4().to_string(),
            display_name: name.to_owned(),
            active: true,
            matrix_id: None,
            directory_id: None,
        }
    }
    /// True when `s` matches this person's MXID or display name (case-insensitive).
    pub fn matches(&self, s: &str) -> bool {
        self.matrix_id.as_deref() == Some(s) || self.display_name.eq_ignore_ascii_case(s)
    }
}

// ── CleaningSlot ──────────────────────────────────────────────────────────────

/// A named sub-unit within a multi-slot `CleaningGroup`.
///
/// When a group has `slots`, each cycle assigns one person per slot using
/// consecutive members from the rotation:
///   slot_index s, due-week index n → member[(n × num_slots + s) % num_members]
///
/// If `slots` is empty the group behaves as a traditional single-slot group.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CleaningSlot {
    pub id: SlotId,
    pub name: String,
    #[serde(default)]
    pub room_names: Vec<String>,
    /// Per-room workload multipliers; absent keys default to 1.0.
    #[serde(default)]
    pub room_weights: std::collections::HashMap<String, f64>,
    /// Per-slot workload multiplier on top of room weights (default 1.0).
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// The place this slot cleans when several people clean it together:
    /// `!groups slot add Staircase Scharni x2` makes the slots "Scharni 1"
    /// and "Scharni 2", both at place "Scharni". Each person still has a
    /// slot of their own (their reminder, their ✅, their swap); plans and
    /// the website show the place once with everyone on it. `None`: the
    /// slot is a place of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
}

impl CleaningSlot {
    pub fn new(name: &str) -> Self {
        CleaningSlot {
            id: Uuid::new_v4().to_string(),
            name: name.to_owned(),
            room_names: Vec::new(),
            room_weights: std::collections::HashMap::new(),
            weight: 1.0,
            place: None,
        }
    }

    /// Whether a command naming `name` means this slot: its own name or its
    /// place's ("Scharni" for "Scharni 1").
    pub fn answers_to(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
            || self
                .place
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case(name))
    }

    /// Where this slot cleans: its place, else its own name.
    pub fn place_name(&self) -> &str {
        self.place.as_deref().unwrap_or(&self.name)
    }
}

// ── CleaningGroup ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CleaningGroup {
    pub id: GroupId,
    pub name: String,
    /// Current members of this group. Order is cosmetic (used only to seed
    /// `rotation_queue` for a brand-new group). The *turn* order lives in
    /// `rotation_queue`, not here.
    pub member_ids: Vec<PersonId>,
    /// Persisted round-robin turn queue: front = who gets the next
    /// not-yet-frozen due week. `resolver::materialize` pops one member per
    /// slot per due week and pushes them back to the tail. Joining re-seats
    /// the queue from the next rotation cycle on, with newcomers spread
    /// through that cycle (see `commands::apply_group_join`); leaving simply
    /// drops them. Either way the running cycle stays as it was instead of
    /// reshuffling everyone via a stateless `index % member_count` formula.
    /// `#[serde(default)]` so existing `state.json` files upgrade seamlessly:
    /// `resolver::reconcile_queue` self-heals/seeds it on first use.
    #[serde(default)]
    pub rotation_queue: Vec<PersonId>,
    /// Used in single-slot mode (slots is empty).
    #[serde(default)]
    pub room_names: Vec<String>,
    /// Per-room workload multipliers for single-slot mode; absent keys default to 1.0.
    #[serde(default)]
    pub room_weights: std::collections::HashMap<String, f64>,
    /// When non-empty, enables multi-slot mode.
    #[serde(default)]
    pub slots: Vec<CleaningSlot>,
    /// Group-level workload multiplier stacked on top of room weights (default 1.0).
    /// Use `!groups weight` to adjust for groups heavier/lighter than their
    /// room count suggests (e.g. kitchen = 2.0, storage = 0.5).
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// When false the group is excluded from scheduling, statistics, and all
    /// bot output.  Defaults to true so existing state upgrades seamlessly.
    #[serde(default = "default_active")]
    pub is_active: bool,
    /// How often the group is cleaned and how each due week is split into
    /// turns (see `rhythm`).
    #[serde(default)]
    pub rhythm: crate::rhythm::Rhythm,
    /// The directory group (`kitchen-floor-1`) this group follows
    /// (`directory::reconcile`): its members join, who leaves it leaves.
    /// `None`: members choose themselves (`!join`, the selector).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_group: Option<String>,
    /// A house-wide group (the staircase): every resident is a member,
    /// whatever their floor groups, and the bot keeps `member_ids` so
    /// (`house::sync`): a resident is everyone active in another active
    /// group, plus `house_added`, minus `house_excused`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub house_wide: bool,
    /// Excused from this house-wide group by an admin (`!member remove`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub house_excused: Vec<PersonId>,
    /// In this house-wide group though in no other group (`!member add`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub house_added: Vec<PersonId>,
    /// How many people the rotation has drawn ahead of someone to spare them
    /// a clash with their other duties (`resolver`): so nobody is put off for
    /// long, and the places are made up on their next turn. Cleared once
    /// they are drawn.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub rotation_deferred: std::collections::BTreeMap<PersonId, u32>,
}

fn default_active() -> bool {
    true
}

/// Default workload multiplier. No adjustment.
pub fn default_weight() -> f64 {
    1.0
}

impl CleaningGroup {
    pub fn new(name: &str) -> Self {
        CleaningGroup {
            id: Uuid::new_v4().to_string(),
            name: name.to_owned(),
            member_ids: Vec::new(),
            rotation_queue: Vec::new(),
            room_names: Vec::new(),
            room_weights: std::collections::HashMap::new(),
            slots: Vec::new(),
            weight: 1.0,
            is_active: true,
            rhythm: crate::rhythm::Rhythm::weekly(),
            directory_group: None,
            house_wide: false,
            house_excused: Vec::new(),
            house_added: Vec::new(),
            rotation_deferred: std::collections::BTreeMap::new(),
        }
    }

    /// How much work one turn of `slot_index` is, in room equivalents:
    /// its rooms (with their weights) × the slot's weight × the group's.
    /// What the rotation balances across groups (`resolver::Workload`).
    pub fn duty_load(&self, slot_index: usize) -> f64 {
        let (rooms, weights, slot_weight) = match self.slots.get(slot_index) {
            Some(s) => (&s.room_names, &s.room_weights, s.weight),
            None => (&self.room_names, &self.room_weights, 1.0),
        };
        crate::analytics::effective_rooms_pub(rooms, weights) * slot_weight * self.weight
    }

    pub fn is_multi_slot(&self) -> bool {
        !self.slots.is_empty()
    }

    pub fn slot_by_name(&self, name: &str) -> Option<&CleaningSlot> {
        self.slots
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
    }

    /// The slot named `name`, else every slot at place `name`
    /// (`CleaningSlot::place`), in order: what a command naming a slot means.
    pub fn slots_at(&self, name: &str) -> Vec<SlotId> {
        match self.slot_by_name(name) {
            Some(slot) => vec![slot.id.clone()],
            None => self
                .slots
                .iter()
                .filter(|s| {
                    s.place
                        .as_deref()
                        .is_some_and(|p| p.eq_ignore_ascii_case(name))
                })
                .map(|s| s.id.clone())
                .collect(),
        }
    }

    pub fn slot_by_id(&self, id: &SlotId) -> Option<&CleaningSlot> {
        self.slots.iter().find(|s| &s.id == id)
    }

    pub fn slot_by_id_mut(&mut self, id: &SlotId) -> Option<&mut CleaningSlot> {
        self.slots.iter_mut().find(|s| &s.id == id)
    }
}

// ── Slot assignment resolver types ───────────────────────────────────────────

/// How a `SlotAssignment` was created. Purely descriptive/audit metadata.
/// Nothing in the scheduling or eligibility logic branches on it; it only
/// answers "why does this person clean this week" for humans reading
/// `!status`, the pinned plan, or the event log.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentSource {
    /// Produced by `resolver::materialize` filling a not-yet-frozen due week.
    #[default]
    RoundRobin,
    /// Legacy tag from before sources were split out below: `!plan assign`,
    /// `!takeover` and accepted swaps all wrote this. Kept only so an
    /// existing `state.json` / event log with old `"manual"` entries still
    /// deserializes; nothing writes it anymore.
    Manual,
    /// Admin override (`!plan assign`, `!plan unassign`).
    Assign,
    /// Self-service handoff (`!takeover`).
    Takeover,
    /// An accepted `!swap`.
    Swap,
    /// One-time migration of a remaining assignment from the old paper plan
    /// (`!plan import`).
    Import,
}

/// A frozen assignment: one person (or nobody) is responsible for one group/slot
/// in one specific due week.  Once stored, never overwritten except via an
/// explicit `SlotAssigned` event (e.g. when the assigned person leaves).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SlotAssignment {
    pub group_id: GroupId,
    /// 0 for single-slot groups; slot index into `CleaningGroup.slots` for multi-slot.
    pub slot_index: usize,
    pub iso_year: i32,
    pub iso_week: u32,
    /// Shift within the week (see `rhythm`); 0 for whole-week rhythms.
    #[serde(default)]
    pub shift: u8,
    /// `None` = unassigned (person left or group has no members).
    pub person_id: Option<PersonId>,
    #[serde(default)]
    pub source: AssignmentSource,
}

// ── Deterministic assignment UID ──────────────────────────────────────────────

/// Fixed namespace UUID for this bot, never changes.
const ASSIGNMENT_NS: Uuid = Uuid::from_bytes([
    0xc1, 0xea, 0x3b, 0x07, 0xc1, 0xea, 0x3b, 0x07, 0xc1, 0xea, 0x3b, 0x07, 0xc1, 0xea, 0x3b, 0x07,
]);

/// Stable UUID v5 for a (group, \[slot,\] year, week, assignee) tuple.
/// Identical inputs always produce the same UID. No storage required.
pub fn assignment_uid(group_id: &str, year: i32, week: u32, person_id: &str) -> String {
    let name = format!("{group_id}:{year}:W{week:02}:{person_id}");
    Uuid::new_v5(&ASSIGNMENT_NS, name.as_bytes()).to_string()
}

pub fn slot_assignment_uid(
    group_id: &str,
    slot_id: &str,
    year: i32,
    week: u32,
    person_id: &str,
) -> String {
    let name = format!("{group_id}:{slot_id}:{year}:W{week:02}:{person_id}");
    Uuid::new_v5(&ASSIGNMENT_NS, name.as_bytes()).to_string()
}
