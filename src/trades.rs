//! Handing turns around without typing commands — two ways, both by
//! reaction in the cleaning room:
//!
//!   * **Cleaning early.** Someone ✅s (or `!done`s) while their own shift of
//!     the week is still to come and the running shift of the same slot is
//!     still open: they did the running one, so they swap — they get the
//!     running shift (done), its holder gets their later one. A message
//!     tells the one moved; ↩️ on it swaps back until the later shift
//!     starts, and taking the ✅ back undoes it all.
//!   * **🆘 Who steps in?** 🆘 on the plan or a reminder — or `!sos` for a
//!     later week — asks the room to cover one of your turns. 🙋 on the
//!     request takes it over; 🔄 takes it and gives you the helper's next
//!     turn of that group in return. ↩️ withdraws an open request, or undoes
//!     the handover until its turns start. If nobody has stepped in by the
//!     time it starts, the admins are told.
//!
//! Every handover is a `Trade`: frozen assignments (`SlotAssigned`) from
//! someone to someone, undone together or not at all. The logic is plain
//! state changes, `today` passed in; the Matrix side is at the end.

use std::collections::HashSet;

use anyhow::Result;
use chrono::NaiveDate;
use mxbot_common::matrix_sdk::{
    ruma::{
        events::{
            reaction::ReactionEventContent,
            relation::{Annotation, Reply},
            room::message::{Relation, ReplacementMetadata, RoomMessageEventContent},
            Mentions,
        },
        EventId, OwnedEventId, OwnedUserId, UserId,
    },
    Room,
};

use crate::{
    analytics::DomainEvent,
    commands::Duty,
    domain::{AssignmentSource, CleaningGroup, GroupId, PersonId},
    format,
    rhythm::{date_range, Turn},
    state::{EarlySwap, HelpRequest, HelpStatus, Reassignment, State, Trade},
    view, BotContext,
};

/// 🆘 on the plan or a reminder: "I can't make it".
pub const ASK: &str = "🆘";
/// 🙋 on a request: "I'll do it".
pub const TAKE: &str = "🙋";
/// 🔄 on a request: "I'll do it, you do my next one".
pub const SWAP: &str = "🔄";
/// ↩️ on a request or an early swap: withdraw, or undo.
pub const UNDO: &str = "↩️";

fn bare(key: &str) -> String {
    key.replace('\u{fe0f}', "")
}

pub(crate) fn is_ask(key: &str) -> bool {
    bare(key) == ASK
}

/// 🙋 in any skin tone, 🙋‍♀️ and 🙋‍♂️ too.
fn is_take(key: &str) -> bool {
    key.starts_with(TAKE)
}

pub(crate) fn is_swap(key: &str) -> bool {
    bare(key) == SWAP
}

fn is_undo(key: &str) -> bool {
    bare(key) == bare(UNDO)
}

// ── Trades ────────────────────────────────────────────────────────────────────

fn holder(state: &State, group: &CleaningGroup, slot_index: usize, turn: Turn) -> Option<PersonId> {
    state
        .slot_assignee(group, slot_index, turn)
        .map(|p| p.id.clone())
}

/// One slot of `turn` going to `to`, from whoever holds it now.
fn reassignment(
    state: &State,
    group: &CleaningGroup,
    slot_index: usize,
    turn: Turn,
    to: &PersonId,
    cleaned: bool,
) -> Reassignment {
    let from_source = state
        .slot_assignments
        .iter()
        .find(|a| {
            a.group_id == group.id
                && a.slot_index == slot_index
                && (a.iso_year, a.iso_week, a.shift) == (turn.year, turn.week, turn.shift)
        })
        .map(|a| a.source.clone())
        .unwrap_or_default();
    Reassignment {
        group_id: group.id.clone(),
        slot_index,
        iso_year: turn.year,
        iso_week: turn.week,
        shift: turn.shift,
        from: holder(state, group, slot_index, turn),
        from_source,
        to: Some(to.clone()),
        cleaned,
    }
}

fn apply(state: &mut State, trade: &Trade, source: AssignmentSource, actor: &str) -> Result<()> {
    for c in &trade.changes {
        state.apply_event(DomainEvent::SlotAssigned {
            group_id: c.group_id.clone(),
            slot_index: c.slot_index,
            iso_year: c.iso_year,
            iso_week: c.iso_week,
            shift: c.shift,
            person_id: c.to.clone(),
            source: source.clone(),
            actor_id: Some(actor.to_owned()),
            previous_person_id: c.from.clone(),
        })?;
    }
    Ok(())
}

/// Whether `trade` can still be undone on `today`: everything it handed
/// over is still where it went, and none of it has started or been done —
/// except a turn that was already cleaned when traded.
pub fn undoable(state: &State, trade: &Trade, today: NaiveDate) -> bool {
    !trade.undone
        && trade.changes.iter().all(|c| {
            let Some(group) = state.group_by_id(&c.group_id) else {
                return false;
            };
            let turn = c.turn();
            holder(state, group, c.slot_index, turn) == c.to
                && (c.cleaned
                    || (turn.dates(&group.rhythm).0 > today
                        && !state.is_turn_slot_done(group, c.slot_index, turn)))
        })
}

/// Hand everything back, if `undoable`. Returns whether it did.
fn undo(state: &mut State, trade: &mut Trade, actor: &str, today: NaiveDate) -> Result<bool> {
    if !undoable(state, trade, today) {
        return Ok(false);
    }
    for c in &trade.changes {
        state.apply_event(DomainEvent::SlotAssigned {
            group_id: c.group_id.clone(),
            slot_index: c.slot_index,
            iso_year: c.iso_year,
            iso_week: c.iso_week,
            shift: c.shift,
            person_id: c.from.clone(),
            source: c.from_source.clone(),
            actor_id: Some(actor.to_owned()),
            previous_person_id: c.to.clone(),
        })?;
    }
    trade.undone = true;
    Ok(true)
}

// ── Cleaning early ────────────────────────────────────────────────────────────

/// One early swap: `person` holds `slot_index` in the `later` shift, while
/// the `running` one is still open with `holder` on it.
struct Early {
    group: CleaningGroup,
    slot_index: usize,
    running: Turn,
    later: Turn,
    holder: PersonId,
}

fn early_swaps(
    state: &State,
    person_id: &PersonId,
    (year, week): (i32, u32),
    only_group: Option<&GroupId>,
    today: NaiveDate,
) -> Vec<Early> {
    let mut found = Vec::new();
    for group in state.cleaning_groups.iter().filter(|g| g.is_active) {
        if only_group.is_some_and(|id| id != &group.id) {
            continue;
        }
        let turns = state.turns_in_week(group, year, week);
        let Some(running) = turns.iter().copied().find(|t| {
            let (start, end) = t.dates(&group.rhythm);
            start <= today && today <= end
        }) else {
            continue;
        };
        let mut used = HashSet::new();
        for later in turns.iter().filter(|t| t.dates(&group.rhythm).0 > today) {
            for slot_index in state.held_slots(group, person_id, *later) {
                if used.contains(&slot_index)
                    || state.is_turn_slot_done(group, slot_index, *later)
                    || state.is_turn_slot_done(group, slot_index, running)
                {
                    continue;
                }
                let Some(holder) = holder(state, group, slot_index, running) else {
                    continue;
                };
                if &holder == person_id {
                    continue;
                }
                used.insert(slot_index);
                found.push(Early {
                    group: group.clone(),
                    slot_index,
                    running,
                    later: *later,
                    holder,
                });
            }
        }
    }
    found
}

/// `person_id` (`mxid`) cleaned in `week` with nothing of theirs running:
/// if the running shift of a slot they hold later is still open, swap —
/// they take it, marked done; its holder takes their later shift. `None`
/// when there's nothing to swap. Only for someone with no started turn
/// open (`commands::markable_duties` is empty).
pub fn swap_early(
    state: &mut State,
    person_id: &PersonId,
    mxid: &str,
    week: (i32, u32),
    only_group: Option<&GroupId>,
    today: NaiveDate,
) -> Result<Option<(Trade, Vec<Duty>)>> {
    let found = early_swaps(state, person_id, week, only_group, today);
    if found.is_empty() {
        return Ok(None);
    }
    let mut trade = Trade::default();
    let mut cleaned = Vec::new();
    for e in found {
        // In pairs: the running shift, then the later one.
        trade.changes.push(reassignment(
            state,
            &e.group,
            e.slot_index,
            e.running,
            person_id,
            true,
        ));
        trade.changes.push(reassignment(
            state,
            &e.group,
            e.slot_index,
            e.later,
            &e.holder,
            false,
        ));
        cleaned.push(Duty {
            group: e.group,
            slot_index: e.slot_index,
            turn: e.running,
        });
    }
    apply(state, &trade, AssignmentSource::Swap, mxid)?;
    crate::commands::mark_duties_done(state, person_id, &cleaned)?;
    Ok(Some((trade, cleaned)))
}

/// Who an early swap moved to a later shift (Matrix IDs).
pub fn moved(state: &State, trade: &Trade) -> Vec<String> {
    let mut out: Vec<String> = trade
        .changes
        .iter()
        .filter(|c| c.cleaned)
        .filter_map(|c| c.from.as_ref())
        .filter_map(|id| state.person_by_id(id)?.matrix_id.clone())
        .collect();
    out.dedup();
    out
}

/// A person as a pill that notifies (`@mxid`) or not (a link).
fn person(state: &State, id: Option<&PersonId>, notify: bool) -> String {
    match id.and_then(|id| state.person_by_id(id)) {
        Some(p) => match (&p.matrix_id, notify) {
            (Some(mxid), true) => mxid.clone(),
            _ => view::user_link(p),
        },
        None => "nobody".to_owned(),
    }
}

fn duty_of(state: &State, c: &Reassignment) -> Option<Duty> {
    Some(Duty {
        group: state.group_by_id(&c.group_id)?.clone(),
        slot_index: c.slot_index,
        turn: c.turn(),
    })
}

/// "Thu–Sun (8 – 11 Oct)" — a shift of a group split into shifts, else the
/// week's dates.
pub fn when(duty: &Duty) -> String {
    let (start, end) = duty.turn.dates(&duty.group.rhythm);
    match duty.turn.shift_label(&duty.group.rhythm) {
        Some(shift) => format!("{shift} ({})", date_range(start, end)),
        None => date_range(start, end),
    }
}

/// The message to whoever an early swap moved.
///
/// ```text
/// 🔄 @bob — alice already cleaned **2nd Floor / Scharni · Mon–Wed** for you, so you two swapped: you're on **Thu–Sun (8 – 11 Oct)** now.
/// Doesn't suit you? React ↩️ and I'll swap you back.
/// ```
pub fn early_text(state: &State, swap: &EarlySwap) -> String {
    let mut lines = Vec::new();
    for pair in swap.trade.changes.chunks(2) {
        let [running, later] = pair else { continue };
        let (Some(done), Some(theirs)) = (duty_of(state, running), duty_of(state, later)) else {
            continue;
        };
        let moved = person(state, running.from.as_ref(), true);
        let cleaner = person(state, running.to.as_ref(), false);
        lines.push(if swap.trade.undone {
            format!(
                "↩️ Swapped back: {moved} keeps **{}**, {cleaner} keeps **{}**.",
                done.label(),
                when(&theirs)
            )
        } else {
            format!(
                "🔄 {moved} — {cleaner} already cleaned **{}** for you, so you two swapped: \
                 you're on **{}** now.",
                done.label(),
                when(&theirs)
            )
        });
    }
    if !swap.trade.undone {
        lines.push("Doesn't suit you? React ↩️ and I'll swap you back.".to_owned());
    }
    lines.join("\n")
}

// ── 🆘 Who steps in? ──────────────────────────────────────────────────────────

/// `person_id`'s turns in `week` that a 🆘 asks cover for: not done, not over.
pub fn askable(
    state: &State,
    person_id: &PersonId,
    (year, week): (i32, u32),
    today: NaiveDate,
) -> Vec<Duty> {
    let mut duties = Vec::new();
    for group in state.cleaning_groups.iter().filter(|g| g.is_active) {
        for turn in state.turns_in_week(group, year, week) {
            if turn.dates(&group.rhythm).1 < today {
                continue;
            }
            for slot_index in state.held_slots(group, person_id, turn) {
                if !state.is_turn_slot_done(group, slot_index, turn) {
                    duties.push(Duty {
                        group: group.clone(),
                        slot_index,
                        turn,
                    });
                }
            }
        }
    }
    duties
}

/// The open request about `duty`, if there is one.
pub fn open_request<'s>(state: &'s State, duty: &Duty) -> Option<(&'s String, &'s HelpRequest)> {
    state.help_requests.iter().find(|(_, r)| {
        r.status == HelpStatus::Open
            && r.group_id == duty.group.id
            && r.slot_index == duty.slot_index
            && r.turn() == duty.turn
    })
}

pub fn new_request(duty: &Duty, requester: &str, trigger: Option<String>) -> HelpRequest {
    HelpRequest {
        requester: requester.to_owned(),
        group_id: duty.group.id.clone(),
        slot_index: duty.slot_index,
        iso_year: duty.turn.year,
        iso_week: duty.turn.week,
        shift: duty.turn.shift,
        status: HelpStatus::Open,
        helper: None,
        trade: None,
        trigger,
        swap_only: false,
        admins_told: false,
        mentioned: Vec::new(),
        rendered: String::new(),
    }
}

fn request_duty(state: &State, req: &HelpRequest) -> Option<Duty> {
    Some(Duty {
        group: state.group_by_id(&req.group_id)?.clone(),
        slot_index: req.slot_index,
        turn: req.turn(),
    })
}

/// Everyone a new request about `req` notifies: the group's members but
/// the one asking.
pub fn request_pings(state: &State, req: &HelpRequest) -> Vec<String> {
    let Some(group) = state.group_by_id(&req.group_id) else {
        return Vec::new();
    };
    state
        .members_of(group)
        .into_iter()
        .filter_map(|p| p.matrix_id.clone())
        .filter(|mxid| mxid != &req.requester)
        .collect()
}

fn requester_link(state: &State, req: &HelpRequest) -> String {
    match state.person_by_matrix_id(&req.requester) {
        Some(p) => view::user_link(p),
        None => view::user_id_link(&req.requester),
    }
}

fn helper_link(state: &State, req: &HelpRequest) -> String {
    let mxid = req.helper.as_deref().unwrap_or_default();
    match state.person_by_matrix_id(mxid) {
        Some(p) => view::user_link(p),
        None => view::user_id_link(mxid),
    }
}

/// A request as it reads now, and whom it notifies.
///
/// ```text
/// 🆘 **Who can step in?** alice can't make it:
/// **2nd Floor / Scharni · Thu–Sun (8 – 11 Oct)** · week 41
/// 🙋 I'll do it · 🔄 swap — you take it, alice takes your next turn
/// (alice: ↩️ if you can make it after all)
/// @bob @carl
/// ```
pub fn request_text(state: &State, req: &HelpRequest) -> (String, Vec<String>) {
    let Some(duty) = request_duty(state, req) else {
        return ("⌛ Closed — that group is gone.".to_owned(), Vec::new());
    };
    let what = format!(
        "**{} · {}** · week {}",
        duty.place(),
        when(&duty),
        req.iso_week
    );
    let asker = requester_link(state, req);
    match req.status {
        HelpStatus::Open => {
            let pings = request_pings(state, req);
            let name = state
                .person_by_matrix_id(&req.requester)
                .map(|p| p.display_name.clone())
                .unwrap_or_else(|| req.requester.clone());
            let mut lines = if req.swap_only {
                vec![
                    format!("🔄 **Who swaps?** {asker} would like to swap:"),
                    what,
                    format!("🔄 swap — you take it, {name} takes your next turn"),
                    format!("({name}: ↩️ to take it back)"),
                ]
            } else {
                vec![
                    format!("🆘 **Who can step in?** {asker} can't make it:"),
                    what,
                    format!("🙋 I'll do it · 🔄 swap — you take it, {name} takes your next turn"),
                    format!("({name}: ↩️ if you can make it after all)"),
                ]
            };
            if !pings.is_empty() {
                lines.push(pings.join(" "));
            }
            (lines.join("\n"), pings)
        }
        HelpStatus::Taken => (
            format!(
                "✅ **Sorted!** {} does {what} for {} — thank you 💛\n↩️ undoes it, until it starts.",
                helper_link(state, req),
                req.requester
            ),
            vec![req.requester.clone()],
        ),
        HelpStatus::Swapped => {
            let instead = req
                .trade
                .as_ref()
                .and_then(|t| t.changes.get(1))
                .and_then(|c| duty_of(state, c))
                .map(|d| {
                    format!(
                        " — and {} does **{} · {}** · week {} instead",
                        req.requester,
                        d.place(),
                        when(&d),
                        d.turn.week
                    )
                })
                .unwrap_or_default();
            (
                format!(
                    "✅ **Swapped!** {} does {what} for {}{instead}. Thank you 💛\n↩️ undoes it, until it starts.",
                    helper_link(state, req),
                    req.requester
                ),
                vec![req.requester.clone()],
            )
        }
        HelpStatus::Withdrawn if req.swap_only => (
            format!("👍 {asker} keeps {what} after all."),
            Vec::new(),
        ),
        HelpStatus::Withdrawn => (
            format!("👍 {asker} can make it after all: {what}."),
            Vec::new(),
        ),
        HelpStatus::Closed => {
            let done = state.is_turn_slot_done(&duty.group, duty.slot_index, duty.turn);
            (
                if done {
                    format!("✨ {what} got done — closed.")
                } else {
                    format!("⌛ Closed: {what} ({asker}).")
                },
                Vec::new(),
            )
        }
    }
}

/// What a 🙋 or 🔄 on a request did.
#[derive(Debug, PartialEq)]
pub enum Taken {
    /// Handed over (and, with 🔄, a turn handed back).
    Done,
    /// Not open (anymore): nothing changed.
    NotOpen,
    /// The one asking tapped it themselves.
    Own,
    /// 🔄, but the helper has no upcoming turn of that group to give.
    NothingToSwap,
    /// 🙋 on a request that only asks for a swap.
    SwapOnly,
}

/// `helper` (Matrix ID) steps in on `req` — with `swap`, giving their next
/// turn of the group for it.
pub fn take(
    state: &mut State,
    req: &mut HelpRequest,
    helper: &str,
    swap: bool,
    today: NaiveDate,
) -> Result<Taken> {
    if settle(state, req, today) || req.status != HelpStatus::Open {
        return Ok(Taken::NotOpen);
    }
    if helper == req.requester {
        return Ok(Taken::Own);
    }
    if req.swap_only && !swap {
        return Ok(Taken::SwapOnly);
    }
    let Some(duty) = request_duty(state, req) else {
        return Ok(Taken::NotOpen);
    };
    state.apply_event(DomainEvent::PersonCreated {
        person_id: uuid::Uuid::new_v4().to_string(),
        display_name: helper.into(),
        matrix_id: Some(helper.into()),
    })?;
    let helper_id = state
        .person_by_matrix_id(helper)
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{helper} has no person record"))?;
    let requester_id = state
        .person_by_matrix_id(&req.requester)
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{} has no person record", req.requester))?;
    let mut trade = Trade::default();
    trade.changes.push(reassignment(
        state,
        &duty.group,
        duty.slot_index,
        duty.turn,
        &helper_id,
        false,
    ));
    if swap {
        let Some(give) = next_turn_to_give(state, &helper_id, &requester_id, &duty, today) else {
            return Ok(Taken::NothingToSwap);
        };
        trade.changes.push(reassignment(
            state,
            &give.group,
            give.slot_index,
            give.turn,
            &requester_id,
            false,
        ));
    }
    let source = if swap {
        AssignmentSource::Swap
    } else {
        AssignmentSource::Takeover
    };
    apply(state, &trade, source, helper)?;
    req.status = if swap {
        HelpStatus::Swapped
    } else {
        HelpStatus::Taken
    };
    req.helper = Some(helper.to_owned());
    req.trade = Some(trade);
    Ok(Taken::Done)
}

/// The helper's next turn of the same group to give in return: not yet
/// started or done, not the turn asked about, and not one in which the one
/// asking already cleans.
fn next_turn_to_give(
    state: &State,
    helper_id: &PersonId,
    requester_id: &PersonId,
    asked: &Duty,
    today: NaiveDate,
) -> Option<Duty> {
    use chrono::Datelike;
    let from = (today.iso_week().year(), today.iso_week().week());
    let horizon = crate::state::add_weeks(asked.turn.year, asked.turn.week, 52);
    state
        .turns_between(&asked.group, from, horizon)
        .into_iter()
        .filter(|t| *t != asked.turn && t.dates(&asked.group.rhythm).0 > today)
        .filter(|t| state.held_slots(&asked.group, requester_id, *t).is_empty())
        .find_map(|turn| {
            state
                .held_slots(&asked.group, helper_id, turn)
                .into_iter()
                .find(|&slot| !state.is_turn_slot_done(&asked.group, slot, turn))
                .map(|slot_index| Duty {
                    group: asked.group.clone(),
                    slot_index,
                    turn,
                })
        })
}

/// What a ↩️ on a request did.
#[derive(Debug, PartialEq)]
pub enum Back {
    /// The one asking withdrew the open request.
    Withdrawn,
    /// The handover was undone; the request is open again.
    Undone,
    /// Too late to undo: it has started (or changed since).
    TooLate,
    /// Not theirs to take back, or nothing to: nothing changed.
    Nothing,
}

/// `who` (Matrix ID) reacted ↩️ on `req`.
pub fn back(state: &mut State, req: &mut HelpRequest, who: &str, today: NaiveDate) -> Result<Back> {
    match req.status {
        HelpStatus::Open if who == req.requester => {
            req.status = HelpStatus::Withdrawn;
            Ok(Back::Withdrawn)
        }
        HelpStatus::Taken | HelpStatus::Swapped
            if who == req.requester || req.helper.as_deref() == Some(who) =>
        {
            let Some(mut trade) = req.trade.clone() else {
                return Ok(Back::Nothing);
            };
            if !undo(state, &mut trade, who, today)? {
                return Ok(Back::TooLate);
            }
            req.status = HelpStatus::Open;
            req.helper = None;
            req.trade = None;
            Ok(Back::Undone)
        }
        _ => Ok(Back::Nothing),
    }
}

/// Close an open request that settled otherwise: done, over, or no longer
/// the asker's. Returns whether it closed.
pub fn settle(state: &State, req: &mut HelpRequest, today: NaiveDate) -> bool {
    if req.status != HelpStatus::Open {
        return false;
    }
    let still_theirs = request_duty(state, req).is_some_and(|d| {
        d.turn.dates(&d.group.rhythm).1 >= today
            && !state.is_turn_slot_done(&d.group, d.slot_index, d.turn)
            && state
                .slot_assignee(&d.group, d.slot_index, d.turn)
                .and_then(|p| p.matrix_id.as_deref())
                == Some(req.requester.as_str())
    });
    if still_theirs {
        return false;
    }
    req.status = HelpStatus::Closed;
    true
}

/// An open request whose turn has started: time to tell the admins.
fn needs_admins(state: &State, req: &HelpRequest, today: NaiveDate) -> bool {
    req.status == HelpStatus::Open
        && !req.admins_told
        && request_duty(state, req).is_some_and(|d| d.turn.dates(&d.group.rhythm).0 <= today)
}

/// Requests and early swaps whose turns are long over are forgotten.
/// Returns whether any were.
fn prune(state: &mut State, today: NaiveDate) -> bool {
    let before = state.help_requests.len() + state.early_swaps.len();
    let cutoff = today - chrono::Duration::days(14);
    let current = |year: i32, week: u32| {
        crate::rhythm::week_monday(year, week) + chrono::Duration::days(6) >= cutoff
    };
    state
        .help_requests
        .retain(|_, r| current(r.iso_year, r.iso_week));
    state.early_swaps.retain(|_, s| {
        s.trade
            .changes
            .iter()
            .any(|c| current(c.iso_year, c.iso_week))
    });
    state.help_requests.len() + state.early_swaps.len() != before
}

// ── Matrix ────────────────────────────────────────────────────────────────────

fn mentions(mxids: &[String]) -> Mentions {
    Mentions::with_user_ids(mxids.iter().filter_map(|m| m.parse::<OwnedUserId>().ok()))
}

async fn content(
    ctx: &BotContext,
    room: &Room,
    text: &str,
    notify: &[String],
) -> RoomMessageEventContent {
    let mut content = crate::names::mentionify(ctx, text, room).await;
    content.mentions = Some(mentions(notify));
    content
}

/// Edit `event_id` to `text`, notifying who of `notify` wasn't in `before`.
async fn edit(
    ctx: &BotContext,
    room: &Room,
    event_id: &str,
    text: &str,
    notify: &[String],
    before: &[String],
) -> bool {
    let Ok(id) = event_id.parse::<OwnedEventId>() else {
        return false;
    };
    let edit = content(ctx, room, text, notify)
        .await
        .make_replacement(ReplacementMetadata::new(id, Some(mentions(before))));
    match room.send(edit).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("Failed to update a swap message: {e}");
            false
        }
    }
}

/// A short answer to `user`, as a reply to `to`.
async fn reply(room: &Room, to: &str, user: &str, text: &str) {
    let Ok(id) = to.parse::<OwnedEventId>() else {
        return;
    };
    let mut content = format::intentional(format::mentionify(&format!("{user} {text}")));
    content.relates_to = Some(Relation::Reply(Reply::with_event_id(id)));
    if let Err(e) = room.send(content).await {
        tracing::warn!("Failed to answer a swap reaction: {e}");
    }
}

async fn seed(room: &Room, event_id: &OwnedEventId, keys: &[&str]) {
    for key in keys {
        let reaction = ReactionEventContent::new(Annotation::new(event_id.clone(), (*key).into()));
        if let Err(e) = room.send(reaction).await {
            tracing::warn!("Failed to seed {key}: {e}");
        }
    }
}

async fn save(ctx: &BotContext, state: &mut State) -> bool {
    match state.save(&ctx.state_path).await {
        Ok(()) => true,
        Err(e) => {
            tracing::error!("Failed to save a swap: {e}");
            false
        }
    }
}

async fn refresh_plan(ctx: &BotContext, room: &Room) {
    let (year, week) = crate::state::current_iso_week();
    crate::scheduler::refresh_pinned_plan(ctx, room, year, week).await;
}

/// Tell whoever an early swap moved, and keep the message for ↩️.
pub async fn post_early_swap(
    ctx: &BotContext,
    room: &Room,
    cleaner: &str,
    trade: Trade,
    reaction_id: Option<String>,
) {
    let (text, notify, record) = {
        let state = ctx.state.lock().await;
        let mut record = EarlySwap {
            moved: moved(&state, &trade),
            trade,
            cleaner: cleaner.to_owned(),
            reaction_id,
            rendered: String::new(),
        };
        record.rendered = early_text(&state, &record);
        (record.rendered.clone(), record.moved.clone(), record)
    };
    let event_id = match room.send(content(ctx, room, &text, &notify).await).await {
        Ok(response) => response.response.event_id,
        Err(e) => {
            tracing::warn!("Failed to tell about an early swap: {e}");
            return;
        }
    };
    {
        let mut state = ctx.state.lock().await;
        state.early_swaps.insert(event_id.to_string(), record);
        save(ctx, &mut state).await;
    }
    seed(room, &event_id, &[UNDO]).await;
}

/// Post a request for `req` in the cleaning room.
pub async fn post_request(
    ctx: &BotContext,
    room: &Room,
    mut req: HelpRequest,
) -> Option<OwnedEventId> {
    let (text, notify) = request_text(&*ctx.state.lock().await, &req);
    let event_id = match room.send(content(ctx, room, &text, &notify).await).await {
        Ok(response) => response.response.event_id,
        Err(e) => {
            tracing::warn!("Failed to post a 🆘 request: {e}");
            return None;
        }
    };
    let swap_only = req.swap_only;
    req.rendered = text;
    req.mentioned = notify;
    {
        let mut state = ctx.state.lock().await;
        state.help_requests.insert(event_id.to_string(), req);
        save(ctx, &mut state).await;
    }
    let buttons: &[&str] = if swap_only { &[SWAP] } else { &[TAKE, SWAP] };
    seed(room, &event_id, buttons).await;
    Some(event_id)
}

/// A reaction in the cleaning room that may be about swapping: 🆘 on the
/// plan or a reminder; 🙋, 🔄 or ↩️ on a request; ↩️ on an early swap.
/// Returns whether it was one of these (and so needs nothing else).
pub async fn on_reaction(
    ctx: &BotContext,
    room: &Room,
    reaction_id: &EventId,
    sender: &UserId,
    reacted_to: &str,
    key: &str,
) -> bool {
    let what = {
        let state = ctx.state.lock().await;
        if state.help_requests.contains_key(reacted_to)
            && (is_take(key) || is_swap(key) || is_undo(key))
        {
            Some(Tap::Request)
        } else if state.early_swaps.contains_key(reacted_to) && is_undo(key) {
            Some(Tap::EarlyUndo)
        } else if is_ask(key) {
            state
                .weekly_plan_event_ids
                .get(reacted_to)
                .copied()
                .or_else(|| {
                    state
                        .reminder_messages
                        .get(reacted_to)
                        .map(|r| (r.iso_year, r.iso_week))
                })
                .map(Tap::Ask)
        } else {
            None
        }
    };
    let Some(what) = what else {
        return false;
    };
    {
        // Once per reaction, also when delivered again later.
        let mut state = ctx.state.lock().await;
        if !state.trade_taps.insert(reaction_id.to_string()) || !save(ctx, &mut state).await {
            return true;
        }
    }
    let today = crate::state::today();
    match what {
        Tap::Ask(week) => ask(ctx, room, reaction_id, sender, reacted_to, week, today).await,
        Tap::Request => on_request(ctx, room, sender, reacted_to, key, today).await,
        Tap::EarlyUndo => undo_early(ctx, room, sender.as_str(), reacted_to, None, today).await,
    }
    true
}

enum Tap {
    Ask((i32, u32)),
    Request,
    EarlyUndo,
}

/// 🆘 on the plan or a reminder of `week`: a request for each of the
/// sender's open turns that week.
async fn ask(
    ctx: &BotContext,
    room: &Room,
    reaction_id: &EventId,
    sender: &UserId,
    reacted_to: &str,
    week: (i32, u32),
    today: NaiveDate,
) {
    let (fresh, asked) = {
        let state = ctx.state.lock().await;
        let duties = state
            .person_by_matrix_id(sender.as_str())
            .map(|p| askable(&state, &p.id, week, today))
            .unwrap_or_default();
        let asked = duties.len();
        let fresh: Vec<HelpRequest> = duties
            .iter()
            .filter(|d| open_request(&state, d).is_none())
            .map(|d| new_request(d, sender.as_str(), Some(reaction_id.to_string())))
            .collect();
        (fresh, asked)
    };
    if asked == 0 {
        reply(
            room,
            reacted_to,
            sender.as_str(),
            "— you have no open turn this week, nothing to ask cover for.",
        )
        .await;
        return;
    }
    if fresh.is_empty() {
        reply(
            room,
            reacted_to,
            sender.as_str(),
            "— you already asked, see your 🆘 above.",
        )
        .await;
        return;
    }
    for req in fresh {
        post_request(ctx, room, req).await;
    }
}

/// 🙋, 🔄 or ↩️ on a request.
async fn on_request(
    ctx: &BotContext,
    room: &Room,
    sender: &UserId,
    request_id: &str,
    key: &str,
    today: NaiveDate,
) {
    let who = sender.as_str();
    let mut state = ctx.state.lock().await;
    let Some(mut req) = state.help_requests.get(request_id).cloned() else {
        return;
    };
    let before = req.clone();
    let swap_only = req.swap_only;
    let mut answer = None;
    let result = if is_undo(key) {
        back(&mut state, &mut req, who, today).map(|outcome| {
            if outcome == Back::TooLate {
                answer = Some("— too late to undo, it has started (or changed since).");
            }
        })
    } else {
        take(&mut state, &mut req, who, is_swap(key), today).map(|outcome| match outcome {
            Taken::Own => {
                answer = Some("— that's your own request. ↩️ withdraws it.");
            }
            Taken::NothingToSwap if swap_only => {
                answer = Some("— you have no upcoming turn in that group to swap.");
            }
            Taken::NothingToSwap => {
                answer = Some(
                    "— you have no upcoming turn in that group to swap. 🙋 takes it over instead.",
                );
            }
            Taken::SwapOnly => {
                answer = Some("— this one is a swap: 🔄 takes it and gives them your next turn.");
            }
            Taken::Done | Taken::NotOpen => {}
        })
    };
    if let Err(e) = result {
        tracing::error!("A swap tap failed: {e}");
        return;
    }
    let changed = req != before;
    if changed {
        state.help_requests.insert(request_id.to_owned(), req);
        if !save(ctx, &mut state).await {
            return;
        }
    }
    drop(state);
    if let Some(text) = answer {
        reply(room, request_id, who, text).await;
    }
    if changed {
        update_request(ctx, room, request_id).await;
        refresh_plan(ctx, room).await;
    }
}

/// Bring a request's message up to date.
async fn update_request(ctx: &BotContext, room: &Room, request_id: &str) {
    let (text, notify, before) = {
        let state = ctx.state.lock().await;
        let Some(req) = state.help_requests.get(request_id) else {
            return;
        };
        let (text, notify) = request_text(&state, req);
        if text == req.rendered {
            return;
        }
        (text, notify, req.mentioned.clone())
    };
    if edit(ctx, room, request_id, &text, &notify, &before).await {
        let mut state = ctx.state.lock().await;
        if let Some(req) = state.help_requests.get_mut(request_id) {
            req.rendered = text;
        }
        save(ctx, &mut state).await;
    }
}

/// ↩️ on an early swap (by the cleaner or one moved), or the ✅ behind it
/// taken back (`reaction`, by the cleaner): swap back.
async fn undo_early(
    ctx: &BotContext,
    room: &Room,
    who: &str,
    notice_id: &str,
    reaction: Option<&str>,
    today: NaiveDate,
) {
    let mut state = ctx.state.lock().await;
    let Some(mut swap) = state.early_swaps.get(notice_id).cloned() else {
        return;
    };
    let allowed =
        swap.cleaner == who || (reaction.is_none() && swap.moved.iter().any(|m| m == who));
    if !allowed || swap.trade.undone {
        return;
    }
    match undo(&mut state, &mut swap.trade, who, today) {
        Ok(true) => {}
        Ok(false) => {
            drop(state);
            if reaction.is_none() {
                reply(
                    room,
                    notice_id,
                    who,
                    "— too late to swap back, the later shift has started (or changed since).",
                )
                .await;
            }
            return;
        }
        Err(e) => {
            tracing::error!("Swapping back failed: {e}");
            return;
        }
    }
    let text = early_text(&state, &swap);
    state.early_swaps.insert(notice_id.to_owned(), swap);
    if !save(ctx, &mut state).await {
        return;
    }
    drop(state);
    if edit(ctx, room, notice_id, &text, &[], &[]).await {
        let mut state = ctx.state.lock().await;
        if let Some(swap) = state.early_swaps.get_mut(notice_id) {
            swap.rendered = text;
        }
        save(ctx, &mut state).await;
    }
    refresh_plan(ctx, room).await;
}

/// A reaction taken back (`redacted_id`, by `sender`): a ✅ behind an early
/// swap swaps back; a 🆘 withdraws its open requests.
pub async fn on_redaction(ctx: &BotContext, room: &Room, sender: &str, redacted_id: &str) {
    let today = crate::state::today();
    let (early, requests) = {
        let mut state = ctx.state.lock().await;
        let early = state
            .early_swaps
            .iter()
            .find(|(_, s)| s.reaction_id.as_deref() == Some(redacted_id))
            .map(|(id, _)| id.clone());
        let mut requests = Vec::new();
        for (id, req) in state.help_requests.iter_mut() {
            if req.trigger.as_deref() == Some(redacted_id)
                && req.requester == sender
                && req.status == HelpStatus::Open
            {
                req.status = HelpStatus::Withdrawn;
                requests.push(id.clone());
            }
        }
        if !requests.is_empty() && !save(ctx, &mut state).await {
            return;
        }
        (early, requests)
    };
    if let Some(notice) = early {
        undo_early(ctx, room, sender, &notice, Some(redacted_id), today).await;
    }
    for id in requests {
        update_request(ctx, room, &id).await;
    }
}

/// Close requests that settled otherwise, tell the admins about open ones
/// whose turn has started (`nudge`), and forget old ones.
pub async fn tidy(ctx: &BotContext, room: &Room, nudge: bool) {
    let today = crate::state::today();
    let (stale, nudges) = {
        let mut state = ctx.state.lock().await;
        let mut changed = prune(&mut state, today);
        let mut requests = std::mem::take(&mut state.help_requests);
        let mut nudges = Vec::new();
        for (id, req) in requests.iter_mut() {
            changed |= settle(&state, req, today);
            if nudge && needs_admins(&state, req, today) {
                req.admins_told = true;
                changed = true;
                nudges.push((id.clone(), request_duty(&state, req).map(|d| d.label())));
            }
        }
        state.help_requests = requests;
        if changed && !save(ctx, &mut state).await {
            return;
        }
        let stale: Vec<String> = state
            .help_requests
            .iter()
            .filter(|(_, r)| request_text(&state, r).0 != r.rendered)
            .map(|(id, _)| id.clone())
            .collect();
        (stale, nudges)
    };
    for id in stale {
        update_request(ctx, room, &id).await;
    }
    let mut admins: Vec<String> = ctx.admin_users.iter().map(|u| u.to_string()).collect();
    admins.sort();
    if nudges.is_empty() || admins.is_empty() {
        return;
    }
    for (id, label) in nudges {
        let Ok(event_id) = id.parse::<OwnedEventId>() else {
            continue;
        };
        let text = format!(
            "👀 Nobody has stepped in yet for **{}** — {} can you help sort it out? (!plan assign)",
            label.unwrap_or_default(),
            admins.join(" ")
        );
        let mut message = content(ctx, room, &text, &admins).await;
        message.relates_to = Some(Relation::Reply(Reply::with_event_id(event_id)));
        if let Err(e) = room.send(message).await {
            tracing::warn!("Failed to tell the admins about a 🆘: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CleaningGroup, Person},
        rhythm::Rhythm,
        state::add_weeks,
    };

    /// A group cleaned Mon–Wed and Thu–Sun with alice, bob and carol, two
    /// weeks ahead (so nothing the real clock does interferes), its turns
    /// frozen: week 1 alice | bob, week 2 carol | alice.
    fn split_week() -> (State, CleaningGroup, (i32, u32), [PersonId; 3]) {
        let people = ["@alice:x.org", "@bob:x.org", "@carol:x.org"].map(Person::new_matrix);
        let ids = people.clone().map(|p| p.id);
        let mut group = CleaningGroup::new("2nd Floor");
        group.rhythm = Rhythm {
            every_weeks: Some(1),
            shift_starts: vec![0, 3],
            ..Rhythm::weekly()
        };
        group.member_ids = ids.to_vec();
        let mut state = State::default();
        state.persons = people.to_vec();
        state.cleaning_groups.push(group.clone());
        let (y, w) = crate::state::current_iso_week();
        let week = add_weeks(y, w, 2);
        let next = add_weeks(week.0, week.1, 1);
        for ((year, wk), shift, who) in [(week, 0, 0), (week, 1, 1), (next, 0, 2), (next, 1, 0)] {
            state
                .apply_event(DomainEvent::SlotAssigned {
                    group_id: group.id.clone(),
                    slot_index: 0,
                    iso_year: year,
                    iso_week: wk,
                    shift,
                    person_id: Some(ids[who].clone()),
                    source: AssignmentSource::RoundRobin,
                    actor_id: None,
                    previous_person_id: None,
                })
                .unwrap();
        }
        (state, group, week, ids)
    }

    fn day(week: (i32, u32), weekday: i64) -> NaiveDate {
        crate::rhythm::week_monday(week.0, week.1) + chrono::Duration::days(weekday)
    }

    fn who(state: &State, group: &CleaningGroup, week: (i32, u32), shift: u8) -> String {
        state
            .slot_assignee(group, 0, Turn::new(week.0, week.1, shift))
            .unwrap()
            .matrix_id
            .clone()
            .unwrap()
    }

    #[test]
    fn cleaning_the_running_shift_early_swaps_and_can_be_undone_until_the_later_starts() {
        let (mut state, group, week, ids) = split_week();
        let tuesday = day(week, 1);
        // Bob (Thu–Sun) cleans on Tuesday, while alice's Mon–Wed is open.
        let (mut trade, cleaned) =
            swap_early(&mut state, &ids[1], "@bob:x.org", week, None, tuesday)
                .unwrap()
                .expect("a swap");
        assert_eq!(cleaned.len(), 1);
        assert_eq!(who(&state, &group, week, 0), "@bob:x.org");
        assert_eq!(who(&state, &group, week, 1), "@alice:x.org");
        assert!(state.is_turn_slot_done(&group, 0, Turn::new(week.0, week.1, 0)));
        assert!(!state.is_turn_slot_done(&group, 0, Turn::new(week.0, week.1, 1)));
        assert_eq!(moved(&state, &trade), ["@alice:x.org"]);
        let swap = EarlySwap {
            trade: trade.clone(),
            cleaner: "@bob:x.org".into(),
            moved: vec!["@alice:x.org".into()],
            reaction_id: None,
            rendered: String::new(),
        };
        let text = early_text(&state, &swap);
        assert!(text.starts_with("🔄 @alice:x.org — [bob]"), "{text}");
        assert!(text.contains("**2nd Floor · Mon–Wed**"), "{text}");
        assert!(text.contains("you're on **Thu–Sun ("), "{text}");
        assert!(text.contains("↩️"), "{text}");

        // Once Thursday came, it stays.
        assert!(!undoable(&state, &trade, day(week, 3)));
        // Before, it goes back — the cleaning stays recorded.
        assert!(undo(&mut state, &mut trade, "@alice:x.org", day(week, 2)).unwrap());
        assert_eq!(who(&state, &group, week, 0), "@alice:x.org");
        assert_eq!(who(&state, &group, week, 1), "@bob:x.org");
        assert!(state.is_turn_slot_done(&group, 0, Turn::new(week.0, week.1, 0)));
        assert!(!undo(&mut state, &mut trade, "@alice:x.org", day(week, 2)).unwrap());
        let text = early_text(&state, &EarlySwap { trade, ..swap });
        assert!(text.starts_with("↩️ Swapped back"), "{text}");
    }

    #[test]
    fn no_early_swap_when_the_running_shift_is_done_or_already_theirs() {
        let (mut state, group, week, ids) = split_week();
        // Alice holds the running shift herself: her ✅ marks that, no swap.
        assert!(swap_early(
            &mut state,
            &ids[0],
            "@alice:x.org",
            week,
            None,
            day(week, 1)
        )
        .unwrap()
        .is_none());
        // Bob on Thursday: his own shift is running, nothing earlier to take.
        assert!(
            swap_early(&mut state, &ids[1], "@bob:x.org", week, None, day(week, 3))
                .unwrap()
                .is_none()
        );
        // Alice already cleaned Mon–Wed.
        crate::commands::mark_duties_done(
            &mut state,
            &ids[0],
            &[Duty {
                group: group.clone(),
                slot_index: 0,
                turn: Turn::new(week.0, week.1, 0),
            }],
        )
        .unwrap();
        assert!(
            swap_early(&mut state, &ids[1], "@bob:x.org", week, None, day(week, 1))
                .unwrap()
                .is_none()
        );
        // Carol has nothing that week.
        assert!(swap_early(
            &mut state,
            &ids[2],
            "@carol:x.org",
            week,
            None,
            day(week, 1)
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn someone_steps_in_and_it_can_be_undone_until_it_starts() {
        let (mut state, group, week, _) = split_week();
        let monday = day(week, 0);
        // Bob can't do his Thu–Sun.
        let duties = askable(
            &state,
            &state.person_by_matrix_id("@bob:x.org").unwrap().id.clone(),
            week,
            monday,
        );
        assert_eq!(duties.len(), 1);
        let mut req = new_request(&duties[0], "@bob:x.org", None);
        let (text, pings) = request_text(&state, &req);
        assert!(text.starts_with("🆘 **Who can step in?** [bob]"), "{text}");
        assert!(text.contains("**2nd Floor · Thu–Sun ("), "{text}");
        assert!(text.contains("🙋 I'll do it · 🔄 swap"), "{text}");
        assert_eq!(pings, ["@alice:x.org", "@carol:x.org"]);

        assert_eq!(
            take(&mut state, &mut req, "@bob:x.org", false, monday).unwrap(),
            Taken::Own
        );
        assert_eq!(
            take(&mut state, &mut req, "@carol:x.org", false, monday).unwrap(),
            Taken::Done
        );
        assert_eq!(who(&state, &group, week, 1), "@carol:x.org");
        assert_eq!(req.status, HelpStatus::Taken);
        let (text, pings) = request_text(&state, &req);
        assert!(text.starts_with("✅ **Sorted!** [carol]"), "{text}");
        assert_eq!(pings, ["@bob:x.org"]);
        // Someone else can't take it any more.
        assert_eq!(
            take(&mut state, &mut req, "@alice:x.org", false, monday).unwrap(),
            Taken::NotOpen
        );
        // Not alice's to undo; carol can, before Thursday.
        assert_eq!(
            back(&mut state, &mut req, "@alice:x.org", monday).unwrap(),
            Back::Nothing
        );
        assert_eq!(
            back(&mut state, &mut req, "@carol:x.org", monday).unwrap(),
            Back::Undone
        );
        assert_eq!(who(&state, &group, week, 1), "@bob:x.org");
        assert_eq!(req.status, HelpStatus::Open);
        // Bob can make it after all.
        assert_eq!(
            back(&mut state, &mut req, "@bob:x.org", monday).unwrap(),
            Back::Withdrawn
        );
        assert!(request_text(&state, &req).0.starts_with("👍 [bob]"));
    }

    #[test]
    fn a_swap_gives_the_helpers_next_turn_back() {
        let (mut state, group, week, _) = split_week();
        let next = add_weeks(week.0, week.1, 1);
        let monday = day(week, 0);
        let bob = state.person_by_matrix_id("@bob:x.org").unwrap().id.clone();
        let mut req = new_request(&askable(&state, &bob, week, monday)[0], "@bob:x.org", None);
        // Carol swaps: she takes bob's Thu–Sun, bob takes her Mon–Wed next week.
        assert_eq!(
            take(&mut state, &mut req, "@carol:x.org", true, monday).unwrap(),
            Taken::Done
        );
        assert_eq!(who(&state, &group, week, 1), "@carol:x.org");
        assert_eq!(who(&state, &group, next, 0), "@bob:x.org");
        let (text, _) = request_text(&state, &req);
        assert!(text.starts_with("✅ **Swapped!** [carol]"), "{text}");
        assert!(
            text.contains("@bob:x.org does **2nd Floor · Mon–Wed ("),
            "{text}"
        );
        // Undone: both go back.
        assert_eq!(
            back(&mut state, &mut req, "@bob:x.org", monday).unwrap(),
            Back::Undone
        );
        assert_eq!(who(&state, &group, week, 1), "@bob:x.org");
        assert_eq!(who(&state, &group, next, 0), "@carol:x.org");
    }

    #[test]
    fn a_swap_needs_a_turn_to_give() {
        let (mut state, group, week, _) = split_week();
        state
            .apply_event(DomainEvent::PersonCreated {
                person_id: "dan".into(),
                display_name: "dan".into(),
                matrix_id: Some("@dan:x.org".into()),
            })
            .unwrap();
        let monday = day(week, 0);
        let bob = state.person_by_matrix_id("@bob:x.org").unwrap().id.clone();
        let mut req = new_request(&askable(&state, &bob, week, monday)[0], "@bob:x.org", None);
        // Dan isn't in the group: nothing to swap, but he can take it.
        assert_eq!(
            take(&mut state, &mut req, "@dan:x.org", true, monday).unwrap(),
            Taken::NothingToSwap
        );
        assert_eq!(req.status, HelpStatus::Open);
        assert_eq!(who(&state, &group, week, 1), "@bob:x.org");
        assert_eq!(
            take(&mut state, &mut req, "@dan:x.org", false, monday).unwrap(),
            Taken::Done
        );
        assert_eq!(who(&state, &group, week, 1), "@dan:x.org");
    }

    #[test]
    fn an_open_request_closes_once_settled_and_tells_the_admins_when_it_starts() {
        let (mut state, group, week, _) = split_week();
        let bob = state.person_by_matrix_id("@bob:x.org").unwrap().id.clone();
        let mut req = new_request(
            &askable(&state, &bob, week, day(week, 0))[0],
            "@bob:x.org",
            None,
        );
        assert!(!needs_admins(&state, &req, day(week, 2)));
        assert!(needs_admins(&state, &req, day(week, 3)));
        assert!(!settle(&state, &mut req, day(week, 6)));
        // Over.
        assert!(settle(&state, &mut req.clone(), day(week, 7)));
        // Done after all.
        crate::commands::mark_duties_done(
            &mut state,
            &bob,
            &[Duty {
                group: group.clone(),
                slot_index: 0,
                turn: Turn::new(week.0, week.1, 1),
            }],
        )
        .unwrap();
        assert!(settle(&state, &mut req, day(week, 4)));
        assert_eq!(req.status, HelpStatus::Closed);
        assert!(request_text(&state, &req).0.starts_with("✨ "));
    }

    #[test]
    fn a_swap_only_request_takes_no_plain_takeover() {
        let (mut state, group, week, _) = split_week();
        let monday = day(week, 0);
        let bob = state.person_by_matrix_id("@bob:x.org").unwrap().id.clone();
        let mut req = new_request(&askable(&state, &bob, week, monday)[0], "@bob:x.org", None);
        req.swap_only = true;
        let (text, pings) = request_text(&state, &req);
        assert!(text.starts_with("🔄 **Who swaps?** [bob]"), "{text}");
        assert!(!text.contains("🙋"), "{text}");
        assert_eq!(pings, ["@alice:x.org", "@carol:x.org"]);
        assert_eq!(
            take(&mut state, &mut req, "@carol:x.org", false, monday).unwrap(),
            Taken::SwapOnly
        );
        assert_eq!(who(&state, &group, week, 1), "@bob:x.org");
        assert_eq!(
            take(&mut state, &mut req, "@carol:x.org", true, monday).unwrap(),
            Taken::Done
        );
        assert_eq!(req.status, HelpStatus::Swapped);
    }

    #[test]
    fn reaction_keys_are_recognised_in_their_variants() {
        assert!(is_ask("🆘"));
        assert!(is_take("🙋") && is_take("🙋🏽") && is_take("🙋\u{200d}♀\u{fe0f}"));
        assert!(is_swap("🔄") && is_swap("🔄\u{fe0f}"));
        assert!(is_undo("↩️") && is_undo("↩"));
        assert!(!is_undo("✅") && !is_take("✅"));
    }
}
