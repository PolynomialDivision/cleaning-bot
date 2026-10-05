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
/// no Skip box on paper; skipping is recorded in Matrix.
const BOX: f64 = 4.8;
const MONDAY_X: f64 = 111.5;
const DAY_PITCH: f64 = 13.;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Document {
    #[serde(default = "legacy_layout")]
    pub layout_version: u8,
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
    pub rows: Vec<Row>,
    #[serde(default)]
    pub fiducials: Vec<[f64; 2]>,
    #[serde(default)]
    pub qr_center: [f64; 2],
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
        let assignments: Vec<_> = snapshot
            .assignments
            .iter()
            .filter(|a| a.group_id == group)
            .collect();
        let chunks = pages_of(&assignments, |a| (a.iso_year, a.iso_week));
        // One row height for all of the group's pages: as tall as its
        // fullest page allows.
        let fullest = chunks.iter().map(|c| c.len()).max().unwrap_or(1).max(1);
        let pitch = ((ROWS_BOTTOM - ROWS_TOP) / fullest as f64).clamp(ROW_PITCH, MAX_ROW_PITCH);
        for chunk in chunks {
            let mut page = Page {
                number: pages.len(),
                title: chunk[0].group_name.clone(),
                rooms: chunk
                    .iter()
                    .flat_map(|a| a.room_names.iter().cloned())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(" · "),
                rows: Vec::new(),
                fiducials: FIDUCIALS.to_vec(),
                qr_center: QR_CENTER,
            };
            for (i, a) in chunk.iter().enumerate() {
                let id = hash((&a.group_id, &a.slot_id, a.iso_year, a.iso_week, a.shift));
                let mut fields = Vec::new();
                let y = ROWS_TOP + (i as f64 + 0.5) * pitch;
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
                if status.is_empty() && a.assignee.is_some() {
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
                }
                page.rows.push(Row {
                    id,
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
                    y,
                    start: a.start,
                    end: a.end,
                    status,
                    baseline: baseline(state, a),
                    fields,
                });
            }
            pages.push(page);
        }
    }
    Document {
        layout_version: 2,
        id: uuid::Uuid::new_v4().simple().to_string(),
        revision: hash((2, &pages))[..12].into(),
        created: Utc::now(),
        pages,
    }
}
/// Keep, for each group, only as many whole weeks as fill one page — what
/// `!plan pdf` prints when no number of weeks is asked for (a group cleaned
/// by two slots gets ten weeks, a weekly one-slot group 21).
pub fn one_page_per_group(snapshot: &mut ScheduleSnapshot) {
    use std::collections::{HashMap, HashSet};
    let all = std::mem::take(&mut snapshot.assignments);
    let mut rows_in_week: HashMap<(&str, (i32, u32)), usize> = HashMap::new();
    for a in &all {
        *rows_in_week
            .entry((&a.group_id, (a.iso_year, a.iso_week)))
            .or_default() += 1;
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
        return Ok(());
    };
    let doc = ctx.state.lock().await.paper_documents.get(&id).cloned();
    let Some(doc) = doc else {
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
            if events.is_empty() {
                proposal.result = Some("No new changes".into());
                return Ok("📷 Cleaning sheet recognized · no new changes.".into());
            }
            let page = rows(&doc, &proposal)?;
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
                        m.day
                            .map(|d| format!("Done {d}"))
                            .unwrap_or("Skipped".into())
                    )
                })
                .collect();
            Ok(format!("📷 Cleaning sheet recognized\n\n{}\n\n{} changes · ✅ Apply · ❌ Cancel\nOnly you can confirm. Expires in 24 hours.",lines.join("\n\n"),events.len()))
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
        // reads 1.15mm around each box.
        const { assert!(DAY_PITCH - BOX > 2. * 1.15) };
        // Sunday's box and its surroundings stay inside the table (right
        // edge at 196mm, see scripts/paper.py).
        const { assert!(196. - (MONDAY_X + 6. * DAY_PITCH) - BOX / 2. > 1.15) };
        const { assert!(ROW_PITCH - BOX > 2. * 1.15) };
        // The last row stays clear of the footer and the QR code.
        // The table (up to the last row's bottom) stays clear of the QR code.
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

    #[test]
    fn by_default_each_group_fills_one_page() {
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
        g.slots = vec![CleaningSlot::new("Stairs"), CleaningSlot::new("Hall")];
        let mut snapshot = crate::schedule::build_schedule(&s, ROWS_PER_PAGE);
        one_page_per_group(&mut snapshot);
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
