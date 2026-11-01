//! Handing turns around without typing commands: two ways, both by
//! reaction in the cleaning room:
//!
//!   * **Cleaning early.** Someone ✅s (or `!done`s) while their own shift of
//!     the week is still to come and the running shift of the same slot is
//!     still open: they did the running one, so they swap. They get the
//!     running shift (done), its holder gets their later one. A message
//!     tells the one moved; ↩️ on it swaps back until the later shift
//!     starts, and taking the ✅ back undoes it all.
//!   * **🆘 Who steps in?** 🆘 on the plan or a reminder, or `!sos` for a
//!     later week: asks the room to cover one of your turns. 🙋 on the
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
use mxbot_common::reaction;

use crate::{
    analytics::DomainEvent,
    commands::Duty,
    domain::{AssignmentSource, CleaningGroup, GroupId, PersonId},
    format,
    rhythm::{date_range, Turn},
    state::{
        CardStatus, ConfirmCard, EarlySwap, HelpRequest, HelpStatus, MenuTurn, Reassignment, State,
        Trade,
    },
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

pub(crate) fn is_ask(key: &str) -> bool {
    reaction::is_one_of(key, &[ASK])
}

/// 🙋 in any skin tone, 🙋‍♀️ and 🙋‍♂️ too.
fn is_take(key: &str) -> bool {
    key.starts_with(TAKE)
}

pub(crate) fn is_swap(key: &str) -> bool {
    reaction::is_one_of(key, &[SWAP])
}

fn is_undo(key: &str) -> bool {
    reaction::is_one_of(key, &[UNDO])
}

/// ✅ on a "Take it over?" question: yes.
fn is_confirm(key: &str) -> bool {
    reaction::is_one_of(key, &["✅"])
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
        // Whoever gets a turn here knows: they did it, or the message about
        // it tells them (`notices` doesn't tell them again).
        state.mark_told(&c.group_id, c.slot_index, c.turn(), c.to.as_ref());
    }
    Ok(())
}

/// Whether `trade` can still be undone on `today`: everything it handed
/// over is still where it went, and none of it has started or been done,
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

/// Whether a turn taken over unasked can still go back: still the taker's,
/// not done, not over. (Started is fine: its holder may have planned the
/// weekend for it.)
pub fn keepable(state: &State, trade: &Trade, today: NaiveDate) -> bool {
    !trade.undone
        && trade.changes.iter().all(|c| {
            let Some(group) = state.group_by_id(&c.group_id) else {
                return false;
            };
            let turn = c.turn();
            holder(state, group, c.slot_index, turn) == c.to
                && turn.dates(&group.rhythm).1 >= today
                && !state.is_turn_slot_done(group, c.slot_index, turn)
        })
}

/// Hand everything back, if `undoable`. Returns whether it did.
fn undo(state: &mut State, trade: &mut Trade, actor: &str, today: NaiveDate) -> Result<bool> {
    if !undoable(state, trade, today) {
        return Ok(false);
    }
    hand_back(state, trade, actor)
}

/// Hand everything back, whether or not `undoable`. Returns `true`.
fn hand_back(state: &mut State, trade: &mut Trade, actor: &str) -> Result<bool> {
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
    // Who took their turn back knows; anyone else gets a change notice.
    let actor_id = state.person_by_matrix_id(actor).map(|p| p.id.clone());
    for c in &trade.changes {
        if actor_id.is_some() && c.from == actor_id {
            state.mark_told(&c.group_id, c.slot_index, c.turn(), c.from.as_ref());
        }
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
/// if the running shift of a slot they hold later is still open, swap.
/// They take it, marked done; its holder takes their later shift. `None`
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

/// "Thu–Sun (8 – 11 Oct)". A shift of a group split into shifts, else the
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
/// 🔄 @bob, alice already cleaned **2nd Floor / Scharni · Mon–Wed** for you, so you two swapped: you're on **Thu–Sun (8 – 11 Oct)** now.
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
                "🔄 {moved}, {cleaner} cleaned **{}** for you.\nYou do **{}** instead.",
                done.label(),
                when(&theirs)
            )
        });
    }
    if !swap.trade.undone {
        lines.push("↩️ swaps back".to_owned());
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
        unasked: false,
        given_back: false,
        asked_at: Some(chrono::Utc::now()),
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

/// `mxid`'s name, or the ID itself.
fn display_name(state: &State, mxid: &str) -> String {
    state
        .person_by_matrix_id(mxid)
        .map(|p| p.display_name.clone())
        .unwrap_or_else(|| mxid.to_owned())
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
/// 🆘 **Who takes over?** alice can't make it:
/// **2nd Floor / Scharni · Thu–Sun (8 – 11 Oct)**
/// 🙋 I'll do it
/// 🔄 swap: you take it, alice takes your next one
/// alice: ↩️ if you can after all
/// @bob @carl
/// ```
pub fn request_text(state: &State, req: &HelpRequest) -> (String, Vec<String>) {
    let Some(duty) = request_duty(state, req) else {
        return ("⌛ Closed. That group is gone.".to_owned(), Vec::new());
    };
    let what = format!("**{} · {}**", duty.place(), when(&duty));
    let asker = requester_link(state, req);
    match req.status {
        HelpStatus::Open => {
            let pings = request_pings(state, req);
            let name = display_name(state, &req.requester);
            let mut lines = if req.swap_only {
                vec![
                    format!("🔄 **Who swaps?** {asker} would like to swap:"),
                    what,
                    format!("🔄 you do this, {name} does your next one"),
                    format!("{name}: ↩️ to cancel"),
                ]
            } else {
                vec![
                    format!("🆘 **Who takes over?** {asker} can't make it:"),
                    what,
                    "🙋 I'll do it".to_owned(),
                    format!("🔄 swap: you do this, {name} does your next one"),
                    format!("{name}: ↩️ to cancel"),
                ]
            };
            if !pings.is_empty() {
                lines.push(pings.join(" "));
            }
            (lines.join("\n"), pings)
        }
        HelpStatus::Taken if req.unasked => {
            let name = display_name(state, &req.requester);
            let mut text = format!(
                "🙋 {} takes over {what} from {asker}. Thanks! 💛",
                helper_link(state, req),
            );
            if req
                .trade
                .as_ref()
                .is_some_and(|t| keepable(state, t, crate::state::today()))
            {
                text.push_str(&format!("\n{name}: ↩️ to keep it"));
            }
            (text, vec![req.requester.clone()])
        }
        HelpStatus::Withdrawn if req.unasked && req.given_back => (
            format!(
                "↩️ {} gives {what} back to {asker}.",
                helper_link(state, req)
            ),
            vec![req.requester.clone()],
        ),
        HelpStatus::Withdrawn if req.unasked => (
            format!(
                "↩️ {asker} keeps {what}. Thanks anyway, {}! 💛",
                helper_link(state, req)
            ),
            req.helper.iter().cloned().collect(),
        ),
        HelpStatus::Taken => (
            format!(
                "✅ **Sorted!** {} does {what} for {asker}. Thanks! 💛\n↩️ to undo, until it starts",
                helper_link(state, req),
            ),
            vec![req.requester.clone()],
        ),
        HelpStatus::Swapped => {
            let mut lines = vec![
                "✅ **Swapped!** Thanks! 💛".to_owned(),
                format!("➡️ {} does {what}", helper_link(state, req)),
            ];
            if let Some(d) = req
                .trade
                .as_ref()
                .and_then(|t| t.changes.get(1))
                .and_then(|c| duty_of(state, c))
            {
                lines.push(format!("⬅️ {asker} does **{} · {}**", d.place(), when(&d)));
            }
            lines.push("↩️ to undo, until it starts".to_owned());
            (lines.join("\n"), vec![req.requester.clone()])
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
                    format!("✨ {what} got done. Closed.")
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

/// `helper` (Matrix ID) steps in on `req`, with `swap`, giving their next
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
            let undone = if req.unasked {
                keepable(state, &trade, today) && hand_back(state, &mut trade, who)?
            } else {
                undo(state, &mut trade, who, today)?
            };
            if !undone {
                return Ok(Back::TooLate);
            }
            // Both know: who tapped, and the other one by the reply
            // (`on_request`) or the edit.
            for c in &trade.changes {
                state.mark_told(&c.group_id, c.slot_index, c.turn(), c.from.as_ref());
            }
            if req.unasked {
                // Nobody asked: undone, the turn is back where it was. Given
                // back by the one who took it, the edit tells its holder.
                req.given_back = req.helper.as_deref() == Some(who);
                if req.given_back {
                    for c in &trade.changes {
                        state.mark_told(&c.group_id, c.slot_index, c.turn(), c.from.as_ref());
                    }
                }
                req.status = HelpStatus::Withdrawn;
                req.trade = Some(trade);
                return Ok(Back::Undone);
            }
            if who == req.requester {
                // The one who asked does it after all.
                req.status = HelpStatus::Withdrawn;
                req.trade = Some(trade);
                return Ok(Back::Undone);
            }
            // The helper can't after all: open again.
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

/// How long the group has to step in before the admins hear of a request
/// whose turn has started.
const ADMIN_GRACE_HOURS: i64 = 4;

/// An open request whose turn has started, asked a while ago (`now`): time
/// to tell the admins.
fn needs_admins(
    state: &State,
    req: &HelpRequest,
    today: NaiveDate,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    req.status == HelpStatus::Open
        && !req.admins_told
        && req
            .asked_at
            .is_none_or(|at| now - at >= chrono::Duration::hours(ADMIN_GRACE_HOURS))
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
    let requests: HashSet<String> = state.help_requests.keys().cloned().collect();
    let cards = state.confirm_cards.len();
    state
        .confirm_cards
        .retain(|_, c| requests.contains(&c.request_id));
    state.help_requests.len() + state.early_swaps.len() != before
        || state.confirm_cards.len() != cards
}

// ── "Take it over? ✅" ────────────────────────────────────────────────────────

/// What `take` would say if `helper` stepped in on `req` now (🔄 with
/// `swap`). Nothing changes.
pub fn could_take(
    state: &State,
    req: &HelpRequest,
    helper: &str,
    swap: bool,
    today: NaiveDate,
) -> Taken {
    let mut probe = req.clone();
    if settle(state, &mut probe, today) || probe.status != HelpStatus::Open {
        return Taken::NotOpen;
    }
    if helper == req.requester {
        return Taken::Own;
    }
    if req.swap_only && !swap {
        return Taken::SwapOnly;
    }
    if swap && swap_give(state, req, helper, today).is_none() {
        return Taken::NothingToSwap;
    }
    Taken::Done
}

fn menu_turn(d: &Duty) -> MenuTurn {
    MenuTurn {
        group_id: d.group.id.clone(),
        slot_index: d.slot_index,
        iso_year: d.turn.year,
        iso_week: d.turn.week,
        shift: d.turn.shift,
    }
}

/// The turn of `helper` a 🔄 on `req` would hand the one asking.
fn swap_give(state: &State, req: &HelpRequest, helper: &str, today: NaiveDate) -> Option<Duty> {
    let duty = request_duty(state, req)?;
    let helper_id = state.person_by_matrix_id(helper)?.id.clone();
    let requester_id = state.person_by_matrix_id(&req.requester)?.id.clone();
    next_turn_to_give(state, &helper_id, &requester_id, &duty, today)
}

/// A question as it reads now.
///
/// ```text
/// 🙋 bob, take over **Kitchen · 13 – 19 Oct** from alice?
/// Tap ✅ to confirm.
/// ```
pub fn card_text(state: &State, card: &ConfirmCard, today: NaiveDate) -> String {
    let Some(req) = state.help_requests.get(&card.request_id) else {
        return "⌛ Not open any more.".to_owned();
    };
    let Some(duty) = request_duty(state, req) else {
        return "⌛ Not open any more.".to_owned();
    };
    let you = match state.person_by_matrix_id(&card.user_id) {
        Some(p) => view::user_link(p),
        None => view::user_id_link(&card.user_id),
    };
    let theirs = req.helper.as_deref() == Some(card.user_id.as_str())
        && matches!(req.status, HelpStatus::Taken | HelpStatus::Swapped);
    if card.status == CardStatus::Done || theirs {
        return format!("✅ Done. Thanks, {you}! 💛");
    }
    let what = format!("**{} · {}**", duty.place(), when(&duty));
    let name = display_name(state, &req.requester);
    let note = card
        .note
        .as_ref()
        .map(|n| format!("\n{n}"))
        .unwrap_or_default();
    let text = match could_take(state, req, &card.user_id, card.swap, today) {
        Taken::Done if card.swap => match swap_give(state, req, &card.user_id, today) {
            Some(give) => format!(
                "🔄 {you}, swap with {name}?\n➡️ You do {what}\n⬅️ {name} does your **{} · {}**\nTap ✅ to swap.",
                give.place(),
                when(&give)
            ),
            None => "⌛ Not open any more.".to_owned(),
        },
        Taken::Done => format!("🙋 {you}, take over {what} from {name}?\nTap ✅ to confirm."),
        Taken::NothingToSwap => {
            format!("🤷 {you}, you have no turn of that group left to swap.")
        }
        _ => {
            return match req.helper.as_deref() {
                Some(helper) if matches!(req.status, HelpStatus::Taken | HelpStatus::Swapped) => {
                    format!(
                        "⌛ {} was quicker. Thanks anyway!",
                        display_name(state, helper)
                    )
                }
                _ => "⌛ Not open any more.".to_owned(),
            }
        }
    };
    format!("{text}{note}")
}

// ── 🙋 Taking a turn over, nobody asked ──────────────────────────────────────

/// What taking a turn over did.
#[derive(Debug, PartialEq)]
pub enum TakeOver {
    /// Theirs now, from someone who is told by `notice` (a request record,
    /// `unasked`, to post: its ↩️ lets them keep it).
    Notice(Box<HelpRequest>),
    /// Theirs now; it had a 🆘 request (this event ID), now sorted.
    Sorted(String),
    /// Theirs now; nobody held it.
    Quiet,
    /// Someone else holds it than when they picked it: nothing changed.
    Changed,
    /// Done already: nothing changed.
    Done,
    /// Over: nothing changed.
    Over,
    /// Theirs already.
    Own,
}

/// `taker` (Matrix ID) takes `duty` over, nobody asked, as long as
/// `expected` still holds it (they saw it that way when they picked it).
pub fn take_over(
    state: &mut State,
    duty: &Duty,
    taker: &str,
    expected: Option<&PersonId>,
    today: NaiveDate,
) -> Result<TakeOver> {
    if duty.turn.dates(&duty.group.rhythm).1 < today {
        return Ok(TakeOver::Over);
    }
    if state.is_turn_slot_done(&duty.group, duty.slot_index, duty.turn) {
        return Ok(TakeOver::Done);
    }
    let current = holder(state, &duty.group, duty.slot_index, duty.turn);
    let taker_id = state.person_by_matrix_id(taker).map(|p| p.id.clone());
    if current.is_some() && current == taker_id {
        return Ok(TakeOver::Own);
    }
    if current.as_ref() != expected {
        return Ok(TakeOver::Changed);
    }
    // Asked for cover already: that request is the one that gets sorted.
    if let Some((id, req)) = open_request(state, duty)
        .filter(|(_, r)| !r.swap_only)
        .map(|(id, r)| (id.clone(), r.clone()))
    {
        let mut req = req;
        if take(state, &mut req, taker, false, today)? == Taken::Done {
            state.help_requests.insert(id.clone(), req);
            return Ok(TakeOver::Sorted(id));
        }
    }
    state.apply_event(DomainEvent::PersonCreated {
        person_id: uuid::Uuid::new_v4().to_string(),
        display_name: taker.into(),
        matrix_id: Some(taker.into()),
    })?;
    let taker_id = state
        .person_by_matrix_id(taker)
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow::anyhow!("{taker} has no person record"))?;
    let from_mxid = current
        .as_ref()
        .and_then(|id| state.person_by_id(id))
        .and_then(|p| p.matrix_id.clone());
    let mut trade = Trade::default();
    trade.changes.push(reassignment(
        state,
        &duty.group,
        duty.slot_index,
        duty.turn,
        &taker_id,
        false,
    ));
    apply(state, &trade, AssignmentSource::Takeover, taker)?;
    let Some(from) = from_mxid else {
        return Ok(TakeOver::Quiet);
    };
    let mut notice = new_request(duty, &from, None);
    notice.unasked = true;
    notice.status = HelpStatus::Taken;
    notice.helper = Some(taker.to_owned());
    notice.trade = Some(trade);
    Ok(TakeOver::Notice(Box::new(notice)))
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
    match mxbot_common::send::send_with_retry(room, edit).await {
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
    if let Err(e) = mxbot_common::send::send_with_retry(room, content).await {
        tracing::warn!("Failed to answer a swap reaction: {e}");
    }
}

async fn seed(room: &Room, event_id: &OwnedEventId, keys: &[&str]) {
    for key in keys {
        let reaction = ReactionEventContent::new(Annotation::new(event_id.clone(), (*key).into()));
        if let Err(e) = mxbot_common::send::send_with_retry(room, reaction).await {
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
    let event_id =
        match mxbot_common::send::send_with_retry(room, content(ctx, room, &text, &notify).await)
            .await
        {
            Ok(event_id) => event_id,
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
    let event_id =
        match mxbot_common::send::send_with_retry(room, content(ctx, room, &text, &notify).await)
            .await
        {
            Ok(event_id) => event_id,
            Err(e) => {
                tracing::warn!("Failed to post a 🆘 request: {e}");
                return None;
            }
        };
    let swap_only = req.swap_only;
    let unasked = req.unasked;
    req.rendered = text;
    req.mentioned = notify;
    {
        let mut state = ctx.state.lock().await;
        state.help_requests.insert(event_id.to_string(), req);
        save(ctx, &mut state).await;
    }
    let buttons: &[&str] = if unasked {
        &[UNDO]
    } else if swap_only {
        &[SWAP, UNDO]
    } else {
        &[TAKE, SWAP, UNDO]
    };
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
        let on_request = state.help_requests.contains_key(reacted_to);
        let plan_week = state.weekly_plan_event_ids.get(reacted_to).copied();
        if state.confirm_cards.contains_key(reacted_to) && is_confirm(key) {
            Some(Tap::Confirm)
        } else if on_request && is_undo(key) {
            Some(Tap::Request)
        } else if on_request && (is_take(key) || is_swap(key)) {
            Some(Tap::Offer(is_swap(key)))
        } else if state.early_swaps.contains_key(reacted_to) && is_undo(key) {
            Some(Tap::EarlyUndo)
        } else if is_ask(key) {
            match plan_week {
                Some(week) => Some(Tap::Ask(week, true, false)),
                None => state
                    .reminder_messages
                    .get(reacted_to)
                    .map(|r| Tap::Ask((r.iso_year, r.iso_week), false, false)),
            }
        } else if is_swap(key) && plan_week.is_some() {
            plan_week.map(|week| Tap::Ask(week, true, true))
        } else if is_take(key) && plan_week.is_some() {
            Some(Tap::TakeMenu)
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
        Tap::Ask(week, on_plan, swap) => {
            let asking = Asking {
                week,
                on_plan,
                swap,
            };
            ask(ctx, room, reaction_id, sender, reacted_to, asking, today).await;
            if swap {
                // 🔄 is a button; 🆘 stays, taking it back withdraws.
                consume(room, reaction_id).await;
            }
        }
        Tap::Request => on_request(ctx, room, sender, reacted_to, today).await,
        Tap::Offer(swap) => {
            offer(ctx, room, sender, reacted_to, swap, today).await;
            consume(room, reaction_id).await;
        }
        Tap::Confirm => {
            confirm(ctx, room, sender, reacted_to, today).await;
            consume(room, reaction_id).await;
        }
        Tap::TakeMenu => {
            take_menu(ctx, room, sender, reacted_to).await;
            consume(room, reaction_id).await;
        }
        Tap::EarlyUndo => undo_early(ctx, room, sender.as_str(), reacted_to, None, today).await,
    }
    true
}

/// What a 🆘 or 🔄 on the plan or a reminder asks for.
struct Asking {
    week: (i32, u32),
    /// On the plan: its week and the next.
    on_plan: bool,
    /// 🔄: a swap only.
    swap: bool,
}

enum Tap {
    /// 🆘 on the plan (`true`) or a reminder of that week; 🔄 on the plan
    /// (`true`, `true`).
    Ask((i32, u32), bool, bool),
    /// ↩️ on a request.
    Request,
    /// 🙋 or 🔄 (`true`) on a request: ask them to confirm.
    Offer(bool),
    /// ✅ on a "Take it over?" question.
    Confirm,
    /// 🙋 on the plan: the menu of turns to take over.
    TakeMenu,
    EarlyUndo,
}

/// Take a button press back, so the button can be pressed again.
async fn consume(room: &Room, reaction_id: &EventId) {
    if let Some(bot) = room.client().user_id().map(ToOwned::to_owned) {
        crate::onboarding::consume_tap(room, &reaction_id.to_owned(), &bot).await;
    }
}

/// 🙋 or 🔄 (`swap`) on request `request_id`: ask the sender to confirm
/// with a ✅, or say why not.
async fn offer(
    ctx: &BotContext,
    room: &Room,
    sender: &UserId,
    request_id: &str,
    swap: bool,
    today: NaiveDate,
) {
    let who = sender.as_str();
    let (check, swap_only, asked_already) = {
        let state = ctx.state.lock().await;
        let Some(req) = state.help_requests.get(request_id) else {
            return;
        };
        let asked_already = state.confirm_cards.values().any(|c| {
            c.user_id == who
                && c.request_id == request_id
                && c.swap == swap
                && c.status == CardStatus::Asking
        });
        (
            could_take(&state, req, who, swap, today),
            req.swap_only,
            asked_already,
        )
    };
    let answer = match check {
        Taken::Done if asked_already => return,
        Taken::Done => None,
        Taken::Own => Some("🙃 That's your own request. ↩️ withdraws it."),
        Taken::NothingToSwap if swap_only => {
            Some("🤷 You have no upcoming turn in that group to swap.")
        }
        Taken::NothingToSwap => {
            Some("🤷 You have no upcoming turn in that group to swap. 🙋 takes it over instead.")
        }
        Taken::SwapOnly => {
            Some("🔄 This one is a swap: 🔄 takes it and gives them your next turn.")
        }
        Taken::NotOpen => Some("⌛ That one is sorted already."),
    };
    if let Some(text) = answer {
        reply(room, request_id, who, text).await;
        return;
    }
    let mut card = ConfirmCard {
        user_id: who.to_owned(),
        request_id: request_id.to_owned(),
        swap,
        give: None,
        note: None,
        status: CardStatus::Asking,
        rendered: String::new(),
    };
    {
        let state = ctx.state.lock().await;
        if swap {
            card.give = state
                .help_requests
                .get(request_id)
                .and_then(|req| swap_give(&state, req, who, today))
                .map(|d| menu_turn(&d));
        }
        card.rendered = card_text(&state, &card, today);
    }
    let Ok(request_event) = request_id.parse::<OwnedEventId>() else {
        return;
    };
    let mut message = content(ctx, room, &card.rendered, &[]).await;
    message.relates_to = Some(Relation::Reply(Reply::with_event_id(request_event)));
    let event_id = match mxbot_common::send::send_with_retry(room, message).await {
        Ok(event_id) => event_id,
        Err(e) => {
            tracing::warn!("Failed to ask to confirm a takeover: {e}");
            return;
        }
    };
    {
        let mut state = ctx.state.lock().await;
        state.confirm_cards.insert(event_id.to_string(), card);
        if !save(ctx, &mut state).await {
            return;
        }
    }
    seed(room, &event_id, &["✅"]).await;
}

/// ✅ on question `card_id`: by the one asked, it goes through (if it still
/// can; the question then says why not). Anyone else's ✅ does nothing.
async fn confirm(ctx: &BotContext, room: &Room, sender: &UserId, card_id: &str, today: NaiveDate) {
    let request_id = {
        let mut state = ctx.state.lock().await;
        let Some(mut card) = state.confirm_cards.get(card_id).cloned() else {
            return;
        };
        if card.user_id != sender.as_str() || card.status != CardStatus::Asking {
            return;
        }
        let Some(mut req) = state.help_requests.get(&card.request_id).cloned() else {
            return;
        };
        // A swap goes through only with the turn it showed.
        let give = card
            .swap
            .then(|| swap_give(&state, &req, sender.as_str(), today))
            .flatten()
            .map(|d| menu_turn(&d));
        if card.swap && give.is_some() && give != card.give {
            card.give = give;
            card.note = Some("🔀 That changed, see above. ✅ again if it suits you.".to_owned());
            let request_id = card.request_id.clone();
            state.confirm_cards.insert(card_id.to_owned(), card);
            if save(ctx, &mut state).await {
                drop(state);
                update_cards(ctx, room, Some(&request_id)).await;
            }
            return;
        }
        card.note = None;
        match take(&mut state, &mut req, sender.as_str(), card.swap, today) {
            Ok(Taken::Done) => {
                card.status = CardStatus::Done;
                state.help_requests.insert(card.request_id.clone(), req);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!("A confirmed takeover failed: {e}");
                return;
            }
        }
        let request_id = card.request_id.clone();
        state.confirm_cards.insert(card_id.to_owned(), card);
        if !save(ctx, &mut state).await {
            return;
        }
        request_id
    };
    update_request(ctx, room, &request_id).await;
    refresh_plan(ctx, room).await;
}

/// 🙋 on the plan (`plan_id`): the sender's menu of turns to take over, in
/// their private chat if they have one, else under the plan.
async fn take_menu(ctx: &BotContext, room: &Room, sender: &UserId, plan_id: &str) {
    let private = crate::help_board::private_chat(ctx, &room.client(), &sender.to_owned()).await;
    let target = private.as_ref().unwrap_or(room);
    let relation = match (&private, plan_id.parse::<OwnedEventId>()) {
        (None, Ok(plan)) => Some(Relation::Reply(Reply::with_event_id(plan))),
        _ => None,
    };
    match crate::turn_menu::post_take(ctx, target, sender, relation).await {
        Ok(true) => {}
        Ok(false) => {
            reply(
                room,
                plan_id,
                sender.as_str(),
                crate::turn_menu::NOTHING_TO_TAKE,
            )
            .await
        }
        Err(e) => tracing::warn!("Failed to post a take menu: {e}"),
    }
}

/// Bring the questions about `request_id` (all, without one) up to date.
pub(crate) async fn update_cards(ctx: &BotContext, room: &Room, request_id: Option<&str>) {
    let today = crate::state::today();
    let stale: Vec<(String, String)> = {
        let state = ctx.state.lock().await;
        state
            .confirm_cards
            .iter()
            .filter(|(_, c)| request_id.is_none_or(|r| c.request_id == r))
            .map(|(id, c)| (id.clone(), card_text(&state, c, today), c))
            .filter(|(_, text, c)| text != &c.rendered)
            .map(|(id, text, _)| (id, text))
            .collect()
    };
    for (id, text) in stale {
        if edit(ctx, room, &id, &text, &[], &[]).await {
            let mut state = ctx.state.lock().await;
            if let Some(card) = state.confirm_cards.get_mut(&id) {
                card.rendered = text;
            }
            save(ctx, &mut state).await;
        }
    }
}

/// 🆘 (or 🔄) on the plan (its week and the next) or a reminder of its
/// week: a request for the sender's open turn. Several on the plan: their
/// menu to pick one; several on a reminder: a request each.
async fn ask(
    ctx: &BotContext,
    room: &Room,
    reaction_id: &EventId,
    sender: &UserId,
    reacted_to: &str,
    asking: Asking,
    today: NaiveDate,
) {
    let Asking {
        week,
        on_plan,
        swap,
    } = asking;
    let (fresh, asked) = {
        let state = ctx.state.lock().await;
        let mut weeks = vec![week];
        if on_plan {
            weeks.push(crate::state::add_weeks(week.0, week.1, 1));
        }
        let duties: Vec<Duty> = state
            .person_by_matrix_id(sender.as_str())
            .map(|p| {
                weeks
                    .iter()
                    .flat_map(|w| askable(&state, &p.id, *w, today))
                    .collect()
            })
            .unwrap_or_default();
        let asked = duties.len();
        let fresh: Vec<HelpRequest> = duties
            .iter()
            .filter(|d| open_request(&state, d).is_none())
            .map(|d| {
                // A 🔄 is consumed: its reaction can't withdraw.
                let trigger = (!swap).then(|| reaction_id.to_string());
                let mut req = new_request(d, sender.as_str(), trigger);
                req.swap_only = swap;
                req
            })
            .collect();
        (fresh, asked)
    };
    if asked == 0 {
        let text = if on_plan {
            "🤷 Nothing open of yours this week or next. Later turns: 📅 !next"
        } else {
            "🤷 Nothing open of yours this week."
        };
        reply(room, reacted_to, sender.as_str(), text).await;
        return;
    }
    if fresh.is_empty() {
        reply(
            room,
            reacted_to,
            sender.as_str(),
            "☝️ You already asked, see your 🆘 above.",
        )
        .await;
        return;
    }
    if on_plan && fresh.len() > 1 {
        let menu = {
            let state = ctx.state.lock().await;
            crate::turn_menu::new_menu(
                &state,
                sender.as_str(),
                room.room_id().as_str(),
                crate::turn_menu::DEFAULT,
            )
        };
        if let (Some(mut menu), Ok(plan)) = (menu, reacted_to.parse::<OwnedEventId>()) {
            menu.feedback = Some(if swap {
                "🤔 Which one? Tap its number, then 🔄.".to_owned()
            } else {
                "🤔 Which one? Tap its number, then 🆘.".to_owned()
            });
            let relation = Some(Relation::Reply(Reply::with_event_id(plan)));
            if let Err(e) = crate::turn_menu::post_menu(ctx, room, menu, relation).await {
                tracing::warn!("Failed to post a turn menu for a 🆘: {e}");
            }
            return;
        }
    }
    for req in fresh {
        post_request(ctx, room, req).await;
    }
}

/// ↩️ on a request: withdraw it (the one asking), or undo the handover
/// (either of the two), or keep a turn taken over unasked (its holder).
async fn on_request(
    ctx: &BotContext,
    room: &Room,
    sender: &UserId,
    request_id: &str,
    today: NaiveDate,
) {
    let who = sender.as_str();
    let mut state = ctx.state.lock().await;
    let Some(mut req) = state.help_requests.get(request_id).cloned() else {
        return;
    };
    let before = req.clone();
    let too_late = match back(&mut state, &mut req, who, today) {
        Ok(outcome) => outcome == Back::TooLate,
        Err(e) => {
            tracing::error!("A swap tap failed: {e}");
            return;
        }
    };
    let changed = req != before;
    // Who an undo leaves holding or losing the turn without having done it:
    // the edit wouldn't notify them (they were in the first message), so a
    // reply does.
    let what = request_duty(&state, &req).map(|d| format!("**{} · {}**", d.place(), when(&d)));
    let handed_over = matches!(before.status, HelpStatus::Taken | HelpStatus::Swapped);
    let tell = match (&what, changed && handed_over, req.status) {
        (Some(what), true, HelpStatus::Withdrawn) if req.unasked && req.given_back => Some((
            req.requester.clone(),
            format!("↩️ it's yours again: {what}"),
        )),
        (Some(_), true, HelpStatus::Withdrawn) if req.unasked => None,
        (Some(what), true, HelpStatus::Withdrawn) => before.helper.clone().map(|helper| {
            (
                helper,
                format!(
                    "↩️ {} does {what} after all. Thanks anyway! 💛",
                    display_name(&state, &req.requester)
                ),
            )
        }),
        (Some(what), true, HelpStatus::Open) => Some((
            req.requester.clone(),
            format!(
                "↩️ {} can't after all: {what} is yours again. Your 🆘 is open.",
                before
                    .helper
                    .as_deref()
                    .map(|h| display_name(&state, h))
                    .unwrap_or_default()
            ),
        )),
        _ => None,
    };
    if changed {
        state.help_requests.insert(request_id.to_owned(), req);
        if !save(ctx, &mut state).await {
            return;
        }
    }
    drop(state);
    if let Some((user, text)) = tell {
        reply(room, request_id, &user, &text).await;
    }
    if too_late {
        reply(room, request_id, who, "⌛ Too late, it has started.").await;
    }
    if changed {
        update_request(ctx, room, request_id).await;
        refresh_plan(ctx, room).await;
    }
}

/// Bring a request's message up to date, and the questions about it.
pub(crate) async fn update_request(ctx: &BotContext, room: &Room, request_id: &str) {
    let (text, notify, before) = {
        let state = ctx.state.lock().await;
        let Some(req) = state.help_requests.get(request_id) else {
            return;
        };
        let (text, notify) = request_text(&state, req);
        if text == req.rendered {
            drop(state);
            update_cards(ctx, room, Some(request_id)).await;
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
    update_cards(ctx, room, Some(request_id)).await;
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
                reply(room, notice_id, who, "⌛ Too late, it has started.").await;
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
            if nudge && needs_admins(&state, req, today, chrono::Utc::now()) {
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
    let mut admins: Vec<String> = ctx
        .admin_users
        .current()
        .iter()
        .map(|u| u.to_string())
        .collect();
    admins.sort();
    if nudges.is_empty() || admins.is_empty() {
        return;
    }
    for (id, label) in nudges {
        let Ok(event_id) = id.parse::<OwnedEventId>() else {
            continue;
        };
        let text = format!(
            "👀 Still nobody for **{}**.\n{}, can you sort it? (!plan assign)",
            label.unwrap_or_default(),
            admins.join(" ")
        );
        let mut message = content(ctx, room, &text, &admins).await;
        message.relates_to = Some(Relation::Reply(Reply::with_event_id(event_id)));
        if let Err(e) = mxbot_common::send::send_with_retry(room, message).await {
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
        assert!(text.starts_with("🔄 @alice:x.org, [bob]"), "{text}");
        assert!(text.contains("**2nd Floor · Mon–Wed**"), "{text}");
        assert!(text.contains("\nYou do **Thu–Sun ("), "{text}");
        assert!(text.contains("↩️"), "{text}");

        // Once Thursday came, it stays.
        assert!(!undoable(&state, &trade, day(week, 3)));
        // Before, it goes back. The cleaning stays recorded.
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
        assert!(text.starts_with("🆘 **Who takes over?** [bob]"), "{text}");
        assert!(text.contains("**2nd Floor · Thu–Sun ("), "{text}");
        assert!(text.contains("🙋 I'll do it\n🔄 swap"), "{text}");
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
        // Carol can't after all: bob's request is open again.
        assert_eq!(req.status, HelpStatus::Open);
        assert_eq!(req.helper, None);
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
        assert!(
            text.starts_with("✅ **Swapped!** Thanks! 💛\n➡️ [carol]"),
            "{text}"
        );
        assert!(text.contains("\n⬅️ [bob]"), "{text}");
        assert!(text.contains(" does **2nd Floor · Mon–Wed ("), "{text}");
        // Undone by bob, who asked: both go back, and he keeps his (no new request).
        assert_eq!(
            back(&mut state, &mut req, "@bob:x.org", monday).unwrap(),
            Back::Undone
        );
        assert_eq!(who(&state, &group, week, 1), "@bob:x.org");
        assert_eq!(who(&state, &group, next, 0), "@carol:x.org");
        assert_eq!(req.status, HelpStatus::Withdrawn);
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
        let asked = req.asked_at.unwrap();
        let later = asked + chrono::Duration::hours(ADMIN_GRACE_HOURS);
        assert!(!needs_admins(&state, &req, day(week, 2), later));
        // Started: the group gets a few hours first.
        assert!(!needs_admins(&state, &req, day(week, 3), asked));
        assert!(needs_admins(&state, &req, day(week, 3), later));
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
        assert!(is_ask("🆘") && is_ask("🆘\u{fe0f}"));
        assert!(is_take("🙋") && is_take("🙋🏽") && is_take("🙋\u{200d}♀\u{fe0f}"));
        assert!(is_swap("🔄") && is_swap("🔄\u{fe0f}"));
        assert!(is_undo("↩️") && is_undo("↩"));
        assert!(!is_undo("✅") && !is_take("✅"));
    }

    fn duty(group: &CleaningGroup, week: (i32, u32), shift: u8) -> Duty {
        Duty {
            group: group.clone(),
            slot_index: 0,
            turn: Turn::new(week.0, week.1, shift),
        }
    }

    #[test]
    fn a_turn_is_taken_over_without_asking_and_its_holder_can_keep_it() {
        let (mut state, group, week, ids) = split_week();
        let monday = day(week, 0);
        // Carol takes bob's Thu–Sun, as she saw it: bob's.
        let thu = duty(&group, week, 1);
        let TakeOver::Notice(notice) =
            take_over(&mut state, &thu, "@carol:x.org", Some(&ids[1]), monday).unwrap()
        else {
            panic!("bob is told");
        };
        let mut notice = *notice;
        assert_eq!(who(&state, &group, week, 1), "@carol:x.org");
        assert!(notice.unasked);
        assert_eq!(notice.requester, "@bob:x.org");
        let (text, pings) = request_text(&state, &notice);
        assert!(
            text.starts_with(
                "🙋 [carol](https://matrix.to/#/@carol:x.org) takes over **2nd Floor · Thu–Sun ("
            ),
            "{text}"
        );
        assert!(text.contains("from [bob]"), "{text}");
        assert!(text.ends_with("\nbob: ↩️ to keep it"), "{text}");
        assert!(!text.contains('—'), "{text}");
        assert_eq!(pings, ["@bob:x.org"]);
        // Carol knows, she did it: no change notice for her.
        assert_eq!(
            state
                .told_holders
                .get(&crate::state::slot_key(&group.id, 0, thu.turn)),
            Some(&Some(ids[2].clone()))
        );

        // Someone else's ↩️ does nothing; bob's keeps it.
        assert_eq!(
            back(&mut state, &mut notice, "@alice:x.org", monday).unwrap(),
            Back::Nothing
        );
        assert_eq!(
            back(&mut state, &mut notice, "@bob:x.org", monday).unwrap(),
            Back::Undone
        );
        assert_eq!(who(&state, &group, week, 1), "@bob:x.org");
        assert_eq!(notice.status, HelpStatus::Withdrawn);
        let (text, pings) = request_text(&state, &notice);
        assert!(text.starts_with("↩️ [bob]"), "{text}");
        assert!(text.contains("keeps **2nd Floor · Thu–Sun ("), "{text}");
        assert_eq!(pings, ["@carol:x.org"]);
        // Undone is undone: no second ↩️.
        assert_eq!(
            back(&mut state, &mut notice, "@bob:x.org", monday).unwrap(),
            Back::Nothing
        );
    }

    #[test]
    fn a_running_turn_taken_over_can_be_kept_until_done_or_over() {
        let (mut state, group, week, ids) = split_week();
        let friday = day(week, 4);
        let thu = duty(&group, week, 1);
        // Friday, bob's Thu–Sun is running; carol takes it.
        let TakeOver::Notice(notice) =
            take_over(&mut state, &thu, "@carol:x.org", Some(&ids[1]), friday).unwrap()
        else {
            panic!("bob is told");
        };
        let mut kept = *notice;
        // Bob wanted to clean on Saturday: he keeps it.
        assert_eq!(
            back(&mut state, &mut kept, "@bob:x.org", day(week, 5)).unwrap(),
            Back::Undone
        );
        assert_eq!(who(&state, &group, week, 1), "@bob:x.org");

        // Taken again and cleaned: then it stays.
        let TakeOver::Notice(notice) =
            take_over(&mut state, &thu, "@carol:x.org", Some(&ids[1]), friday).unwrap()
        else {
            panic!("bob is told");
        };
        let mut done = *notice;
        crate::commands::mark_duties_done(&mut state, &ids[2], std::slice::from_ref(&thu)).unwrap();
        assert_eq!(
            back(&mut state, &mut done, "@bob:x.org", day(week, 5)).unwrap(),
            Back::TooLate
        );
        assert_eq!(who(&state, &group, week, 1), "@carol:x.org");
    }

    #[test]
    fn a_turn_taken_over_and_given_back_tells_its_holder() {
        let (mut state, group, week, ids) = split_week();
        let monday = day(week, 0);
        let thu = duty(&group, week, 1);
        let TakeOver::Notice(notice) =
            take_over(&mut state, &thu, "@carol:x.org", Some(&ids[1]), monday).unwrap()
        else {
            panic!("bob is told");
        };
        let mut notice = *notice;
        assert_eq!(
            back(&mut state, &mut notice, "@carol:x.org", monday).unwrap(),
            Back::Undone
        );
        assert_eq!(who(&state, &group, week, 1), "@bob:x.org");
        let (text, pings) = request_text(&state, &notice);
        assert!(text.starts_with("↩️ [carol]"), "{text}");
        assert!(text.contains(" gives **2nd Floor · Thu–Sun ("), "{text}");
        assert!(
            text.ends_with(" back to [bob](https://matrix.to/#/@bob:x.org)."),
            "{text}"
        );
        assert_eq!(pings, ["@bob:x.org"]);
        // The edit tells bob: no change notice on top.
        assert_eq!(
            state
                .told_holders
                .get(&crate::state::slot_key(&group.id, 0, thu.turn)),
            Some(&Some(ids[1].clone()))
        );
    }

    #[test]
    fn a_takeover_only_goes_through_while_the_turn_is_still_as_picked() {
        let (mut state, group, week, ids) = split_week();
        let monday = day(week, 0);
        let thu = duty(&group, week, 1);
        // Carol and alice both picked bob's Thu–Sun; carol confirms first.
        assert!(matches!(
            take_over(&mut state, &thu, "@carol:x.org", Some(&ids[1]), monday).unwrap(),
            TakeOver::Notice(_)
        ));
        assert_eq!(
            take_over(&mut state, &thu, "@alice:x.org", Some(&ids[1]), monday).unwrap(),
            TakeOver::Changed
        );
        assert_eq!(who(&state, &group, week, 1), "@carol:x.org");
        // Seeing it's carol's now, alice can still take it.
        assert!(matches!(
            take_over(&mut state, &thu, "@alice:x.org", Some(&ids[2]), monday).unwrap(),
            TakeOver::Notice(_)
        ));
        assert_eq!(who(&state, &group, week, 1), "@alice:x.org");
        assert_eq!(
            take_over(&mut state, &thu, "@alice:x.org", Some(&ids[0]), monday).unwrap(),
            TakeOver::Own
        );
        // Over, or done: nothing changes.
        let mon = duty(&group, week, 0);
        assert_eq!(
            take_over(&mut state, &mon, "@bob:x.org", Some(&ids[0]), day(week, 3)).unwrap(),
            TakeOver::Over
        );
        crate::commands::mark_duties_done(&mut state, &ids[0], std::slice::from_ref(&mon)).unwrap();
        assert_eq!(
            take_over(&mut state, &mon, "@bob:x.org", Some(&ids[0]), monday).unwrap(),
            TakeOver::Done
        );
        assert_eq!(who(&state, &group, week, 0), "@alice:x.org");
    }

    #[test]
    fn taking_over_a_turn_its_holder_asked_cover_for_sorts_the_request() {
        let (mut state, group, week, ids) = split_week();
        let monday = day(week, 0);
        let thu = duty(&group, week, 1);
        state
            .help_requests
            .insert("$sos".into(), new_request(&thu, "@bob:x.org", None));
        assert_eq!(
            take_over(&mut state, &thu, "@carol:x.org", Some(&ids[1]), monday).unwrap(),
            TakeOver::Sorted("$sos".into())
        );
        let req = &state.help_requests["$sos"];
        assert_eq!(req.status, HelpStatus::Taken);
        assert_eq!(req.helper.as_deref(), Some("@carol:x.org"));
        assert_eq!(who(&state, &group, week, 1), "@carol:x.org");
    }

    #[test]
    fn a_tap_on_a_request_asks_first_and_only_the_first_confirm_wins() {
        let (mut state, group, week, _) = split_week();
        let monday = day(week, 0);
        let thu = duty(&group, week, 1);
        state
            .help_requests
            .insert("$sos".into(), new_request(&thu, "@bob:x.org", None));
        let card = |user: &str, swap: bool| ConfirmCard {
            user_id: user.to_owned(),
            request_id: "$sos".into(),
            swap,
            give: None,
            note: None,
            status: CardStatus::Asking,
            rendered: String::new(),
        };
        let req = state.help_requests["$sos"].clone();
        assert_eq!(
            could_take(&state, &req, "@bob:x.org", false, monday),
            Taken::Own
        );
        assert_eq!(
            could_take(&state, &req, "@carol:x.org", false, monday),
            Taken::Done
        );
        // Asking changes nothing.
        assert_eq!(who(&state, &group, week, 1), "@bob:x.org");

        let carols = card("@carol:x.org", false);
        let text = card_text(&state, &carols, monday);
        assert!(
            text.starts_with(
                "🙋 [carol](https://matrix.to/#/@carol:x.org), take over **2nd Floor · Thu–Sun ("
            ),
            "{text}"
        );
        assert!(text.ends_with(" from bob?\nTap ✅ to confirm."), "{text}");

        // Carol swaps: she does bob's Thu–Sun, he her Mon–Wed next week.
        let swap = card_text(&state, &card("@carol:x.org", true), monday);
        assert!(swap.starts_with("🔄 [carol]"), "{swap}");
        assert!(
            swap.contains(", swap with bob?\n➡️ You do **2nd Floor · Thu–Sun ("),
            "{swap}"
        );
        assert!(
            swap.contains("\n⬅️ bob does your **2nd Floor · Mon–Wed ("),
            "{swap}"
        );
        assert!(swap.ends_with("\nTap ✅ to swap."), "{swap}");

        // Alice confirms first.
        let mut req = state.help_requests["$sos"].clone();
        assert_eq!(
            take(&mut state, &mut req, "@alice:x.org", false, monday).unwrap(),
            Taken::Done
        );
        state.help_requests.insert("$sos".into(), req.clone());
        assert_eq!(
            card_text(&state, &carols, monday),
            "⌛ alice was quicker. Thanks anyway!"
        );
        assert_eq!(
            could_take(&state, &req, "@carol:x.org", false, monday),
            Taken::NotOpen
        );
        let alices = card("@alice:x.org", false);
        assert!(card_text(&state, &alices, monday).starts_with("✅ Done. Thanks, [alice]"));
        assert_eq!(who(&state, &group, week, 1), "@alice:x.org");
    }

    #[test]
    fn whoever_gets_a_turn_by_a_trade_counts_as_told() {
        let (mut state, group, week, ids) = split_week();
        let monday = day(week, 0);
        let thu = duty(&group, week, 1);
        let mut req = new_request(&thu, "@bob:x.org", None);
        assert_eq!(
            take(&mut state, &mut req, "@carol:x.org", true, monday).unwrap(),
            Taken::Done
        );
        let told = |state: &State, shift_week: (i32, u32), shift: u8| {
            state
                .told_holders
                .get(&crate::state::slot_key(
                    &group.id,
                    0,
                    Turn::new(shift_week.0, shift_week.1, shift),
                ))
                .cloned()
        };
        let next = add_weeks(week.0, week.1, 1);
        // Carol has bob's Thu–Sun, bob carol's Mon–Wed next week.
        assert_eq!(told(&state, week, 1), Some(Some(ids[2].clone())));
        assert_eq!(told(&state, next, 0), Some(Some(ids[1].clone())));
        // Bob takes it back: he knows, and carol is told by a reply.
        assert_eq!(
            back(&mut state, &mut req, "@bob:x.org", monday).unwrap(),
            Back::Undone
        );
        assert_eq!(told(&state, week, 1), Some(Some(ids[1].clone())));
        assert_eq!(told(&state, next, 0), Some(Some(ids[2].clone())));
    }
}
