//! Printed forms: persisted identities and optimistic, confirmed domain changes.
use crate::{
    analytics::DomainEvent,
    schedule::{AssignmentInstance, ScheduleSnapshot},
    state::State,
};
use anyhow::{bail, ensure, Result};
use chrono::{Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// Layout v2 (mm from the top left of an A4 page). `scripts/paper.py` reads
// boxes only where each document's manifest says, so sheets printed with
// earlier v2 positions (16 rows from 58.25mm, QR at 187/277) still scan; its
// V2_FIDUCIALS must match and its QR search area must cover QR_CENTER.
/// Corner registration targets.
const FIDUCIALS: [[f64; 2]; 4] = [[10., 10.], [200., 10.], [200., 287.], [10., 287.]];
/// Centre of the page's identity QR code (20mm with its quiet zone, drawn
/// by scripts/paper.py).
const QR_CENTER: [f64; 2] = [185., 276.];
/// Duties per page and the rows' room: a one-line header leaves the table
/// from 25mm (column titles) to 262mm, rows from 31mm. Rows are 11mm high
/// on a full page and taller (up to 14mm) when a group's pages hold fewer,
/// so a page is filled rather than left half empty.
pub const ROWS_PER_PAGE: usize = 21;
const ROWS_TOP: f64 = 31.;
const ROWS_BOTTOM: f64 = 262.;
const ROW_PITCH: f64 = 11.;
const MAX_ROW_PITCH: f64 = 14.;
/// Box side, Monday's box centre and the distance between days: the seven
/// day columns share the table's width right of "Who" (105–196mm). There is
/// no Skip box on paper; skipping is recorded in Matrix. Boxes are 6mm with
/// a 0.4mm outline (sheets printed before: 4.8mm, 0.22mm — each manifest
/// says its size): in a phone photo a chat app shrank to 1200x1600, a
/// thinner line is barely a grey pixel.
const BOX: f64 = 6.;
const MONDAY_X: f64 = 111.5;
const DAY_PITCH: f64 = 13.;
/// Tick sheets: columns from here to the table's right edge (Week and the
/// week's dates to the left), and each box this far left of its column's
/// right edge (clear of the line by more than the scanner reads around it).
const TICK_COLUMNS_LEFT: f64 = 52.;
const TABLE_RIGHT: f64 = 196.;
const TICK_BOX_INSET: f64 = 5.5;

/// The two kinds of printable plan: a box for each day a duty may be done
/// (the day is recorded), or one box per duty (only that it was done).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Style {
    #[default]
    Days,
    Tick,
}

impl Style {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "days" | "day" => Some(Style::Days),
            "tick" | "ticks" | "simple" => Some(Style::Tick),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Style::Days => "days",
            Style::Tick => "tick",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Style::Days => "days: a box for each day, the day is recorded",
            Style::Tick => "tick: one box per duty, slots side by side; done or not, no day",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Document {
    #[serde(default = "legacy_layout")]
    pub layout_version: u8,
    /// Only to look at (`!plan pdf view`): no boxes, no QR code, no corner
    /// targets, and it says so. Never stored or scanned.
    #[serde(default)]
    pub view_only: bool,
    pub id: String,
    pub revision: String,
    pub created: chrono::DateTime<Utc>,
    pub pages: Vec<Page>,
}
fn legacy_layout() -> u8 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page {
    pub number: usize,
    pub title: String,
    pub rooms: String,
    /// The rooms per slot (one group without a slot name for a group without
    /// slots), each as a kind and what is left of its name — for the header.
    #[serde(default)]
    pub room_groups: Vec<RoomGroup>,
    /// Each group has its own style, so each page does.
    #[serde(default)]
    pub style: Style,
    /// Tick sheets: the slots (and shifts) side by side, left to right.
    #[serde(default)]
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    #[serde(default)]
    pub fiducials: Vec<[f64; 2]>,
    #[serde(default)]
    pub qr_center: [f64; 2],
}
/// A column of a tick sheet: its slot ("" without slots), its shift
/// ("Mon–Tue", "" for a whole week) and where it is (mm).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Column {
    pub title: String,
    pub subtitle: String,
    pub left: f64,
    pub right: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RoomGroup {
    pub slot: Option<String>,
    pub rooms: Vec<RoomLabel>,
}
/// "Colbe Toilet 3rd" in slot Colbe: kind `toilet`, label "3rd" — the
/// sheet shows a toilet symbol and "3rd".
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RoomLabel {
    /// `toilet`, `shower`, `kitchen` (each drawn as a symbol) or `other`.
    pub kind: String,
    pub label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Row {
    pub id: String,
    pub group: String,
    pub slot: Option<String>,
    pub slot_index: usize,
    pub year: i32,
    pub week: u32,
    pub shift: u8,
    pub person: Option<String>,
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub task: String,
    /// Tick sheets: the column it stands in.
    #[serde(default)]
    pub column: usize,
    #[serde(default)]
    pub y: f64,
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub status: String,
    pub baseline: String,
    pub fields: Vec<Field>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Field {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub size: f64,
    pub x: f64,
    pub y: f64,
    pub label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Mark {
    pub row: String,
    pub skipped: bool,
    pub day: Option<NaiveDate>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proposal {
    pub document: String,
    pub page: usize,
    pub user: String,
    pub room: String,
    pub source_event: String,
    pub changes: Vec<Mark>,
    pub created: chrono::DateTime<Utc>,
    pub result: Option<String>,
}
#[derive(Deserialize)]
struct Scan {
    document: Option<String>,
    revision: Option<String>,
    page: Option<usize>,
    #[serde(default)]
    marks: Vec<Mark>,
    error: Option<String>,
    /// Corner targets but no readable QR code: a sheet, badly photographed.
    #[serde(default)]
    unreadable: bool,
    /// Duties whose boxes couldn't be read (a fold, a shadow, a dot, several
    /// days marked): named in the preview, to be recorded in Matrix.
    #[serde(default)]
    unclear: Vec<Unclear>,
    /// Boxes filled in: a mark taken back, not counted — said in the
    /// preview, in case it was meant as a (thick) mark.
    #[serde(default)]
    taken_back: Vec<TakenBack>,
}

#[derive(Deserialize, Debug)]
struct TakenBack {
    row: String,
    day: Option<NaiveDate>,
}

#[derive(Deserialize, Debug)]
struct Unclear {
    row: String,
    reason: String,
}

/// The preview's note on duties a photo left unclear and boxes filled in
/// ("" if none).
fn unclear_note(page: &Page, unclear: &[Unclear], taken_back: &[TakenBack]) -> String {
    let duty = |id: &str| page.rows.iter().find(|r| r.id == id);
    let not_read: Vec<String> = unclear
        .iter()
        .filter_map(|u| {
            let r = duty(&u.row)?;
            Some(format!(
                "❓ Week {} · {} · {} — {}",
                r.week, r.name, r.label, u.reason
            ))
        })
        .collect();
    let filled: Vec<String> = taken_back
        .iter()
        .filter_map(|t| {
            let r = duty(&t.row)?;
            let day = t.day.map(|d| format!(" · {}", d.format("%a")));
            Some(format!(
                "✏️ Week {} · {} · {}{}",
                r.week,
                r.name,
                r.label,
                day.unwrap_or_default()
            ))
        })
        .collect();
    let mut note = String::new();
    if !not_read.is_empty() {
        note.push_str(&format!(
            "\n\nNot read:\n{}\nRecord these with !done, or send a clearer photo.",
            not_read.join("\n")
        ));
    }
    if !filled.is_empty() {
        note.push_str(&format!(
            "\n\nFilled in, so taken back (not counted):\n{}\nMeant as done? Record it with !done.",
            filled.join("\n")
        ));
    }
    note
}

/// Tell `room` about a photo of a sheet that can't be used. Once per photo.
async fn photo_problem(room: &Room, event: &OwnedEventId, text: &str) -> Result<()> {
    let txn: mxbot_common::matrix_sdk::ruma::OwnedTransactionId =
        format!("paper-{}", hash(event.as_str())).into();
    room.send(crate::format::intentional(
        mxbot_common::matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(
            text,
        ),
    ))
    .with_transaction_id(txn)
    .await?;
    Ok(())
}
fn hash(value: impl Serialize) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(&value).expect("serializable"),
    ))
}
fn baseline(state: &State, a: &AssignmentInstance) -> String {
    // Include the assignment-edit history: changing away and back is still stale.
    let edits: Vec<_> = state
        .event_log
        .iter()
        .filter(|e| match &e.event {
            DomainEvent::CleaningCompleted { .. } | DomainEvent::CleaningSkipped { .. } => false,
            DomainEvent::SlotAssigned {
                group_id,
                slot_index,
                iso_year,
                iso_week,
                shift,
                source,
                previous_person_id,
                actor_id,
                ..
            } => {
                group_id == &a.group_id
                    && *slot_index == a.slot_index
                    && (*iso_year, *iso_week, *shift) == (a.iso_year, a.iso_week, a.shift)
                    && !(*source == crate::domain::AssignmentSource::RoundRobin
                        && previous_person_id.is_none()
                        && actor_id.is_none())
            }
            DomainEvent::CleaningUndone {
                group_id,
                iso_year,
                iso_week,
                shift,
                slot_id,
            } => {
                group_id == &a.group_id
                    && (*iso_year, *iso_week) == (a.iso_year, a.iso_week)
                    && shift.is_none_or(|s| s == a.shift)
                    && (slot_id.is_none() || slot_id == &a.slot_id)
            }
            _ => serde_json::to_value(&e.event).is_ok_and(|v| v["group_id"] == a.group_id),
        })
        .map(|e| &e.id)
        .collect();
    hash((
        &a.group_id,
        &a.slot_id,
        a.slot_index,
        a.iso_year,
        a.iso_week,
        a.shift,
        a.start,
        a.end,
        &a.source,
        a.assignee.as_ref().map(|p| &p.id),
        edits,
    ))
}
pub fn document(state: &State, snapshot: &ScheduleSnapshot) -> Document {
    let mut pages: Vec<Page> = Vec::new();
    let mut groups: Vec<&str> = Vec::new();
    for a in &snapshot.assignments {
        if !groups.contains(&a.group_id.as_str()) {
            groups.push(&a.group_id);
        }
    }
    for group in groups {
        let assignments: Vec<&AssignmentInstance> = snapshot
            .assignments
            .iter()
            .filter(|a| a.group_id == group)
            .collect();
        match state.paper_style_for(group) {
            Style::Days => days_pages(state, &assignments, &mut pages),
            Style::Tick => tick_pages(state, &assignments, &mut pages),
        }
    }
    Document {
        layout_version: 2,
        view_only: false,
        id: uuid::Uuid::new_v4().simple().to_string(),
        revision: hash((2, &pages))[..12].into(),
        created: Utc::now(),
        pages,
    }
}

/// The same plan only to look at: each group's table as it prints to be
/// ticked, without anything to tick.
pub fn view(state: &State, snapshot: &ScheduleSnapshot) -> Document {
    let mut doc = document(state, snapshot);
    doc.view_only = true;
    for row in doc.pages.iter_mut().flat_map(|p| p.rows.iter_mut()) {
        row.fields.clear();
    }
    doc
}

/// A page of `duties` (all of one group) without rows yet.
fn page_for(number: usize, style: Style, duties: &[&AssignmentInstance]) -> Page {
    Page {
        number,
        title: duties[0].group_name.clone(),
        rooms: duties
            .iter()
            .flat_map(|a| a.room_names.iter().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(" · "),
        room_groups: room_groups(duties),
        style,
        columns: Vec::new(),
        rows: Vec::new(),
        fiducials: FIDUCIALS.to_vec(),
        qr_center: QR_CENTER,
    }
}

/// The row of one duty, at height `y` (in `column` of a tick sheet), with
/// `fields` to fill in unless it is already done, skipped or unassigned.
fn duty_row(
    state: &State,
    a: &AssignmentInstance,
    y: f64,
    column: usize,
    fields: impl FnOnce(&str) -> Vec<Field>,
) -> Row {
    let id = hash((&a.group_id, &a.slot_id, a.iso_year, a.iso_week, a.shift));
    let status = if a.is_skipped {
        "Skipped".into()
    } else if a.is_completed {
        match a.completed_at {
            Some(d) => format!("Done · {}", d.format("%a %-d %b")),
            None => "Done".into(),
        }
    } else {
        String::new()
    };
    let fields = if status.is_empty() && a.assignee.is_some() {
        fields(&id)
    } else {
        Vec::new()
    };
    Row {
        group: a.group_id.clone(),
        slot: a.slot_id.clone(),
        slot_index: a.slot_index,
        year: a.iso_year,
        week: a.iso_week,
        shift: a.shift,
        person: a.assignee.as_ref().map(|p| p.id.clone()),
        name: a.assignee_name().to_string(),
        label: format!(
            "{} {}{}",
            a.slot_name.as_deref().unwrap_or(""),
            a.period_label,
            match a.source {
                crate::domain::AssignmentSource::RoundRobin => "",
                crate::domain::AssignmentSource::Import => " · imported",
                _ => " · assigned",
            }
        ),
        task: a.slot_name.clone().unwrap_or_default(),
        column,
        y,
        start: a.start,
        end: a.end,
        status,
        baseline: baseline(state, a),
        fields,
        id,
    }
}

/// Row height for pages whose fullest one holds `fullest` rows.
fn pitch_for(fullest: usize) -> f64 {
    ((ROWS_BOTTOM - ROWS_TOP) / fullest.max(1) as f64).clamp(ROW_PITCH, MAX_ROW_PITCH)
}

/// One row per duty, a box for each day it may be done.
fn days_pages(state: &State, assignments: &[&AssignmentInstance], pages: &mut Vec<Page>) {
    let chunks = pages_of(assignments, |a| (a.iso_year, a.iso_week));
    // One row height for all of the group's pages: as tall as its fullest
    // page allows.
    let pitch = pitch_for(chunks.iter().map(|c| c.len()).max().unwrap_or(1));
    for chunk in chunks {
        let mut page = page_for(pages.len(), Style::Days, chunk);
        for (i, a) in chunk.iter().enumerate() {
            let y = ROWS_TOP + (i as f64 + 0.5) * pitch;
            page.rows.push(duty_row(state, a, y, 0, |id| {
                let mut fields = Vec::new();
                let mut day = a.start;
                while day <= a.end {
                    fields.push(Field {
                        id: format!("{id}:{day}"),
                        kind: day.to_string(),
                        size: BOX,
                        x: MONDAY_X + day.weekday().num_days_from_monday() as f64 * DAY_PITCH,
                        y,
                        label: day.format("%a").to_string(),
                    });
                    day = day.succ_opt().expect("schedule date");
                }
                fields
            }));
        }
        pages.push(page);
    }
}

/// One row per week; the group's slots (and shifts) side by side, each duty
/// a name and a single box: done or not, without a day.
fn tick_pages(state: &State, assignments: &[&AssignmentInstance], pages: &mut Vec<Page>) {
    // Columns: every shift × slot the group has in these weeks, in order.
    let mut keys: Vec<(u8, usize)> = assignments
        .iter()
        .map(|a| (a.shift, a.slot_index))
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let width = (TABLE_RIGHT - TICK_COLUMNS_LEFT) / keys.len().max(1) as f64;
    let columns: Vec<Column> = keys
        .iter()
        .enumerate()
        .map(|(i, key)| {
            let a = assignments
                .iter()
                .find(|a| (a.shift, a.slot_index) == *key)
                .expect("a duty for each column");
            Column {
                title: a.slot_name.clone().unwrap_or_default(),
                subtitle: a.shift_label.clone().unwrap_or_default(),
                left: TICK_COLUMNS_LEFT + i as f64 * width,
                right: TICK_COLUMNS_LEFT + (i + 1) as f64 * width,
            }
        })
        .collect();
    let mut weeks: Vec<Vec<&AssignmentInstance>> = Vec::new();
    for a in assignments {
        match weeks.last_mut() {
            Some(week) if (week[0].iso_year, week[0].iso_week) == (a.iso_year, a.iso_week) => {
                week.push(a)
            }
            _ => weeks.push(vec![a]),
        }
    }
    let chunks = pages_of(&weeks, |w| (w[0].iso_year, w[0].iso_week));
    let pitch = pitch_for(chunks.iter().map(|c| c.len()).max().unwrap_or(1));
    for chunk in chunks {
        let duties: Vec<&AssignmentInstance> = chunk.iter().flatten().copied().collect();
        let mut page = page_for(pages.len(), Style::Tick, &duties);
        page.columns = columns.clone();
        for (i, week) in chunk.iter().enumerate() {
            let y = ROWS_TOP + (i as f64 + 0.5) * pitch;
            for a in week {
                let column = keys
                    .iter()
                    .position(|k| *k == (a.shift, a.slot_index))
                    .expect("a column for each duty");
                let x = columns[column].right - TICK_BOX_INSET;
                page.rows.push(duty_row(state, a, y, column, |id| {
                    vec![Field {
                        id: format!("{id}:done"),
                        kind: "done".into(),
                        size: BOX,
                        x,
                        y,
                        label: "Done".into(),
                    }]
                }));
            }
        }
        pages.push(page);
    }
}

/// A tick has no day. It counts as done in the middle of its window —
/// Thursday for a whole week, Monday for Mon–Tue — but never later than
/// `today`. (Before the window starts there is nothing to count: that day
/// lies in the future and the import refuses it.)
pub fn tick_day(start: NaiveDate, end: NaiveDate, today: NaiveDate) -> NaiveDate {
    let middle = start + chrono::Duration::days((end - start).num_days() / 2);
    middle.min(today).max(start)
}

/// Give the ticks scanned from a tick sheet their day (see `tick_day`).
pub fn date_ticks(doc: &Document, page: usize, marks: &mut [Mark], today: NaiveDate) {
    let Some(page) = doc.pages.get(page).filter(|p| p.style == Style::Tick) else {
        return;
    };
    for mark in marks.iter_mut().filter(|m| !m.skipped && m.day.is_none()) {
        if let Some(row) = page.rows.iter().find(|r| r.id == mark.row) {
            mark.day = Some(tick_day(row.start, row.end, today));
        }
    }
}

/// The rooms of `chunk`'s duties, per slot in slot order.
fn room_groups(chunk: &[&AssignmentInstance]) -> Vec<RoomGroup> {
    let mut groups: Vec<(usize, RoomGroup)> = Vec::new();
    for a in chunk {
        let i = match groups.iter().position(|(i, _)| *i == a.slot_index) {
            Some(i) => i,
            None => {
                groups.push((
                    a.slot_index,
                    RoomGroup {
                        slot: a.slot_name.clone(),
                        rooms: Vec::new(),
                    },
                ));
                groups.len() - 1
            }
        };
        let group = &mut groups[i].1;
        for name in &a.room_names {
            let room = room(group.slot.as_deref(), name);
            if !group.rooms.contains(&room) {
                group.rooms.push(room);
            }
        }
    }
    groups.sort_by_key(|(i, _)| *i);
    groups.into_iter().map(|(_, g)| g).collect()
}

/// A room as the header shows it: its kind (a symbol on the sheet) and what
/// is left of its name without the slot's name, the word for its kind and a
/// bare "Room": "Colbe Toilet 3rd" in Colbe → toilet "3rd", "Shower Room" →
/// shower "". A room of no known kind keeps its name.
fn room(slot: Option<&str>, name: &str) -> RoomLabel {
    use crate::view::RoomKind;
    let kind = match crate::view::room_kind(name) {
        RoomKind::Toilet => "toilet",
        RoomKind::Shower => "shower",
        RoomKind::Kitchen => "kitchen",
        RoomKind::Other => "other",
    };
    let mut words: Vec<&str> = name.split_whitespace().collect();
    if kind != "other" {
        let same = |a: &str, b: &str| a.to_lowercase() == b.to_lowercase();
        if let (Some(slot), true) = (slot, words.len() > 1) {
            if same(words[0], slot) {
                words.remove(0);
            }
        }
        if let Some(pos) = words
            .iter()
            .position(|w| crate::view::room_kind(w) != RoomKind::Other)
        {
            words.remove(pos);
        }
        words.retain(|w| !["room", "raum", "zimmer"].contains(&w.to_lowercase().as_str()));
    }
    RoomLabel {
        kind: kind.into(),
        label: words.join(" "),
    }
}

/// Keep, for each group, only as many whole weeks as fill one page — what
/// `!plan pdf` prints when no number of weeks is asked for (a group cleaned
/// by two slots gets ten weeks, a weekly one-slot group 21). A tick sheet
/// has one row per week, so a group printed that way always gets 21.
pub fn one_page_per_group(snapshot: &mut ScheduleSnapshot, state: &State) {
    use std::collections::{HashMap, HashSet};
    let all = std::mem::take(&mut snapshot.assignments);
    let mut rows_in_week: HashMap<(&str, (i32, u32)), usize> = HashMap::new();
    for a in &all {
        let rows = rows_in_week
            .entry((&a.group_id, (a.iso_year, a.iso_week)))
            .or_default();
        *rows = match state.paper_style_for(&a.group_id) {
            Style::Days => *rows + 1,
            Style::Tick => 1,
        };
    }
    // Weeks each group keeps, in order, while they fit (a first week always).
    let mut keep: HashSet<(&str, (i32, u32))> = HashSet::new();
    let mut used: HashMap<&str, usize> = HashMap::new();
    let mut closed: HashSet<&str> = HashSet::new();
    for a in &all {
        let key = (a.group_id.as_str(), (a.iso_year, a.iso_week));
        if keep.contains(&key) || closed.contains(key.0) {
            continue;
        }
        let rows = used.entry(key.0).or_default();
        if *rows > 0 && *rows + rows_in_week[&key] > ROWS_PER_PAGE {
            closed.insert(key.0);
            continue;
        }
        *rows += rows_in_week[&key];
        keep.insert(key);
    }
    let kept: Vec<bool> = all
        .iter()
        .map(|a| keep.contains(&(a.group_id.as_str(), (a.iso_year, a.iso_week))))
        .collect();
    snapshot.assignments = all
        .into_iter()
        .zip(kept)
        .filter_map(|(a, k)| k.then_some(a))
        .collect();
}

/// Split `rows` into pages of at most ROWS_PER_PAGE, evenly (18 rows make
/// 10 + 8, not 16 + 2), breaking only between weeks unless a single week is
/// longer than a page.
fn pages_of<T, W: PartialEq>(rows: &[T], week: impl Fn(&T) -> W) -> Vec<&[T]> {
    let mut pages = rows.len().div_ceil(ROWS_PER_PAGE).max(1);
    let mut out = Vec::new();
    let mut rest = rows;
    while !rest.is_empty() {
        // Share what is left evenly over the pages still to come.
        let target = rest.len().div_ceil(pages.max(1));
        pages = pages.saturating_sub(1);
        let max = rest.len().min(ROWS_PER_PAGE);
        // The first week boundary at or after `target` that still fits,
        // else the last one before it.
        let boundary = |i: usize| i == rest.len() || week(&rest[i - 1]) != week(&rest[i]);
        let cut = (target.min(max)..=max)
            .find(|&i| boundary(i))
            .or_else(|| (1..target.min(max)).rev().find(|&i| boundary(i)))
            .unwrap_or(max);
        let (page, tail) = rest.split_at(cut);
        out.push(page);
        rest = tail;
    }
    out
}

fn current(state: &State, row: &Row) -> Option<AssignmentInstance> {
    crate::schedule::build_schedule_from(state, (row.year, row.week), 1)
        .assignments
        .into_iter()
        .find(|a| a.group_id == row.group && a.slot_id == row.slot && a.shift == row.shift)
}
/// A row of a tick sheet: one box, no day.
fn ticked(row: &Row) -> bool {
    row.fields.iter().any(|f| f.kind == "done")
}
fn validate(state: &State, row: &Row, mark: &Mark, user: &str, admin: bool) -> Result<bool> {
    let a = current(state, row).ok_or_else(|| anyhow::anyhow!("Duty no longer exists"))?;
    let actor = state
        .person_by_matrix_id(user)
        .ok_or_else(|| anyhow::anyhow!("No linked participant"))?;
    ensure!(actor.active, "Participant is inactive");
    ensure!(
        admin || row.person.as_ref() == Some(&actor.id),
        "Only your own duties may be changed"
    );
    ensure!(
        !mark.skipped || admin,
        "Skipping a duty requires an administrator"
    );
    ensure!(
        baseline(state, &a) == row.baseline,
        "Printed assignment changed; request a fresh PDF"
    );
    // A tick only says "done": recorded on any day, that is no change.
    if a.is_completed && !mark.skipped && ticked(row) {
        return Ok(false);
    }
    if a.is_completed || a.is_skipped {
        ensure!(
            a.is_skipped == mark.skipped && (mark.skipped || a.completed_at == mark.day),
            "Duty already has a different recorded result"
        );
        return Ok(false);
    }
    ensure!(row.status.is_empty(), "Printed duty was already closed");
    ensure!(row.person.is_some(), "Duty has no assignee");
    if mark.skipped {
        ensure!(mark.day.is_none(), "Skipped duty also has a day marked");
    } else {
        let day = mark
            .day
            .ok_or_else(|| anyhow::anyhow!("Select one completion day"))?;
        ensure!(
            day >= row.start && day <= row.end && day <= crate::state::today(),
            "Completion day is outside the window or in the future"
        );
    }
    Ok(true)
}
fn rows<'a>(doc: &'a Document, p: &Proposal) -> Result<&'a Page> {
    doc.pages
        .get(p.page)
        .ok_or_else(|| anyhow::anyhow!("Unknown page"))
}
fn changes(state: &State, p: &Proposal, admin: bool) -> Result<Vec<DomainEvent>> {
    let doc = state
        .paper_documents
        .get(&p.document)
        .ok_or_else(|| anyhow::anyhow!("Document expired; request a fresh PDF"))?;
    let page = rows(doc, p)?;
    let mut events = Vec::new();
    for m in &p.changes {
        let r = page
            .rows
            .iter()
            .find(|r| r.id == m.row)
            .ok_or_else(|| anyhow::anyhow!("Unknown field"))?;
        if !validate(state, r, m, &p.user, admin)? {
            continue;
        }
        let person = state.person_by_matrix_id(&p.user).unwrap();
        events.push(if m.skipped {
            DomainEvent::CleaningSkipped {
                group_id: r.group.clone(),
                skipper_id: person.id.clone(),
                iso_year: r.year,
                iso_week: r.week,
                slot_id: r.slot.clone(),
                shift: Some(r.shift),
            }
        } else {
            DomainEvent::CleaningCompleted {
                group_id: r.group.clone(),
                slot_id: r.slot.clone(),
                person_id: r.person.clone().unwrap(),
                responsible_person_ids: r.person.iter().cloned().collect(),
                iso_year: r.year,
                iso_week: r.week,
                shift: r.shift,
                completed_on: m.day,
            }
        });
    }
    Ok(events)
}
/// Validate the whole proposal before applying anything; caller saves the staged state atomically.
pub fn apply(state: &mut State, p: &Proposal, admin: bool) -> Result<usize> {
    ensure!(
        Utc::now() - p.created < chrono::Duration::hours(24),
        "Confirmation expired; send a new photo"
    );
    let events = changes(state, p, admin)?;
    let n = events.len();
    let mut staged = state.clone();
    for event in events {
        staged.apply_event(event)?;
    }
    *state = staged;
    Ok(n)
}
pub fn prune(state: &mut State) {
    let now = Utc::now();
    state
        .paper_documents
        .retain(|_, d| now - d.created < chrono::Duration::days(400));
    state
        .paper_scans
        .retain(|_, p| now - p.created < chrono::Duration::days(7));
}

async fn worker(mode: &str, input: &[u8], manifest: Option<&Document>) -> Result<Vec<u8>> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join("paper.py");
    tokio::fs::write(&script, include_str!("../scripts/paper.py")).await?;
    tokio::fs::write(
        dir.path().join("paper_v1.py"),
        include_str!("../scripts/paper_v1.py"),
    )
    .await?;
    tokio::fs::write(dir.path().join("input"), input).await?;
    if let Some(d) = manifest {
        tokio::fs::write(dir.path().join("manifest.json"), serde_json::to_vec(d)?).await?;
    }
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new("python3")
            .arg(script)
            .arg(mode)
            .current_dir(dir.path())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    ensure!(
        out.status.success(),
        "Paper worker failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}
pub async fn pdf(doc: &Document) -> Result<Vec<u8>> {
    worker("pdf", &serde_json::to_vec(doc)?, None).await
}

use crate::BotContext;
use mxbot_common::matrix_sdk::{
    ruma::{
        events::room::message::{ImageMessageEventContent, ReplacementMetadata},
        OwnedEventId, OwnedUserId,
    },
    Room,
};
async fn authorized(ctx: &BotContext, room: &Room, user: &OwnedUserId) -> bool {
    if room.room_id() != ctx.room_id {
        return crate::private::authorized(ctx, room, user).await;
    }
    use mxbot_common::matrix_sdk::ruma::api::client::state::get_state_events;
    room.client()
        .send(get_state_events::v3::Request::new(ctx.room_id.clone()))
        .await
        .is_ok_and(|s| {
            s.room_state.iter().any(|e| {
                e.deserialize_as::<serde_json::Value>().is_ok_and(|v| {
                    v["type"] == "m.room.member"
                        && v["state_key"] == user.as_str()
                        && v["content"]["membership"] == "join"
                })
            })
        })
}
const UNREADABLE: &str = "📷 That looks like a cleaning plan, but I can't read its code \
(bottom right). Please take the photo again: the whole page, flat, without shadow, \
from closer — and send it in full quality, not compressed.";
const UNKNOWN_SHEET: &str = "📷 I don't know this cleaning plan: it was printed by another \
bot or before its records were reset. Print a fresh one with !plan pdf.";

pub async fn image(
    ctx: &BotContext,
    room: &Room,
    user: &OwnedUserId,
    event: &OwnedEventId,
    image: &ImageMessageEventContent,
) -> Result<()> {
    if !authorized(ctx, room, user).await {
        return Ok(());
    }
    // The operation lock also bounds image processing to one worker per bot.
    let _op = ctx.operations.lock().await;
    if ctx
        .state
        .lock()
        .await
        .paper_scans
        .values()
        .any(|p| p.source_event == event.as_str())
    {
        return Ok(());
    }
    if image
        .info
        .as_ref()
        .and_then(|i| i.size)
        .is_some_and(|n| u64::from(n) > 12_000_000)
    {
        return Ok(());
    }
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        room.client().media().get_file(image, false),
    )
    .await??;
    let Some(bytes) = bytes else {
        return Ok(());
    };
    if bytes.len() > 12_000_000 {
        return Ok(());
    }
    let identified: Scan = serde_json::from_slice(&worker("identify", &bytes, None).await?)?;
    let Some(id) = identified.document else {
        // Any other picture: ignored. A sheet whose code can't be read: said.
        if identified.unreadable {
            photo_problem(room, event, UNREADABLE).await?;
        }
        return Ok(());
    };
    let doc = ctx.state.lock().await.paper_documents.get(&id).cloned();
    let Some(doc) = doc else {
        photo_problem(room, event, UNKNOWN_SHEET).await?;
        return Ok(());
    };
    let scanned: Scan = serde_json::from_slice(&worker("scan", &bytes, Some(&doc)).await?)?;
    let mut proposal = Proposal {
        document: id,
        page: scanned.page.unwrap_or(usize::MAX),
        user: user.to_string(),
        room: room.room_id().to_string(),
        source_event: event.to_string(),
        changes: scanned.marks,
        created: Utc::now(),
        result: None,
    };
    date_ticks(
        &doc,
        proposal.page,
        &mut proposal.changes,
        crate::state::today(),
    );
    {
        let state = ctx.state.lock().await;
        if state.paper_scans.values().any(|p| {
            p.document == proposal.document
                && p.page == proposal.page
                && p.user == proposal.user
                && p.room == proposal.room
                && p.changes == proposal.changes
                && Utc::now() - p.created < chrono::Duration::hours(24)
                && (p.result.is_none() || p.result.as_ref().is_some_and(|r| r.starts_with("✅")))
        }) {
            return Ok(());
        }
    }
    let admin = ctx.admin_users.contains(user);
    let text = {
        let state = ctx.state.lock().await;
        let outcome = (|| -> Result<String> {
            if let Some(e) = scanned.error {
                bail!("{e}");
            }
            ensure!(
                scanned.revision.as_deref() == Some(&doc.revision),
                "Unknown sheet revision"
            );
            let events = changes(&state, &proposal, admin)?;
            let page = rows(&doc, &proposal)?;
            let not_read = unclear_note(page, &scanned.unclear, &scanned.taken_back);
            if events.is_empty() {
                proposal.result = Some("No new changes".into());
                return Ok(format!(
                    "📷 Cleaning sheet recognized · no new changes.{not_read}"
                ));
            }
            let lines: Vec<_> = proposal
                .changes
                .iter()
                .map(|m| {
                    let r = page.rows.iter().find(|r| r.id == m.row).unwrap();
                    format!(
                        "{} Week {} · {} · {}\n{}",
                        if m.skipped { "⏭" } else { "✅" },
                        r.week,
                        r.name,
                        r.label,
                        match m.day {
                            // No day on a tick sheet: the middle of the window.
                            Some(d) if ticked(r) => {
                                format!("Done (counted as {})", d.format("%a %-d %b"))
                            }
                            Some(d) => format!("Done {d}"),
                            None => "Skipped".into(),
                        }
                    )
                })
                .collect();
            Ok(format!("📷 Cleaning sheet recognized\n\n{}{not_read}\n\n{} changes · ✅ Apply · ❌ Cancel\nOnly you can confirm. Expires in 24 hours.",lines.join("\n\n"),events.len()))
        })();
        match outcome {
            Ok(t) => t,
            Err(e) => {
                proposal.result = Some(e.to_string());
                format!("📷 Cleaning sheet recognized\n⚠️ {e}\nNothing changed. Check the sheet and send a clear photo of one full page.")
            }
        }
    };
    // Stable transaction prevents duplicate previews after a crash between send/save.
    let txn: mxbot_common::matrix_sdk::ruma::OwnedTransactionId =
        format!("paper-{}", hash(event.as_str())).into();
    let sent=room.send(crate::format::intentional(mxbot_common::matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(text))).with_transaction_id(txn).await?;
    let pending = proposal.result.is_none();
    let mut state = ctx.state.lock().await;
    state
        .paper_scans
        .insert(sent.response.event_id.to_string(), proposal);
    state.save(&ctx.state_path).await?;
    drop(state);
    if pending {
        use mxbot_common::matrix_sdk::ruma::events::{
            reaction::ReactionEventContent, relation::Annotation,
        };
        for key in ["✅", "❌"] {
            room.send(ReactionEventContent::new(Annotation::new(
                sent.response.event_id.clone(),
                key.into(),
            )))
            .await?;
        }
    }
    Ok(())
}
pub async fn reaction(
    ctx: &BotContext,
    room: &Room,
    user: &OwnedUserId,
    target: &str,
    key: &str,
) -> Result<bool> {
    let p = ctx.state.lock().await.paper_scans.get(target).cloned();
    let Some(mut p) = p else {
        return Ok(false);
    };
    if p.user != user.as_str()
        || p.room != room.room_id().as_str()
        || !authorized(ctx, room, user).await
    {
        return Ok(true);
    }
    if p.result.is_none() {
        if key == "❌" {
            p.result = Some("Cancelled · nothing changed".into());
        } else if key == "✅" {
            let mut state = ctx.state.lock().await;
            let mut staged = state.clone();
            p.result = Some(
                match apply(&mut staged, &p, ctx.admin_users.contains(user)) {
                    Ok(n) => format!("✅ {n} changes applied"),
                    Err(e) => format!("⚠️ {e} · nothing changed"),
                },
            );
            staged.paper_scans.insert(target.into(), p.clone());
            staged.save(&ctx.state_path).await?;
            *state = staged;
        } else {
            return Ok(true);
        }
        let mut state = ctx.state.lock().await;
        state.paper_scans.insert(target.into(), p.clone());
        state.save(&ctx.state_path).await?;
    }
    let id = OwnedEventId::try_from(target)?;
    let content = crate::format::intentional(
        mxbot_common::matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(
            format!("📷 Cleaning sheet\n{}", p.result.unwrap_or_default()),
        ),
    )
    .make_replacement(ReplacementMetadata::new(id, None));
    room.send(content).await?;
    if let Some(main) = room.client().get_room(&ctx.room_id) {
        let (y, w) = crate::state::current_iso_week();
        crate::scheduler::refresh_pinned_plan(ctx, &main, y, w).await;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AssignmentSource, CleaningGroup, CleaningSlot, Person};
    fn fixture() -> (State, Document, Proposal) {
        let mut state = State::default();
        let p = Person::new_matrix("@alice:test");
        let mut g = CleaningGroup::new("Kitchen");
        g.member_ids.push(p.id.clone());
        g.rhythm.every_weeks = Some(1);
        state.persons.push(p);
        state.cleaning_groups.push(g);
        let (y, w) = crate::state::current_iso_week();
        let week = crate::state::add_weeks(y, w, -1);
        state.slot_assignments.push(crate::domain::SlotAssignment {
            group_id: state.cleaning_groups[0].id.clone(),
            slot_index: 0,
            iso_year: week.0,
            iso_week: week.1,
            shift: 0,
            person_id: Some(state.persons[0].id.clone()),
            source: Default::default(),
        });
        let snapshot = crate::schedule::build_schedule_from(&state, week, 1);
        let doc = document(&state, &snapshot);
        let row = &doc.pages[0].rows[0];
        let proposal = Proposal {
            document: doc.id.clone(),
            page: 0,
            user: "@alice:test".into(),
            room: "!room:test".into(),
            source_event: "$image".into(),
            changes: vec![Mark {
                row: row.id.clone(),
                skipped: false,
                day: Some(row.start),
            }],
            created: Utc::now(),
            result: None,
        };
        state.paper_documents.insert(doc.id.clone(), doc.clone());
        (state, doc, proposal)
    }
    #[test]
    fn manifest_stable_except_opaque_identity_and_timestamp() {
        let (state, doc, _) = fixture();
        let r = &doc.pages[0].rows[0];
        let snapshot = crate::schedule::build_schedule_from(&state, (r.year, r.week), 1);
        let again = document(&state, &snapshot);
        assert_eq!(doc.revision, again.revision);
        assert_ne!(doc.id, again.id);
        assert_eq!(
            serde_json::to_value(&doc.pages).unwrap(),
            serde_json::to_value(&again.pages).unwrap()
        );
        let loaded: State = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(loaded.paper_documents[&doc.id].revision, doc.revision);
    }
    #[test]
    fn preview_does_not_write_confirmation_uses_events_and_is_idempotent() {
        let (mut s, _, p) = fixture();
        let before = s.event_log.len();
        assert_eq!(changes(&s, &p, false).unwrap().len(), 1);
        assert!(s.completions.is_empty());
        assert_eq!(apply(&mut s, &p, false).unwrap(), 1);
        assert_eq!(s.event_log.len(), before + 1);
        assert!(matches!(
            s.event_log.last().unwrap().event,
            DomainEvent::CleaningCompleted {
                completed_on: Some(_),
                ..
            }
        ));
        assert_eq!(s.completions[0].completed_on, p.changes[0].day);
        let mut s: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(apply(&mut s, &p, false).unwrap(), 0);
        assert_eq!(s.completions.len(), 1);
    }
    #[test]
    fn permission_skip_and_date_checks_are_all_or_nothing() {
        let (mut s, _, mut p) = fixture();
        p.user = "@stranger:test".into();
        assert!(apply(&mut s, &p, false).is_err());
        p.user = "@alice:test".into();
        p.changes[0].skipped = true;
        p.changes[0].day = None;
        assert!(apply(&mut s, &p, false).is_err());
        assert!(s.completions.is_empty());
        assert_eq!(apply(&mut s, &p, true).unwrap(), 1);
        assert!(s.completions[0].skipped);
        assert!(matches!(
            s.event_log.last().unwrap().event,
            DomainEvent::CleaningSkipped { .. }
        ));
    }
    #[test]
    fn changed_manual_assignment_and_undo_invalidate_paper() {
        let (mut s, doc, p) = fixture();
        let r = &doc.pages[0].rows[0];
        s.apply_event(DomainEvent::SlotAssigned {
            group_id: r.group.clone(),
            slot_index: r.slot_index,
            iso_year: r.year,
            iso_week: r.week,
            shift: r.shift,
            person_id: r.person.clone(),
            source: AssignmentSource::Import,
            actor_id: None,
            previous_person_id: None,
        })
        .unwrap();
        assert!(apply(&mut s, &p, true).is_err());
        assert!(s.completions.is_empty());
        let (mut s, doc, p) = fixture();
        assert_eq!(apply(&mut s, &p, false).unwrap(), 1);
        let r = &doc.pages[0].rows[0];
        s.apply_event(DomainEvent::CleaningUndone {
            group_id: r.group.clone(),
            iso_year: r.year,
            iso_week: r.week,
            shift: Some(r.shift),
            slot_id: r.slot.clone(),
        })
        .unwrap();
        assert!(apply(&mut s, &p, false).is_err());
        assert!(s.completions.is_empty());
    }
    #[test]
    fn conflicting_existing_result_and_expired_confirmation_cannot_overwrite() {
        let (mut s, _, mut p) = fixture();
        apply(&mut s, &p, false).unwrap();
        p.changes[0].day = p.changes[0].day.map(|d| d.succ_opt().unwrap());
        assert!(apply(&mut s, &p, false).is_err());
        let (mut s, _, mut p) = fixture();
        p.created -= chrono::Duration::days(2);
        assert!(apply(&mut s, &p, false).is_err());
        assert!(s.completions.is_empty());
    }
    #[test]
    fn two_windows_and_multi_slot_have_separate_rows_and_only_permitted_days() {
        let (mut s, _, _) = fixture();
        for name in ["Bob", "Carol", "Dan"] {
            let person = Person::new_named(name);
            s.cleaning_groups[0].member_ids.push(person.id.clone());
            s.persons.push(person);
        }
        let g = &mut s.cleaning_groups[0];
        g.name = "Upper Floor".into();
        g.rhythm.shift_starts = vec![0, 3];
        g.rhythm.shift_ends = vec![1, 4];
        g.slots = vec![CleaningSlot::new("Stairs"), CleaningSlot::new("Hall")];
        let snapshot = crate::schedule::build_schedule(&s, 6);
        let doc = document(&s, &snapshot);
        assert_eq!(doc.pages.len(), 2);
        let rows: Vec<_> = doc.pages.iter().flat_map(|p| &p.rows).collect();
        assert_eq!(rows.len(), 24);
        let ids: std::collections::HashSet<_> = rows.iter().map(|r| &r.id).collect();
        assert_eq!(ids.len(), 24);
        for r in rows {
            // A two-day window: a box for each of its two days.
            assert_eq!((r.end - r.start).num_days(), 1);
            assert_eq!(r.fields.len(), 2);
            assert!(r.slot.is_some());
            assert!(r.fields.iter().all(|f| f.x < 196. && f.y < 260.));
        }
        // 12 rows a page: taller rows (at most 14mm) fill more of it.
        let ys: Vec<f64> = doc.pages[0].rows.iter().map(|r| r.y).collect();
        assert!(ys
            .windows(2)
            .all(|w| (w[1] - w[0] - MAX_ROW_PITCH).abs() < 1e-9));
    }
    #[test]
    fn routine_freeze_is_valid_but_reassigning_away_and_back_is_stale() {
        let (mut s, doc, p) = fixture();
        let r = &doc.pages[0].rows[0];
        let event = |person, source, previous| DomainEvent::SlotAssigned {
            group_id: r.group.clone(),
            slot_index: r.slot_index,
            iso_year: r.year,
            iso_week: r.week,
            shift: r.shift,
            person_id: person,
            source,
            actor_id: None,
            previous_person_id: previous,
        };
        // A first automatic freeze of the already printed projection is harmless.
        s.event_log.push(crate::analytics::LoggedEvent::now(event(
            r.person.clone(),
            AssignmentSource::RoundRobin,
            None,
        )));
        assert_eq!(changes(&s, &p, false).unwrap().len(), 1);
        s.apply_event(event(None, AssignmentSource::Assign, r.person.clone()))
            .unwrap();
        s.apply_event(event(r.person.clone(), AssignmentSource::RoundRobin, None))
            .unwrap();
        assert!(apply(&mut s, &p, false).is_err());
        assert!(s.completions.is_empty());
    }
    #[test]
    fn a_late_bad_row_cannot_partially_apply_a_page() {
        let (mut s, _, mut p) = fixture();
        p.changes.push(Mark {
            row: "unknown".into(),
            skipped: false,
            day: p.changes[0].day,
        });
        let before = s.event_log.len();
        assert!(apply(&mut s, &p, true).is_err());
        assert!(s.completions.is_empty());
        assert_eq!(s.event_log.len(), before);
    }

    #[test]
    fn layout_v2_geometry_is_what_the_scanner_expects() {
        let (_, doc, _) = fixture();
        assert_eq!(doc.layout_version, 2);
        let page = &doc.pages[0];
        assert_eq!(page.fiducials, FIDUCIALS.to_vec());
        assert_eq!(page.qr_center, QR_CENTER);
        let row = &page.rows[0];
        // One week on the page: rows as tall as allowed.
        assert_eq!(row.y, ROWS_TOP + MAX_ROW_PITCH / 2.);
        // A whole week: a box for each day, Monday to Sunday — no
        // separate "done" box and no Skip box.
        let kinds: Vec<&str> = row.fields.iter().map(|f| f.kind.as_str()).collect();
        assert_eq!(kinds.len(), 7);
        assert!(!kinds.contains(&"done") && !kinds.contains(&"skip"));
        for f in &row.fields {
            assert_eq!(f.size, BOX);
            assert_eq!(f.y, row.y);
            let day: NaiveDate = f.kind.parse().unwrap();
            let weekday = day.weekday().num_days_from_monday() as f64;
            assert_eq!(f.x, MONDAY_X + weekday * DAY_PITCH);
            assert_eq!(f.id, format!("{}:{day}", row.id));
        }
        // Boxes never touch: neighbours are a pitch apart, and the scanner
        // reads 1.4mm around each box (RING in scripts/paper.py).
        const { assert!(DAY_PITCH - BOX > 2. * 1.4) };
        // Sunday's box and its surroundings stay inside the table (right
        // edge at 196mm, see scripts/paper.py), Monday's clear of the "Who"
        // column's line (105mm), and a tick sheet's box of its column's.
        const { assert!(196. - (MONDAY_X + 6. * DAY_PITCH) - BOX / 2. > 1.4) };
        const { assert!(MONDAY_X - BOX / 2. - 105. > 1.4) };
        const { assert!(TICK_BOX_INSET - BOX / 2. > 1.4) };
        const { assert!(ROW_PITCH - BOX > 2. * 1.4) };
        // A full page fits between the column titles and the footer, and
        // the table stays clear of the QR code.
        const { assert!(ROWS_TOP + ROWS_PER_PAGE as f64 * ROW_PITCH <= ROWS_BOTTOM) };
        const { assert!(ROWS_BOTTOM < QR_CENTER[1] - 20. / 2.) };
        // And the QR code of the bottom-right corner target.
        const { assert!(QR_CENTER[0] + 20. / 2. < FIDUCIALS[2][0] - 2.5) };
    }

    #[test]
    fn pages_break_evenly_and_between_weeks() {
        let sizes = |weeks: &[usize]| {
            let rows: Vec<usize> = weeks
                .iter()
                .enumerate()
                .flat_map(|(w, &n)| std::iter::repeat_n(w, n))
                .collect();
            let pages = pages_of(&rows, |w| *w);
            for page in &pages {
                assert!(page.len() <= ROWS_PER_PAGE);
            }
            // No week is split (unless it alone is longer than a page).
            for pair in pages.windows(2) {
                assert_ne!(pair[0].last(), pair[1].first());
            }
            pages.iter().map(|p| p.len()).collect::<Vec<_>>()
        };
        // 11 weeks of 2 rows (22): not 21 + 1, and no week split.
        assert_eq!(sizes(&[2; 11]), [12, 10]);
        assert_eq!(sizes(&[2; 10]), [20]);
        assert_eq!(sizes(&[1; 21]), [21]);
        assert_eq!(sizes(&[1; 22]), [11, 11]);
        // 4 rows a week (2 slots × 2 shifts), 6 weeks.
        assert_eq!(sizes(&[4; 6]), [12, 12]);
        assert_eq!(sizes(&[2; 25]), [18, 16, 16]);
        assert_eq!(sizes(&[]), Vec::<usize>::new());
    }

    /// "Upper Floor": cleaned twice a week by two slots, four people.
    fn upper_floor() -> State {
        let (mut s, _, _) = fixture();
        for name in ["Bob", "Carol", "Dan"] {
            let person = Person::new_named(name);
            s.cleaning_groups[0].member_ids.push(person.id.clone());
            s.persons.push(person);
        }
        // Two slots, twice a week: four rows a week.
        let g = &mut s.cleaning_groups[0];
        g.name = "Upper Floor".into();
        g.rhythm.shift_starts = vec![0, 3];
        g.rhythm.shift_ends = vec![1, 4];
        let mut stairs = CleaningSlot::new("Stairs");
        stairs.room_names = vec!["Stairs Toilet 3rd".into(), "Stairs Toilet 4th".into()];
        let mut hall = CleaningSlot::new("Hall");
        hall.room_names = vec!["Hall Toilet".into(), "Shower Room".into()];
        g.slots = vec![stairs, hall];
        s
    }

    #[test]
    fn by_default_each_group_fills_one_page() {
        let s = upper_floor();
        let mut snapshot = crate::schedule::build_schedule(&s, ROWS_PER_PAGE);
        one_page_per_group(&mut snapshot, &s);
        let weeks: std::collections::BTreeSet<_> = snapshot
            .assignments
            .iter()
            .map(|a| (a.iso_year, a.iso_week))
            .collect();
        assert_eq!(snapshot.assignments.len(), 20);
        assert_eq!(weeks.len(), 5);
        let doc = document(&s, &snapshot);
        // What `!plan pdf` prints by default — the Python tests scan it.
        if let Ok(path) = std::env::var("PAPER_LAYOUT_FIXTURE") {
            std::fs::write(path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
        }
        assert_eq!(doc.pages.len(), 1);
        // The page is filled: 20 rows of 11.55mm reach the footer.
        let rows = &doc.pages[0].rows;
        assert!(rows.last().unwrap().y + (rows[1].y - rows[0].y) / 2. > ROWS_BOTTOM - 0.1);
    }

    #[test]
    fn a_tick_sheet_puts_the_slots_side_by_side() {
        let mut s = upper_floor();
        let group = s.cleaning_groups[0].id.clone();
        s.paper_styles.insert(group.clone(), Style::Tick);
        let mut snapshot = crate::schedule::build_schedule(&s, ROWS_PER_PAGE);
        one_page_per_group(&mut snapshot, &s);
        // One line a week: 21 weeks of four duties on one page.
        assert_eq!(snapshot.assignments.len(), 4 * ROWS_PER_PAGE);
        let doc = document(&s, &snapshot);
        // What `!plan pdf` prints in this style — the Python tests scan it.
        if let Ok(path) = std::env::var("PAPER_TICK_FIXTURE") {
            std::fs::write(path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
        }
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].style, Style::Tick);
        let page = &doc.pages[0];
        let titles: Vec<_> = page.columns.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["Stairs", "Hall", "Stairs", "Hall"]);
        assert_eq!(page.columns[0].subtitle, page.columns[1].subtitle);
        assert_ne!(page.columns[0].subtitle, page.columns[2].subtitle);
        assert!(page.columns.iter().all(|c| !c.subtitle.is_empty()));
        let lines: std::collections::BTreeSet<_> =
            page.rows.iter().map(|r| (r.y * 100.) as i64).collect();
        assert_eq!(lines.len(), ROWS_PER_PAGE);
        for row in &page.rows {
            let column = &page.columns[row.column];
            assert_eq!(column.title, row.task);
            assert!(row.fields.len() <= 1);
            for f in &row.fields {
                assert_eq!(f.kind, "done");
                assert_eq!(f.y, row.y);
                // What the scanner reads around the box (3.6mm) stays clear
                // of the column's line and leaves room for the name.
                assert!(column.right - f.x > 3.6, "{column:?} {f:?}");
                assert!(f.x - column.left > 3.6 + 20., "{column:?} {f:?}");
            }
        }
        // Neither the style nor the columns are the same sheet as days.
        s.paper_styles.remove(&group);
        assert_ne!(document(&s, &snapshot).revision, doc.revision);
    }

    #[test]
    fn groups_print_in_their_own_style_in_one_document() {
        let mut s = upper_floor();
        let mut bath = CleaningGroup::new("Bathroom");
        bath.member_ids = s.cleaning_groups[0].member_ids.clone();
        let bath_id = bath.id.clone();
        s.cleaning_groups.push(bath);
        s.paper_styles.insert(bath_id.clone(), Style::Tick);
        let mut snapshot = crate::schedule::build_schedule(&s, ROWS_PER_PAGE);
        one_page_per_group(&mut snapshot, &s);
        let weeks = |group: &str| {
            snapshot
                .assignments
                .iter()
                .filter(|a| a.group_id == group)
                .map(|a| (a.iso_year, a.iso_week))
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        };
        // Upper Floor by days: five weeks of four rows; Bathroom ticked: 21.
        assert_eq!(weeks(&s.cleaning_groups[0].id), 5);
        assert_eq!(weeks(&bath_id), ROWS_PER_PAGE);
        let doc = document(&s, &snapshot);
        let styles: Vec<_> = doc.pages.iter().map(|p| p.style).collect();
        assert_eq!(styles, [Style::Days, Style::Tick]);
        assert!(doc.pages[0].columns.is_empty());
        assert_eq!(doc.pages[1].columns.len(), 1);
        // Only the tick page's marks get a day from the bot.
        let mark = |page: usize| Mark {
            row: doc.pages[page].rows[0].id.clone(),
            skipped: false,
            day: None,
        };
        let today = NaiveDate::from_ymd_opt(2030, 1, 1).unwrap();
        let mut days = [mark(0)];
        date_ticks(&doc, 0, &mut days, today);
        assert_eq!(days[0].day, None);
        let mut ticks = [mark(1)];
        date_ticks(&doc, 1, &mut ticks, today);
        let row = &doc.pages[1].rows[0];
        assert_eq!(ticks[0].day, Some(tick_day(row.start, row.end, today)));
        // A group's own style goes with the group.
        s.apply_event(DomainEvent::GroupDeleted { group_id: bath_id })
            .unwrap();
        assert!(s.paper_styles.is_empty());
    }

    #[test]
    fn a_view_is_the_same_plan_with_nothing_to_tick() {
        let mut s = upper_floor();
        let mut bath = CleaningGroup::new("Bathroom");
        bath.member_ids = s.cleaning_groups[0].member_ids.clone();
        s.paper_styles.insert(bath.id.clone(), Style::Tick);
        s.cleaning_groups.push(bath);
        let mut snapshot = crate::schedule::build_schedule(&s, ROWS_PER_PAGE);
        one_page_per_group(&mut snapshot, &s);
        let sheet = document(&s, &snapshot);
        let view = view(&s, &snapshot);
        // What `!plan pdf view` prints — the Python tests render it.
        if let Ok(path) = std::env::var("PAPER_VIEW_FIXTURE") {
            std::fs::write(path, serde_json::to_vec_pretty(&view).unwrap()).unwrap();
        }
        assert!(view.view_only && !sheet.view_only);
        assert!(sheet
            .pages
            .iter()
            .any(|p| p.rows.iter().any(|r| !r.fields.is_empty())));
        assert!(view
            .pages
            .iter()
            .all(|p| p.rows.iter().all(|r| r.fields.is_empty())));
        // Otherwise the same: pages, styles, columns, rows, names.
        let shape = |d: &Document| {
            d.pages
                .iter()
                .map(|p| {
                    (
                        p.style,
                        p.columns.len(),
                        p.rows
                            .iter()
                            .map(|r| (r.name.clone(), r.y.to_bits()))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&view), shape(&sheet));
        // Manifests from before load as sheets.
        let mut old = serde_json::to_value(&sheet).unwrap();
        old.as_object_mut().unwrap().remove("view_only");
        assert!(!serde_json::from_value::<Document>(old).unwrap().view_only);
    }

    #[test]
    fn unclear_duties_are_named_in_the_preview() {
        let (_, doc, _) = fixture();
        let page = &doc.pages[0];
        let row = &page.rows[0];
        assert_eq!(unclear_note(page, &[], &[]), "");
        let note = unclear_note(
            page,
            &[
                Unclear {
                    row: row.id.clone(),
                    reason: "only a dot".into(),
                },
                // Not on this page: left out.
                Unclear {
                    row: "elsewhere".into(),
                    reason: "fold".into(),
                },
            ],
            &[],
        );
        assert_eq!(
            note,
            format!(
                "\n\nNot read:\n❓ Week {} · {} · {} — only a dot\n\
                 Record these with !done, or send a clearer photo.",
                row.week, row.name, row.label
            )
        );
        // What the scanner sends is read with it.
        let scan: Scan = serde_json::from_value(serde_json::json!({
            "document": "d", "revision": "r", "page": 0, "marks": [],
            "unclear": [{"row": row.id, "reason": "several days marked"}]
        }))
        .unwrap();
        assert_eq!(scan.unclear[0].reason, "several days marked");
        // A box filled in: said with its day.
        let day = row.start;
        let note = unclear_note(
            page,
            &[],
            &[TakenBack {
                row: row.id.clone(),
                day: Some(day),
            }],
        );
        assert_eq!(
            note,
            format!(
                "\n\nFilled in, so taken back (not counted):\n✏️ Week {} · {} · {} · {}\n\
                 Meant as done? Record it with !done.",
                row.week,
                row.name,
                row.label,
                day.format("%a")
            )
        );
    }

    #[test]
    fn a_tick_counts_in_the_middle_of_its_days() {
        let d = |day| NaiveDate::from_ymd_opt(2026, 10, day).unwrap();
        let later = d(20);
        assert_eq!(tick_day(d(5), d(11), later), d(8)); // Mon–Sun: Thursday
        assert_eq!(tick_day(d(5), d(6), later), d(5)); // Mon–Tue: Monday
        assert_eq!(tick_day(d(8), d(9), later), d(8)); // Thu–Fri: Thursday
        assert_eq!(tick_day(d(8), d(8), later), d(8));
        // Never later than the photo; before the window, its first day
        // (which the import then refuses as the future).
        assert_eq!(tick_day(d(5), d(11), d(6)), d(6));
        assert_eq!(tick_day(d(5), d(11), d(1)), d(5));
    }

    #[test]
    fn ticks_are_dated_and_count_once() {
        let (mut s, _, _) = fixture();
        s.paper_style = Style::Tick;
        let (y, w) = crate::state::current_iso_week();
        let snapshot =
            crate::schedule::build_schedule_from(&s, crate::state::add_weeks(y, w, -1), 1);
        let doc = document(&s, &snapshot);
        s.paper_documents.insert(doc.id.clone(), doc.clone());
        let row = &doc.pages[0].rows[0];
        assert_eq!(row.fields[0].kind, "done");
        let tick = |day| Proposal {
            document: doc.id.clone(),
            page: 0,
            user: "@alice:test".into(),
            room: "!room:test".into(),
            source_event: "$image".into(),
            changes: vec![Mark {
                row: row.id.clone(),
                skipped: false,
                day,
            }],
            created: Utc::now(),
            result: None,
        };
        let mut p = tick(None);
        // Undated, it is refused…
        assert!(changes(&s, &p, false).is_err());
        // …dated, it is last week's Thursday.
        date_ticks(&doc, 0, &mut p.changes, crate::state::today());
        let thursday = row.start + chrono::Duration::days(3);
        assert_eq!(p.changes[0].day, Some(thursday));
        assert_eq!(apply(&mut s, &p, false).unwrap(), 1);
        assert_eq!(s.completions[0].completed_on, Some(thursday));
        // Done already, on whatever day: another photo of the tick changes
        // nothing (on a days sheet another day would be refused).
        assert_eq!(apply(&mut s, &tick(Some(row.start)), false).unwrap(), 0);
        assert_eq!(s.completions.len(), 1);
    }

    #[test]
    fn the_style_is_kept_and_defaults_to_days() {
        assert_eq!(Style::parse("Tick"), Some(Style::Tick));
        assert_eq!(Style::parse("days"), Some(Style::Days));
        assert_eq!(Style::parse("weekly"), None);
        let mut s = State::default();
        assert_eq!(s.paper_style, Style::Days);
        s.paper_style = Style::Tick;
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["paper_style"], "tick");
        let loaded: State = serde_json::from_value(json).unwrap();
        assert_eq!(loaded.paper_style, Style::Tick);
        // Manifests and state from before: days.
        let mut old = serde_json::to_value(&loaded).unwrap();
        old.as_object_mut().unwrap().remove("paper_style");
        let old: State = serde_json::from_value(old).unwrap();
        assert_eq!(old.paper_style, Style::Days);
    }

    #[test]
    fn rooms_are_shown_per_slot_as_symbols_and_what_is_left() {
        let r = |slot, name| {
            let room = room(slot, name);
            (room.kind, room.label)
        };
        let t = |k: &str, l: &str| (k.to_owned(), l.to_owned());
        assert_eq!(r(Some("Colbe"), "Colbe Toilet 3rd"), t("toilet", "3rd"));
        assert_eq!(r(Some("Scharni"), "Scharni Toilet"), t("toilet", ""));
        assert_eq!(r(Some("Scharni"), "Shower Room"), t("shower", ""));
        assert_eq!(r(None, "Kitchen"), t("kitchen", ""));
        assert_eq!(r(None, "WC oben"), t("toilet", "oben"));
        assert_eq!(r(None, "Duschraum"), t("shower", ""));
        // No known kind: the name stays, slot name and all.
        assert_eq!(
            r(Some("Colbe"), "Colbe Hallway"),
            t("other", "Colbe Hallway")
        );

        let (mut s, _, _) = fixture();
        let mut colbe = CleaningSlot::new("Colbe");
        colbe.room_names = vec!["Colbe Toilet 3rd".into(), "Colbe Toilet 4th".into()];
        let mut scharni = CleaningSlot::new("Scharni");
        scharni.room_names = vec!["Scharni Toilet".into(), "Shower Room".into()];
        s.cleaning_groups[0].slots = vec![scharni, colbe];
        let snapshot = crate::schedule::build_schedule(&s, 2);
        let doc = document(&s, &snapshot);
        let groups = &doc.pages[0].room_groups;
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].slot.as_deref(), Some("Scharni"));
        assert_eq!(
            groups[0]
                .rooms
                .iter()
                .map(|r| r.kind.as_str())
                .collect::<Vec<_>>(),
            ["toilet", "shower"]
        );
        assert_eq!(groups[1].slot.as_deref(), Some("Colbe"));
        assert_eq!(
            groups[1]
                .rooms
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["3rd", "4th"]
        );
    }

    #[test]
    fn a_v1_manifest_without_the_new_fields_still_loads() {
        let v1 = serde_json::json!({
            "id": "0123456789abcdef0123456789abcdef",
            "revision": "abcdef012345",
            "created": "2026-09-28T10:00:00Z",
            "pages": [{
                "number": 0, "title": "Kitchen", "rooms": "",
                "rows": [{
                    "id": "r", "group": "g", "slot": null, "slot_index": 0,
                    "year": 2026, "week": 40, "shift": 0, "person": "p",
                    "name": "Alice", "label": "Kitchen", "start": "2026-09-28",
                    "end": "2026-10-04", "status": "", "baseline": "b",
                    "fields": [{"id": "done", "kind": "done", "x": 114.0, "y": 64.0, "label": "Done"}]
                }]
            }]
        });
        let doc: Document = serde_json::from_value(v1).unwrap();
        assert_eq!(doc.layout_version, 1);
        assert!(doc.pages[0].fiducials.is_empty());
        assert_eq!(doc.pages[0].rows[0].task, "");
        assert_eq!(doc.pages[0].rows[0].fields[0].size, 0.);
    }

    #[test]
    fn old_event_and_state_deserialize_without_migration() {
        let old = serde_json::to_value(DomainEvent::CleaningCompleted {
            group_id: "g".into(),
            slot_id: None,
            person_id: "p".into(),
            responsible_person_ids: vec![],
            iso_year: 2026,
            iso_week: 40,
            shift: 0,
            completed_on: None,
        })
        .unwrap();
        let mut old = old;
        assert!(old
            .as_object_mut()
            .unwrap()
            .remove("completed_on")
            .is_some());
        let _: DomainEvent = serde_json::from_value(old).unwrap();
        let s: State = serde_json::from_str("{}").unwrap();
        assert!(s.paper_documents.is_empty());
    }
}
