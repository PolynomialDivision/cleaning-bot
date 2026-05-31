//! Offline state inspector for cleaning-bot.
//!
//! Reads state.json and prints a human-readable report: persons, groups,
//! schedule, completions, pending swaps, inactive stubs, and a validation
//! summary.
//!
//! Usage:
//!   cargo run --bin state_debug                          # ./store/state.json
//!   cargo run --bin state_debug -- /path/to/state.json
//!   INTERVAL=2 cargo run --bin state_debug -- /path/to/state.json

use serde::Deserialize;
use std::collections::{HashMap, HashSet};

// ── Minimal deserialization types ─────────────────────────────────────────────
// These mirror the real structs but only include fields we need for reporting.

#[derive(Deserialize, Debug)]
struct Person {
    id:           String,
    display_name: String,
    #[serde(default)]
    active:       bool,
    #[serde(default)]
    matrix_id:    Option<String>,
}

#[derive(Deserialize, Debug)]
struct CleaningSlot {
    #[allow(dead_code)]
    id:   String,
    name: String,
    #[serde(default)]
    room_names: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct CleaningGroup {
    id:         String,
    name:       String,
    #[serde(default)]
    member_ids: Vec<String>,
    #[serde(default)]
    room_names: Vec<String>,
    #[serde(default)]
    slots:      Vec<CleaningSlot>,
    #[serde(default = "one")]
    weight:     f64,
}
fn one() -> f64 { 1.0 }

#[derive(Deserialize, Debug)]
struct Completion {
    group_id:        String,
    completed_by_id: String,
    iso_year:        i32,
    iso_week:        u32,
    completed_at:    String,
    #[serde(default)]
    skipped:         bool,
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SwapStatus { Pending, Accepted, Rejected, Cancelled }

#[derive(Deserialize, Debug)]
struct SwapRequest {
    id:        u64,
    requester: String,
    target:    String,
    group_id:  String,
    iso_year:  i32,
    iso_week:  u32,
    status:    SwapStatus,
}

#[derive(Deserialize, Debug)]
struct SlotAssignment {
    group_id:   String,
    slot_index: usize,
    iso_year:   i32,
    iso_week:   u32,
    person_id:  Option<String>,
}

#[derive(Deserialize, Debug)]
struct Absence {
    person_id:      String,
    group_id:       String,
    from_year:      i32,
    from_week:      u32,
    duration_weeks: u32,
}

#[derive(Deserialize, Debug)]
struct State {
    #[serde(default)]
    persons:         Vec<Person>,
    #[serde(default)]
    cleaning_groups: Vec<CleaningGroup>,
    #[serde(default)]
    completions:     Vec<Completion>,
    #[serde(default)]
    swap_requests:   Vec<SwapRequest>,
    #[serde(default)]
    slot_assignments: Vec<SlotAssignment>,
    #[serde(default)]
    absences:        Vec<Absence>,
    created_at:      Option<String>,
    last_modified:   Option<String>,
}

impl State {
    fn person(&self, id: &str) -> Option<&Person> {
        self.persons.iter().find(|p| p.id == id)
    }
    fn group(&self, id: &str) -> Option<&CleaningGroup> {
        self.cleaning_groups.iter().find(|g| g.id == id)
    }
}

fn current_iso_week() -> (i32, u32) {
    let d = chrono::Utc::now().date_naive();
    use chrono::Datelike;
    (d.iso_week().year(), d.iso_week().week())
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "store/state.json".to_owned());

    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Cannot read {path}: {e}"));
    let state: State = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("Parse error: {e}"));

    println!("═══════════════════════════════════════════════");
    println!("  cleaning-bot state: {path}");
    println!("═══════════════════════════════════════════════\n");

    // ── Metadata ──────────────────────────────────────────────────────────────
    if let Some(t) = &state.created_at    { println!("Created:       {t}"); }
    if let Some(t) = &state.last_modified { println!("Last modified: {t}"); }
    println!();

    // ── Persons ───────────────────────────────────────────────────────────────
    println!("── Persons ({}) ──────────────────────────────", state.persons.len());
    for p in &state.persons {
        let mxid = p.matrix_id.as_deref().unwrap_or("(no matrix)");
        let active_tag = if !p.active { "  [INACTIVE]" } else { "" };
        let groups: Vec<&str> = state.cleaning_groups.iter()
            .filter(|g| g.member_ids.contains(&p.id))
            .map(|g| g.name.as_str())
            .collect();
        let grp = if groups.is_empty() { "⚠ no group".to_owned() } else { groups.join(", ") };
        println!("  {:<24}  {:<36}  {grp}{active_tag}", p.display_name, mxid);
    }
    println!();

    // ── Groups ────────────────────────────────────────────────────────────────
    println!("── Groups ({}) ───────────────────────────────", state.cleaning_groups.len());
    for g in &state.cleaning_groups {
        let members: Vec<&str> = g.member_ids.iter()
            .filter_map(|id| state.person(id).map(|p| p.display_name.as_str()))
            .collect();
        println!("  {} ({} members): {}", g.name, members.len(), members.join(", "));
        if !g.room_names.is_empty() {
            println!("    rooms: {}", g.room_names.join(", "));
        }
        for slot in &g.slots {
            let rooms = if slot.room_names.is_empty() { "(no rooms)".to_owned() }
                else { slot.room_names.join(", ") };
            println!("    slot «{}»: {}", slot.name, rooms);
        }
        if (g.weight - 1.0).abs() > 0.01 { println!("    weight: {:.2}×", g.weight); }
    }
    println!();

    // ── Completions ───────────────────────────────────────────────────────────
    let real:    Vec<_> = state.completions.iter().filter(|c| !c.skipped).collect();
    let skipped: Vec<_> = state.completions.iter().filter(|c|  c.skipped).collect();
    println!("── Completions: {} done, {} skipped ─────────", real.len(), skipped.len());
    let mut sorted = real.clone();
    sorted.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));
    for c in sorted.iter().take(8) {
        let who   = state.person(&c.completed_by_id).map(|p| p.display_name.as_str()).unwrap_or("?");
        let group = state.group(&c.group_id).map(|g| g.name.as_str()).unwrap_or("?");
        println!("  {:04}-W{:02}  {group:<20}  by {who}", c.iso_year, c.iso_week);
    }
    if sorted.len() > 8 { println!("  … ({} more)", sorted.len() - 8); }
    println!();

    // ── Future schedule ───────────────────────────────────────────────────────
    let (cur_y, cur_w) = current_iso_week();
    let mut future: Vec<_> = state.slot_assignments.iter()
        .filter(|a| a.iso_year > cur_y || (a.iso_year == cur_y && a.iso_week >= cur_w))
        .collect();
    future.sort_by_key(|a| (a.iso_year, a.iso_week, a.slot_index));
    println!("── Future schedule (next 10 assignments) ────");
    for a in future.iter().take(10) {
        let group = state.group(&a.group_id).map(|g| g.name.as_str()).unwrap_or("?");
        let who   = a.person_id.as_deref()
            .and_then(|pid| state.person(pid))
            .map(|p| p.display_name.as_str())
            .unwrap_or("(nobody)");
        let slot_name = state.group(&a.group_id)
            .and_then(|g| g.slots.get(a.slot_index))
            .map(|s| format!(" / {}", s.name))
            .unwrap_or_default();
        let current_tag = if a.iso_year == cur_y && a.iso_week == cur_w { " ← THIS WEEK" } else { "" };
        println!("  {:04}-W{:02}  {group}{slot_name:<28} → {who}{current_tag}",
            a.iso_year, a.iso_week);
    }
    if future.len() > 10 { println!("  … ({} more)", future.len() - 10); }
    println!();

    // ── Pending swaps ─────────────────────────────────────────────────────────
    let pending: Vec<_> = state.swap_requests.iter()
        .filter(|s| s.status == SwapStatus::Pending)
        .collect();
    if !pending.is_empty() {
        println!("── Pending swaps ({}) ─────────────────────", pending.len());
        for s in &pending {
            let group = state.group(&s.group_id).map(|g| g.name.as_str()).unwrap_or("?");
            println!("  #{} {group} {:04}-W{:02}  {} → {}",
                s.id, s.iso_year, s.iso_week, s.requester, s.target);
        }
        println!();
    }

    // ── Absences ──────────────────────────────────────────────────────────────
    if !state.absences.is_empty() {
        println!("── Absences ({}) ─────────────────────────", state.absences.len());
        for a in &state.absences {
            let who   = state.person(&a.person_id).map(|p| p.display_name.as_str()).unwrap_or("?");
            let group = state.group(&a.group_id).map(|g| g.name.as_str()).unwrap_or("?");
            println!("  {who} in {group}: from {:04}-W{:02} for {} weeks",
                a.from_year, a.from_week, a.duration_weeks);
        }
        println!();
    }

    // ── Stubs / orphans ───────────────────────────────────────────────────────
    let grouped: HashSet<&str> = state.cleaning_groups.iter()
        .flat_map(|g| g.member_ids.iter().map(String::as_str))
        .collect();
    let stubs: Vec<_> = state.persons.iter()
        .filter(|p| !p.active || !grouped.contains(p.id.as_str()))
        .collect();
    if !stubs.is_empty() {
        println!("── Stubs / ungrouped persons ({}) ──────────", stubs.len());
        for p in &stubs {
            let reason = if !p.active { "INACTIVE" } else { "no group" };
            println!("  {} ({}) — {reason}",
                p.display_name, p.matrix_id.as_deref().unwrap_or("no mxid"));
        }
        println!();
    }

    // ── Validation ────────────────────────────────────────────────────────────
    println!("── Validation ────────────────────────────────");
    let valid_pids: HashSet<&str> = state.persons.iter().map(|p| p.id.as_str()).collect();
    let valid_gids: HashSet<&str> = state.cleaning_groups.iter().map(|g| g.id.as_str()).collect();
    let mut errors:   Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // Duplicate person IDs
    let mut seen_pids: HashSet<&str> = HashSet::new();
    for p in &state.persons {
        if !seen_pids.insert(&p.id) {
            errors.push(format!("Duplicate PersonId: {} ({})", p.id, p.display_name));
        }
    }
    // Duplicate group IDs
    let mut seen_gids: HashSet<&str> = HashSet::new();
    for g in &state.cleaning_groups {
        if !seen_gids.insert(&g.id) {
            errors.push(format!("Duplicate GroupId: {} ({})", g.id, g.name));
        }
    }
    // Orphaned member references
    for g in &state.cleaning_groups {
        for pid in &g.member_ids { if !valid_pids.contains(pid.as_str()) {
            errors.push(format!("Group «{}» → unknown PersonId {pid}", g.name));
        }}
        if g.member_ids.is_empty() { warnings.push(format!("Group «{}» has no members", g.name)); }
        // Duplicate members within a group
        let mut ms: HashSet<&str> = HashSet::new();
        for pid in &g.member_ids {
            if !ms.insert(pid.as_str()) { warnings.push(format!("Group «{}» has duplicate member {pid}", g.name)); }
        }
    }
    // Orphaned completion references
    for (i, c) in state.completions.iter().enumerate() {
        if !valid_gids.contains(c.group_id.as_str()) {
            warnings.push(format!("Completion #{i} → unknown GroupId {}", c.group_id));
        }
        if !valid_pids.contains(c.completed_by_id.as_str()) {
            warnings.push(format!("Completion #{i} → unknown PersonId {}", c.completed_by_id));
        }
    }
    // Per-group completion counts
    let mut per_group: HashMap<&str, (usize, usize)> = HashMap::new(); // (done, skipped)
    for c in &state.completions {
        let e = per_group.entry(c.group_id.as_str()).or_default();
        if c.skipped { e.1 += 1; } else { e.0 += 1; }
    }
    println!("  Completion counts per group:");
    for g in &state.cleaning_groups {
        let (done, skip) = per_group.get(g.id.as_str()).copied().unwrap_or_default();
        println!("    {}: {} done, {} skipped", g.name, done, skip);
    }

    println!();
    if errors.is_empty() && warnings.is_empty() {
        println!("  ✅ No issues found.");
    } else {
        for e in &errors   { println!("  ❌ {e}"); }
        for w in &warnings { println!("  ⚠️  {w}"); }
    }

    Ok(())
}
