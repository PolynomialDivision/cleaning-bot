//! Stable domain types.
//!
//! All scheduling references use opaque UUIDs (PersonId, GroupId).
//! Display names and Matrix IDs are metadata only — never used as identity keys.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

// ── Type aliases ──────────────────────────────────────────────────────────────

pub type PersonId = String;
pub type GroupId  = String;
pub type SlotId   = String;

// ── Person ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Person {
    pub id:           PersonId,
    pub display_name: String,
    pub active:       bool,
    /// If set, this person has a Matrix account and can send commands.
    #[serde(default)]
    pub matrix_id:    Option<String>,
}

impl Person {
    pub fn new_matrix(mxid: &str) -> Self {
        // Use localpart as the initial display name so PDF/ICS show "bomberdomme"
        // instead of "@bomberdomme:matrix.org".  Updated to the real Matrix display
        // name when the person first sends a command.
        let display_name = mxid.strip_prefix('@')
            .and_then(|s| s.split(':').next())
            .map(str::to_owned)
            .unwrap_or_else(|| mxid.to_owned());
        Person {
            id:           Uuid::new_v4().to_string(),
            display_name,
            active:       true,
            matrix_id:    Some(mxid.to_owned()),
        }
    }
    pub fn new_named(name: &str) -> Self {
        Person {
            id:           Uuid::new_v4().to_string(),
            display_name: name.to_owned(),
            active:       true,
            matrix_id:    None,
        }
    }
    /// True when `s` matches this person's MXID or display name (case-insensitive).
    pub fn matches(&self, s: &str) -> bool {
        self.matrix_id.as_deref() == Some(s)
            || self.display_name.eq_ignore_ascii_case(s)
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
    pub id:         SlotId,
    pub name:       String,
    #[serde(default)]
    pub room_names: Vec<String>,
    /// Per-slot workload multiplier on top of room count (default 1.0).
    #[serde(default = "default_weight")]
    pub weight:     f64,
}

impl CleaningSlot {
    pub fn new(name: &str) -> Self {
        CleaningSlot { id: Uuid::new_v4().to_string(), name: name.to_owned(), room_names: Vec::new(), weight: 1.0 }
    }
}

// ── CleaningGroup ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CleaningGroup {
    pub id:         GroupId,
    pub name:       String,
    /// Member PersonIds **in rotation order** — index determines whose turn it is.
    pub member_ids: Vec<PersonId>,
    /// Used in single-slot mode (slots is empty).
    #[serde(default)]
    pub room_names: Vec<String>,
    /// When non-empty, enables multi-slot mode.
    #[serde(default)]
    pub slots:      Vec<CleaningSlot>,
    /// Group-level workload multiplier on top of room count (default 1.0).
    /// Use `!setgroupweight` to adjust for groups that are heavier/lighter
    /// than their room count suggests (e.g. kitchen = 2.0, storage = 0.5).
    #[serde(default = "default_weight")]
    pub weight:     f64,
}

/// Default workload multiplier — no adjustment.
pub fn default_weight() -> f64 { 1.0 }

impl CleaningGroup {
    pub fn new(name: &str) -> Self {
        CleaningGroup {
            id:         Uuid::new_v4().to_string(),
            name:       name.to_owned(),
            member_ids: Vec::new(),
            room_names: Vec::new(),
            slots:      Vec::new(),
            weight:     1.0,
        }
    }

    pub fn is_multi_slot(&self) -> bool { !self.slots.is_empty() }

    pub fn slot_by_name(&self, name: &str) -> Option<&CleaningSlot> {
        self.slots.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    pub fn slot_by_id(&self, id: &SlotId) -> Option<&CleaningSlot> {
        self.slots.iter().find(|s| &s.id == id)
    }

    pub fn slot_by_id_mut(&mut self, id: &SlotId) -> Option<&mut CleaningSlot> {
        self.slots.iter_mut().find(|s| &s.id == id)
    }

    /// Single-slot rooms text (for non-slotted groups or scheduler header).
    pub fn rooms_text(&self) -> Option<String> {
        if self.room_names.is_empty() {
            None
        } else {
            Some(format!("Rooms: {}", self.room_names.join(", ")))
        }
    }
}

// ── CalendarToken ─────────────────────────────────────────────────────────────

/// A per-person iCal feed token.  Only the SHA-256 hash is persisted;
/// the raw token is shown once and never stored.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CalendarToken {
    pub id:         String,     // UUID for the record itself
    pub token_hash: String,     // hex SHA-256 of the 32-byte raw token
    pub person_id:  PersonId,
    pub created_at: DateTime<Utc>,
    pub revoked:    bool,
}

/// Generate a new random token.
/// Returns `(raw_hex_64_char_token, sha256_hex_hash)`.
pub fn new_calendar_token() -> (String, String) {
    let raw: [u8; 32] = rand::random();
    let token = hex::encode(raw);
    let hash  = hex::encode(Sha256::digest(raw));
    (token, hash)
}

/// Verify a raw hex token string against a stored SHA-256 hash.
pub fn verify_calendar_token(token_hex: &str, stored_hash: &str) -> bool {
    let Ok(raw) = hex::decode(token_hex) else { return false; };
    hex::encode(Sha256::digest(&raw)) == stored_hash
}

// ── Slot assignment resolver types ───────────────────────────────────────────

/// How a `SlotAssignment` was created.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentSource {
    #[default]
    RoundRobin,
    Manual,
}

/// A frozen assignment: one person (or nobody) is responsible for one group/slot
/// in one specific due week.  Once stored, never overwritten except via an
/// explicit `SlotAssigned` event (e.g. when the assigned person leaves).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SlotAssignment {
    pub group_id:   GroupId,
    /// 0 for single-slot groups; slot index into `CleaningGroup.slots` for multi-slot.
    pub slot_index: usize,
    pub iso_year:   i32,
    pub iso_week:   u32,
    /// `None` = unassigned (person left or group has no members).
    pub person_id:  Option<PersonId>,
    #[serde(default)]
    pub source:     AssignmentSource,
}

// ── Deterministic assignment UID ──────────────────────────────────────────────

/// Fixed namespace UUID for this bot — never changes.
const ASSIGNMENT_NS: Uuid = Uuid::from_bytes([
    0xc1, 0xea, 0x3b, 0x07, 0xc1, 0xea, 0x3b, 0x07,
    0xc1, 0xea, 0x3b, 0x07, 0xc1, 0xea, 0x3b, 0x07,
]);

/// Stable UUID v5 for a (group, [slot,] year, week, assignee) tuple.
/// Identical inputs always produce the same UID — no storage required.
pub fn assignment_uid(group_id: &str, year: i32, week: u32, person_id: &str) -> String {
    let name = format!("{group_id}:{year}:W{week:02}:{person_id}");
    Uuid::new_v5(&ASSIGNMENT_NS, name.as_bytes()).to_string()
}

pub fn slot_assignment_uid(group_id: &str, slot_id: &str, year: i32, week: u32, person_id: &str) -> String {
    let name = format!("{group_id}:{slot_id}:{year}:W{week:02}:{person_id}");
    Uuid::new_v5(&ASSIGNMENT_NS, name.as_bytes()).to_string()
}
