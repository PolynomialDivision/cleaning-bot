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
        Person {
            id:           Uuid::new_v4().to_string(),
            display_name: mxid.to_owned(),
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

// ── CleaningGroup ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CleaningGroup {
    pub id:         GroupId,
    pub name:       String,
    /// Member PersonIds **in rotation order** — index determines whose turn it is.
    pub member_ids: Vec<PersonId>,
    #[serde(default)]
    pub room_names: Vec<String>,
}

impl CleaningGroup {
    pub fn new(name: &str) -> Self {
        CleaningGroup {
            id:         Uuid::new_v4().to_string(),
            name:       name.to_owned(),
            member_ids: Vec::new(),
            room_names: Vec::new(),
        }
    }
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

// ── Deterministic assignment UID ──────────────────────────────────────────────

/// Fixed namespace UUID for this bot — never changes.
const ASSIGNMENT_NS: Uuid = Uuid::from_bytes([
    0xc1, 0xea, 0x3b, 0x07, 0xc1, 0xea, 0x3b, 0x07,
    0xc1, 0xea, 0x3b, 0x07, 0xc1, 0xea, 0x3b, 0x07,
]);

/// Stable UUID v5 for a (group, year, week, assignee) tuple.
/// Identical inputs always produce the same UID — no storage required.
pub fn assignment_uid(group_id: &str, year: i32, week: u32, person_id: &str) -> String {
    let name = format!("{group_id}:{year}:W{week:02}:{person_id}");
    Uuid::new_v5(&ASSIGNMENT_NS, name.as_bytes()).to_string()
}
