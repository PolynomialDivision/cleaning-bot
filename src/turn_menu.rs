//! Turn menus: lists of turns to tap. Like a group selector, only its
//! owner's taps count, the menu is edited in place, and each tap is taken
//! back so the same button works again. Only each person's latest menu of
//! each kind is kept.
//!
//! **Your turns** (`!next`, 📅 on the help board): tap a number, then 🔄 to
//! ask who swaps with you, or 🆘 to give it away; either posts a request in
//! the cleaning room (`trades`).
//!
//! ```text
//! 📅 Your next turns
//! 1️⃣ 2nd Floor · Thu–Sun (8 – 11 Oct) · later this week
//! 2️⃣ 👉 Kitchen · 13 – 19 Oct · next week
//! 3️⃣ 2nd Floor · Mon–Wed (20 – 22 Oct) · in 2 weeks · 🆘 asked
//!
//! 👉 Kitchen · 13 – 19 Oct
//! 🔄 swap · 🆘 give away
//! ```
//!
//! **Take over a turn** (`!take`, 🙋 on the plan or the help board): other
//! people's turns in your groups, the next three weeks. Tap a number, then
//! ✅: it's yours, nobody has to agree. Its holder is told in the cleaning
//! room and can ↩️ keep it until it starts (`trades::take_over`).
//!
//! ```text
//! 🙋 Take over a turn
//! 1️⃣ Kitchen · 13 – 19 Oct · alice · 🆘
//! 2️⃣ 👉 Staircase / Scharni · 20 – 26 Oct · carol
//!
//! 👉 Staircase / Scharni · 20 – 26 Oct from carol
//! Tap ✅ to take it.
//! ```

use anyhow::Result;
use chrono::NaiveDate;
use mxbot_common::matrix_sdk::{
    ruma::{
        events::{
            reaction::ReactionEventContent, relation::Annotation,
            room::message::ReplacementMetadata,
        },
        EventId, OwnedEventId, OwnedUserId, UserId,
    },
    Client, Room,
};

use crate::{
    commands::{Answer, Duty},
    domain::{Person, PersonId},
    format,
    onboarding::{number_index, NUMBERS},
    state::{HelpRequest, MenuKind, MenuTurn, PickedFrom, State, TurnMenu},
    trades, view, BotContext,
};

/// Turns a menu shows unless asked for fewer, at most one per number.
pub const DEFAULT: usize = 5;
pub const MAX: usize = NUMBERS.len();
/// How many weeks ahead a take menu looks, this one included.
pub const TAKE_WEEKS: i64 = 3;
/// ✅ in a take menu: take the picked turn.
pub const CONFIRM: &str = "✅";

fn is_confirm(key: &str) -> bool {
    mxbot_common::reaction::is_one_of(key, &[CONFIRM])
}

/// A menu of `user`'s next `count` turns, in `room_id`; `None` when they
/// have none.
pub fn new_menu(state: &State, user: &str, room_id: &str, count: usize) -> Option<TurnMenu> {
    let person = state.person_by_matrix_id(user)?;
    let duties = crate::commands::upcoming_duties(state, &person.id, count.clamp(1, MAX));
    if duties.is_empty() {
        return None;
    }
    Some(TurnMenu {
        user_id: user.to_owned(),
        room_id: room_id.to_owned(),
        // Just one: picked already, one tap less.
        selected: (duties.len() == 1).then_some(0),
        turns: duties
            .iter()
            .map(|d| MenuTurn {
                group_id: d.group.id.clone(),
                slot_index: d.slot_index,
                iso_year: d.turn.year,
                iso_week: d.turn.week,
                shift: d.turn.shift,
            })
            .collect(),
        feedback: None,
        rendered: String::new(),
        kind: MenuKind::Mine,
        picked_from: None,
    })
}

/// A take menu for `user` (Matrix ID): the turns of their groups from this
/// week to `TAKE_WEEKS` ahead that aren't over, done or theirs, by date.
/// `None` when there are none.
pub fn new_take_menu(
    state: &State,
    user: &str,
    room_id: &str,
    today: NaiveDate,
) -> Option<TurnMenu> {
    use chrono::Datelike;
    let person = state.person_by_matrix_id(user)?;
    let from = (today.iso_week().year(), today.iso_week().week());
    let to = crate::state::add_weeks(from.0, from.1, TAKE_WEEKS - 1);
    let mut found = Vec::new();
    for group in state
        .groups_for_person(&person.id)
        .into_iter()
        .filter(|g| g.is_active)
    {
        for turn in state.turns_between(group, from, to) {
            let (start, end) = turn.dates(&group.rhythm);
            if end < today {
                continue;
            }
            for slot_index in State::slot_indices(group) {
                let theirs = state
                    .slot_assignee(group, slot_index, turn)
                    .is_some_and(|p| p.id == person.id);
                if theirs || state.is_turn_slot_done(group, slot_index, turn) {
                    continue;
                }
                found.push((
                    start,
                    group.name.clone(),
                    MenuTurn {
                        group_id: group.id.clone(),
                        slot_index,
                        iso_year: turn.year,
                        iso_week: turn.week,
                        shift: turn.shift,
                    },
                ));
            }
        }
    }
    if found.is_empty() {
        return None;
    }
    found.sort_by(|a, b| (a.0, &a.1, a.2.slot_index).cmp(&(b.0, &b.1, b.2.slot_index)));
    found.truncate(MAX);
    let turns: Vec<MenuTurn> = found.into_iter().map(|(_, _, t)| t).collect();
    // Just one: picked already, one tap less.
    let picked_from = match turns.as_slice() {
        [only] => Some(PickedFrom {
            holder: state
                .group_by_id(&only.group_id)
                .and_then(|g| state.slot_assignee(g, only.slot_index, only.turn()))
                .map(|p| p.id.clone()),
        }),
        _ => None,
    };
    Some(TurnMenu {
        user_id: user.to_owned(),
        room_id: room_id.to_owned(),
        selected: picked_from.as_ref().map(|_| 0),
        turns,
        feedback: None,
        rendered: String::new(),
        kind: MenuKind::Take,
        picked_from,
    })
}

/// A take menu of just `duty`, picked already: `!takeover <group> …`.
pub fn take_one(state: &State, user: &str, room_id: &str, duty: &Duty) -> TurnMenu {
    TurnMenu {
        user_id: user.to_owned(),
        room_id: room_id.to_owned(),
        turns: vec![MenuTurn {
            group_id: duty.group.id.clone(),
            slot_index: duty.slot_index,
            iso_year: duty.turn.year,
            iso_week: duty.turn.week,
            shift: duty.turn.shift,
        }],
        selected: Some(0),
        feedback: None,
        rendered: String::new(),
        kind: MenuKind::Take,
        picked_from: Some(PickedFrom {
            holder: state
                .slot_assignee(&duty.group, duty.slot_index, duty.turn)
                .map(|p| p.id.clone()),
        }),
    }
}

fn duty(state: &State, t: &MenuTurn) -> Option<Duty> {
    Some(Duty {
        group: state.group_by_id(&t.group_id)?.clone(),
        slot_index: t.slot_index,
        turn: t.turn(),
    })
}

/// Where one turn of a menu stands now.
#[derive(Debug, PartialEq)]
enum Row {
    /// Still theirs and open.
    Open,
    /// Still theirs, and they asked the room (🔄 when only for a swap).
    Asked {
        swap_only: bool,
    },
    Done,
    /// Someone else has it now (a swap, a takeover).
    Moved,
    Over,
    /// Its group is gone.
    Gone,
}

fn row(state: &State, menu: &TurnMenu, t: &MenuTurn, today: NaiveDate) -> Row {
    let Some(d) = duty(state, t) else {
        return Row::Gone;
    };
    if state.is_turn_slot_done(&d.group, d.slot_index, d.turn) {
        return Row::Done;
    }
    let holder = state
        .slot_assignee(&d.group, d.slot_index, d.turn)
        .and_then(|p| p.matrix_id.as_deref());
    if holder != Some(menu.user_id.as_str()) {
        return Row::Moved;
    }
    if d.turn.dates(&d.group.rhythm).1 < today {
        return Row::Over;
    }
    match trades::open_request(state, &d) {
        Some((_, req)) if req.requester == menu.user_id => Row::Asked {
            swap_only: req.swap_only,
        },
        _ => Row::Open,
    }
}

/// "2nd Floor / Scharni · Thu–Sun (8 – 11 Oct)".
fn label(d: &Duty) -> String {
    format!("{} · {}", d.place(), trades::when(d))
}

/// The menu as it reads now.
pub fn menu_text(
    state: &State,
    menu: &TurnMenu,
    in_cleaning_room: bool,
    today: NaiveDate,
) -> String {
    match menu.kind {
        MenuKind::Mine => own_text(state, menu, in_cleaning_room, today),
        MenuKind::Take => take_text(state, menu, in_cleaning_room, today),
    }
}

fn own_text(state: &State, menu: &TurnMenu, in_cleaning_room: bool, today: NaiveDate) -> String {
    let mut lines = vec![match state.person_by_matrix_id(&menu.user_id) {
        Some(p) if in_cleaning_room => format!("📅 **Your next turns** · {}", view::user_link(p)),
        _ => "📅 **Your next turns**".to_owned(),
    }];
    let mut any_tentative = false;
    for (i, t) in menu.turns.iter().enumerate() {
        let number = NUMBERS[i];
        let Some(d) = duty(state, t) else {
            lines.push(format!("{number} ~~(a group that's gone)~~"));
            continue;
        };
        let what = label(&d);
        let when = view::relative(d.turn, &d.group.rhythm, today);
        lines.push(match row(state, menu, t, today) {
            Row::Open | Row::Asked { .. } => {
                let pick = if menu.selected == Some(i) {
                    "👉 "
                } else {
                    ""
                };
                let mut line = format!("{number} {pick}**{what}** · {when}");
                let frozen = state.slot_assignments.iter().any(|a| {
                    a.group_id == d.group.id
                        && a.slot_index == d.slot_index
                        && (a.iso_year, a.iso_week, a.shift) == (t.iso_year, t.iso_week, t.shift)
                });
                if !frozen {
                    any_tentative = true;
                    line.push_str(" · ⏳");
                }
                match row(state, menu, t, today) {
                    Row::Asked { swap_only: true } => line.push_str(" · 🔄 asked"),
                    Row::Asked { swap_only: false } => line.push_str(" · 🆘 asked"),
                    _ => {}
                }
                line
            }
            Row::Done => format!("{number} ✅ ~~{what}~~ · done"),
            Row::Moved => {
                let now = state
                    .slot_assignee(&d.group, d.slot_index, d.turn)
                    .map(view::user_link)
                    .unwrap_or_else(|| "nobody".to_owned());
                format!("{number} ~~{what}~~ · now {now}")
            }
            Row::Over => format!("{number} ~~{what}~~ · over"),
            Row::Gone => format!("{number} ~~{what}~~"),
        });
    }
    lines.push(String::new());
    match menu
        .selected
        .and_then(|i| Some((i, menu.turns.get(i)?)))
        .filter(|(_, t)| matches!(row(state, menu, t, today), Row::Open | Row::Asked { .. }))
        .and_then(|(_, t)| duty(state, t))
    {
        Some(d) => lines.push(format!("👉 **{}**\n🔄 swap · 🆘 give away", label(&d))),
        None if menu.feedback.is_none() => {
            lines.push("👆 Tap a number, then 🔄 swap or 🆘 give away.".to_owned())
        }
        None => {}
    }
    if let Some(feedback) = &menu.feedback {
        lines.push(feedback.clone());
    }
    if any_tentative {
        lines.push("⏳ not fixed yet, may still change".to_owned());
    }
    lines.join("\n")
}

/// Where one turn of a take menu stands now.
enum TakeRow<'s> {
    /// Someone else's (or nobody's), to take.
    Open {
        holder: Option<&'s Person>,
        asked: bool,
    },
    /// Theirs now.
    Yours,
    Done,
    Over,
    Gone,
}

fn take_row<'s>(state: &'s State, menu: &TurnMenu, t: &MenuTurn, today: NaiveDate) -> TakeRow<'s> {
    let Some(d) = duty(state, t) else {
        return TakeRow::Gone;
    };
    if state.is_turn_slot_done(&d.group, d.slot_index, d.turn) {
        return TakeRow::Done;
    }
    if d.turn.dates(&d.group.rhythm).1 < today {
        return TakeRow::Over;
    }
    let holder = state.slot_assignee(&d.group, d.slot_index, d.turn);
    if holder.is_some_and(|p| p.matrix_id.as_deref() == Some(menu.user_id.as_str())) {
        return TakeRow::Yours;
    }
    TakeRow::Open {
        holder,
        asked: trades::open_request(state, &d).is_some(),
    }
}

fn who(holder: Option<&Person>) -> String {
    holder
        .map(view::user_link)
        .unwrap_or_else(|| "nobody".to_owned())
}

fn take_text(state: &State, menu: &TurnMenu, in_cleaning_room: bool, today: NaiveDate) -> String {
    let mut lines = vec![match state.person_by_matrix_id(&menu.user_id) {
        Some(p) if in_cleaning_room => format!("🙋 **Take over a turn** · {}", view::user_link(p)),
        _ => "🙋 **Take over a turn**".to_owned(),
    }];
    for (i, t) in menu.turns.iter().enumerate() {
        let number = NUMBERS[i];
        let Some(d) = duty(state, t) else {
            lines.push(format!("{number} ~~(a group that's gone)~~"));
            continue;
        };
        let what = label(&d);
        lines.push(match take_row(state, menu, t, today) {
            TakeRow::Open { holder, asked } => {
                let pick = if menu.selected == Some(i) {
                    "👉 "
                } else {
                    ""
                };
                let sos = if asked { " · 🆘 asked" } else { "" };
                format!("{number} {pick}**{what}** · {}{sos}", who(holder))
            }
            TakeRow::Yours => format!("{number} ✅ **{what}** · yours"),
            TakeRow::Done => format!("{number} ~~{what}~~ · done"),
            TakeRow::Over | TakeRow::Gone => format!("{number} ~~{what}~~ · over"),
        });
    }
    lines.push(String::new());
    let picked = menu.selected.and_then(|i| menu.turns.get(i)).and_then(|t| {
        match take_row(state, menu, t, today) {
            TakeRow::Open { holder, .. } => Some((duty(state, t)?, holder)),
            _ => None,
        }
    });
    match picked {
        Some((d, Some(holder))) => lines.push(format!(
            "👉 **{}** from {}\nTap ✅ to take it.",
            label(&d),
            view::user_link(holder)
        )),
        Some((d, None)) => lines.push(format!(
            "👉 **{}**, nobody has it yet\nTap ✅ to take it.",
            label(&d)
        )),
        None if menu.feedback.is_none() => lines.push("👆 Tap the one you'll do.".to_owned()),
        None => {}
    }
    if let Some(feedback) = &menu.feedback {
        lines.push(feedback.clone());
    }
    lines.join("\n")
}

/// What a tap on a menu did.
#[derive(Debug, PartialEq)]
pub enum Tapped {
    /// Not a button of the menu.
    Nothing,
    /// The menu changed (a turn picked, or why not).
    Changed,
    /// Ask the room. The request to post.
    Ask(HelpRequest),
    /// Take this turn over, as long as this one (or nobody) still holds it.
    Take(MenuTurn, Option<PersonId>),
}

/// Its owner tapped `key` on `menu`.
pub fn tap(state: &State, menu: &mut TurnMenu, key: &str, today: NaiveDate) -> Tapped {
    match menu.kind {
        MenuKind::Mine => tap_own(state, menu, key, today),
        MenuKind::Take => tap_take(state, menu, key, today),
    }
}

fn tap_take(state: &State, menu: &mut TurnMenu, key: &str, today: NaiveDate) -> Tapped {
    if let Some(i) = number_index(key) {
        let Some(t) = menu.turns.get(i) else {
            return Tapped::Nothing;
        };
        let (selected, from, feedback) = match take_row(state, menu, t, today) {
            TakeRow::Open { holder, .. } => (
                Some(i),
                Some(PickedFrom {
                    holder: holder.map(|p| p.id.clone()),
                }),
                None,
            ),
            TakeRow::Yours => (None, None, Some("👍 That one is yours.")),
            TakeRow::Done => (None, None, Some("✅ That one is done already.")),
            TakeRow::Over | TakeRow::Gone => (None, None, Some("⌛ That one is over.")),
        };
        menu.selected = selected;
        menu.picked_from = from;
        menu.feedback = feedback.map(str::to_owned);
        return Tapped::Changed;
    }
    if !is_confirm(key) {
        return Tapped::Nothing;
    }
    let Some(t) = menu.selected.and_then(|i| menu.turns.get(i)) else {
        menu.feedback = Some("👆 Pick a turn first: tap its number.".to_owned());
        return Tapped::Changed;
    };
    match take_row(state, menu, t, today) {
        TakeRow::Open { .. } => Tapped::Take(
            t.clone(),
            menu.picked_from.as_ref().and_then(|p| p.holder.clone()),
        ),
        _ => {
            menu.selected = None;
            menu.feedback = Some("⌛ That one isn't open any more.".to_owned());
            Tapped::Changed
        }
    }
}

fn tap_own(state: &State, menu: &mut TurnMenu, key: &str, today: NaiveDate) -> Tapped {
    if let Some(i) = number_index(key) {
        let Some(t) = menu.turns.get(i) else {
            return Tapped::Nothing;
        };
        let (selected, feedback) = match row(state, menu, t, today) {
            Row::Open | Row::Asked { .. } => (Some(i), None),
            Row::Done => (None, Some("✅ That one is done already.")),
            Row::Moved => (None, Some("🔀 That one isn't yours any more.")),
            Row::Over | Row::Gone => (None, Some("⌛ That one is over.")),
        };
        menu.selected = selected;
        menu.feedback = feedback.map(str::to_owned);
        return Tapped::Changed;
    }
    let swap = trades::is_swap(key);
    if !swap && !trades::is_ask(key) {
        return Tapped::Nothing;
    }
    let Some(t) = menu.selected.and_then(|i| menu.turns.get(i)) else {
        menu.feedback = Some("👆 Pick a turn first: tap its number.".to_owned());
        return Tapped::Changed;
    };
    let outcome = match (row(state, menu, t, today), duty(state, t)) {
        (Row::Open, Some(d)) => {
            let mut req = trades::new_request(&d, &menu.user_id, None);
            req.swap_only = swap;
            Tapped::Ask(req)
        }
        (Row::Asked { .. }, _) => {
            menu.feedback =
                Some("☝️ You already asked for that one. See the cleaning room.".to_owned());
            Tapped::Changed
        }
        _ => {
            menu.feedback = Some("⌛ That one isn't open any more.".to_owned());
            Tapped::Changed
        }
    };
    menu.selected = None;
    outcome
}

// ── Matrix ────────────────────────────────────────────────────────────────────

/// Post a menu of `user`'s next `count` turns in `room` (as `relation`, if
/// given) and seed its buttons. `false` when they have no turns. Nothing
/// is posted then.
pub async fn post(
    ctx: &BotContext,
    room: &Room,
    user: &UserId,
    count: usize,
    relation: Option<Answer>,
) -> Result<bool> {
    let menu = {
        let state = ctx.state.lock().await;
        new_menu(&state, user.as_str(), room.room_id().as_str(), count)
    };
    match menu {
        Some(menu) => post_menu(ctx, room, menu, relation).await.map(|()| true),
        None => Ok(false),
    }
}

/// Post a take menu for `user` in `room` (as `relation`, if given). `false`
/// when there's nothing to take over: nothing is posted then.
pub async fn post_take(
    ctx: &BotContext,
    room: &Room,
    user: &UserId,
    relation: Option<Answer>,
) -> Result<bool> {
    let menu = {
        let state = ctx.state.lock().await;
        new_take_menu(
            &state,
            user.as_str(),
            room.room_id().as_str(),
            crate::state::today(),
        )
    };
    match menu {
        Some(menu) => post_menu(ctx, room, menu, relation).await.map(|()| true),
        None => Ok(false),
    }
}

/// What an old menu says once a newer one replaced it.
const REPLACED: &str = "⌛ Replaced by a newer menu.";
/// A take menu's feedback once the turn is taken. In the cleaning room the
/// menu then goes away: the pinned plan shows the new holder.
const TAKEN: &str = "✅ It's yours. Thanks! 💛";

/// What a take menu says when there's nothing to take.
pub const NOTHING_TO_TAKE: &str = "🤷 Nothing to take over in the next 3 weeks.";

/// Post `menu` in `room` (as `relation`, if given) and seed its buttons.
pub async fn post_menu(
    ctx: &BotContext,
    room: &Room,
    mut menu: TurnMenu,
    relation: Option<Answer>,
) -> Result<()> {
    let in_cleaning_room = room.room_id() == ctx.room_id;
    let text = {
        let state = ctx.state.lock().await;
        menu_text(&state, &menu, in_cleaning_room, crate::state::today())
    };
    menu.rendered = text.clone();
    let mut content = format::intentional(crate::names::mentionify(ctx, &text, room).await);
    content.relates_to = relation;
    let event_id = mxbot_common::send::send_with_retry(room, content).await?;
    let buttons = menu.turns.len();
    let kind = menu.kind;
    let replaced: Vec<(String, String)> = {
        let mut state = ctx.state.lock().await;
        // Only the latest menu per person and kind stays live.
        let replaced = state
            .turn_menus
            .iter()
            .filter(|(_, m)| m.user_id == menu.user_id && m.kind == kind)
            .map(|(id, m)| (id.clone(), m.room_id.clone()))
            .collect();
        state
            .turn_menus
            .retain(|_, m| m.user_id != menu.user_id || m.kind != kind);
        state.turn_menus.insert(event_id.to_string(), menu);
        state.save(&ctx.state_path).await?;
        replaced
    };
    // An old menu's buttons do nothing any more: it says so.
    for (id, room_id) in replaced {
        let (Ok(id), Some(old_room)) = (
            id.parse::<OwnedEventId>(),
            room_id
                .parse::<mxbot_common::matrix_sdk::ruma::OwnedRoomId>()
                .ok()
                .and_then(|r| room.client().get_room(&r)),
        ) else {
            continue;
        };
        let edit = format::quiet(format::mentionify(REPLACED))
            .make_replacement(ReplacementMetadata::new(id, None));
        if let Err(e) = mxbot_common::send::send_with_retry(&old_room, edit).await {
            tracing::warn!("Failed to retire an old turn menu: {e}");
        }
    }
    let actions: &[&str] = match kind {
        MenuKind::Mine => &[trades::SWAP, trades::ASK],
        MenuKind::Take => &[CONFIRM],
    };
    for key in NUMBERS[..buttons].iter().chain(actions).copied() {
        let reaction = ReactionEventContent::new(Annotation::new(event_id.clone(), key.into()));
        if let Err(e) = mxbot_common::send::send_with_retry(room, reaction).await {
            tracing::warn!("Failed to seed {key} on a turn menu: {e}");
        }
    }
    Ok(())
}

/// What follows a takeover from a menu, once the state is saved.
enum AfterTake {
    Nothing,
    /// Tell the one who held it.
    Notice(HelpRequest),
    /// A request got sorted: bring its message up to date.
    Sorted(String),
}

/// `user` takes `t` over (`expected` held it when picked): the menu's
/// feedback, and what to do next.
fn take_from_menu(
    state: &mut State,
    menu: &mut TurnMenu,
    t: &MenuTurn,
    expected: Option<&PersonId>,
    today: NaiveDate,
) -> AfterTake {
    let Some(d) = duty(state, t) else {
        menu.selected = None;
        menu.feedback = Some("⌛ That one is over.".to_owned());
        return AfterTake::Nothing;
    };
    let user = menu.user_id.clone();
    let outcome = match trades::take_over(state, &d, &user, expected, today) {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::error!("A takeover failed: {e}");
            menu.feedback = Some("😕 That didn't work. Try again in a moment.".to_owned());
            return AfterTake::Nothing;
        }
    };
    let thanks = TAKEN.to_owned();
    let (feedback, after) = match outcome {
        trades::TakeOver::Notice(req) => (thanks, AfterTake::Notice(*req)),
        trades::TakeOver::Sorted(id) => (thanks, AfterTake::Sorted(id)),
        trades::TakeOver::Quiet => (thanks, AfterTake::Nothing),
        trades::TakeOver::Changed => {
            // Someone else has it now: picked from them, one more ✅ takes it.
            let now = state.slot_assignee(&d.group, d.slot_index, d.turn);
            menu.picked_from = Some(PickedFrom {
                holder: now.map(|p| p.id.clone()),
            });
            menu.feedback = Some(format!(
                "🔀 It changed hands, {} has it now. ✅ again takes it from them.",
                who(now)
            ));
            return AfterTake::Nothing;
        }
        trades::TakeOver::Done => (
            "✅ Someone cleaned it already.".to_owned(),
            AfterTake::Nothing,
        ),
        trades::TakeOver::Over => ("⌛ That one is over.".to_owned(), AfterTake::Nothing),
        trades::TakeOver::Own => ("👍 That one is yours.".to_owned(), AfterTake::Nothing),
    };
    menu.selected = None;
    menu.picked_from = None;
    menu.feedback = Some(feedback);
    after
}

/// A reaction on a turn menu: its owner's taps do things (and are taken
/// back), anyone else's are ignored. Returns whether it was on a menu.
pub async fn on_reaction(
    ctx: &BotContext,
    room: &Room,
    reaction_id: &EventId,
    sender: &UserId,
    reacted_to: &str,
    key: &str,
    bot: &OwnedUserId,
) -> bool {
    let today = crate::state::today();
    let delete;
    let request = {
        let mut state = ctx.state.lock().await;
        let Some(mut menu) = state
            .turn_menus
            .get(reacted_to)
            .filter(|m| m.room_id == room.room_id().as_str())
            .cloned()
        else {
            return false;
        };
        if menu.user_id != sender.as_str() {
            return true;
        }
        // Once per tap, also when delivered again later.
        if !state.trade_taps.insert(reaction_id.to_string()) {
            return true;
        }
        let tapped = tap(&state, &mut menu, key, today);
        if tapped == Tapped::Nothing {
            let _ = state.save(&ctx.state_path).await;
            return true;
        }
        let after = match &tapped {
            Tapped::Take(t, expected) => {
                take_from_menu(&mut state, &mut menu, t, expected.as_ref(), today)
            }
            _ => AfterTake::Nothing,
        };
        // A turn taken from a menu in the cleaning room: the menu is done
        // and only clutters the room, it goes (below). In a private chat it
        // stays as the confirmation.
        delete = menu.feedback.as_deref() == Some(TAKEN) && menu.room_id == ctx.room_id.as_str();
        if delete {
            state.turn_menus.remove(reacted_to);
        } else {
            state.turn_menus.insert(reacted_to.to_owned(), menu);
        }
        if let Err(e) = state.save(&ctx.state_path).await {
            tracing::error!("Failed to save a turn menu tap: {e}");
            return true;
        }
        match (tapped, after) {
            (Tapped::Ask(req), _) => Some(req),
            (_, AfterTake::Nothing) => None,
            (_, after) => {
                drop(state);
                if let Some(main) = room.client().get_room(&ctx.room_id) {
                    match after {
                        AfterTake::Notice(req) => {
                            let slot = (req.group_id.clone(), req.slot_index, req.turn());
                            let holder = req.requester.clone();
                            if trades::post_request(ctx, &main, req).await.is_none() {
                                // Not told: a change notice tells them instead.
                                let mut state = ctx.state.lock().await;
                                let was = state.person_by_matrix_id(&holder).map(|p| p.id.clone());
                                state.mark_told(&slot.0, slot.1, slot.2, was.as_ref());
                                if let Err(e) = state.save(&ctx.state_path).await {
                                    tracing::error!("Failed to save a takeover: {e}");
                                }
                            }
                        }
                        AfterTake::Sorted(id) => trades::update_request(ctx, &main, &id).await,
                        AfterTake::Nothing => {}
                    }
                    let (year, week) = crate::state::current_iso_week();
                    crate::scheduler::refresh_pinned_plan(ctx, &main, year, week).await;
                }
                None
            }
        }
    };
    if let Some(req) = request {
        let what = {
            let state = ctx.state.lock().await;
            state
                .group_by_id(&req.group_id)
                .map(|group| {
                    label(&Duty {
                        group: group.clone(),
                        slot_index: req.slot_index,
                        turn: req.turn(),
                    })
                })
                .unwrap_or_default()
        };
        let swap_only = req.swap_only;
        let posted = match room.client().get_room(&ctx.room_id) {
            Some(main) => trades::post_request(ctx, &main, req).await.is_some(),
            None => false,
        };
        let feedback = match (posted, swap_only) {
            (true, true) => format!("🔄 Asked the group who swaps **{what}**."),
            (true, false) => format!("🆘 Asked the group who takes over **{what}**."),
            (false, _) => {
                "😕 I couldn't ask in the cleaning room. Try again in a moment.".to_owned()
            }
        };
        let mut state = ctx.state.lock().await;
        if let Some(menu) = state.turn_menus.get_mut(reacted_to) {
            menu.feedback = Some(feedback);
        }
        if let Err(e) = state.save(&ctx.state_path).await {
            tracing::error!("Failed to save a turn menu: {e}");
        }
    }
    refresh_all(ctx, &room.client()).await;
    if delete {
        // The bot's own message: no power level needed. Its reactions go with it.
        let menu_id = reacted_to.parse::<OwnedEventId>();
        match menu_id {
            Ok(id) => {
                if let Err(e) = room.redact(&id, None, None).await {
                    tracing::warn!("Failed to remove a used take menu: {e}");
                }
            }
            Err(e) => tracing::warn!("A take menu with a bad event ID {reacted_to}: {e}"),
        }
    } else {
        crate::onboarding::consume_tap(room, &reaction_id.to_owned(), bot).await;
    }
    true
}

/// Bring every live menu up to date; the edits notify nobody.
pub async fn refresh_all(ctx: &BotContext, client: &Client) {
    let today = crate::state::today();
    let stale: Vec<(String, String, String)> = {
        let state = ctx.state.lock().await;
        state
            .turn_menus
            .iter()
            .filter_map(|(id, menu)| {
                let in_cleaning_room = menu.room_id == ctx.room_id.as_str();
                let text = menu_text(&state, menu, in_cleaning_room, today);
                (text != menu.rendered).then(|| (id.clone(), menu.room_id.clone(), text))
            })
            .collect()
    };
    for (id, room_id, text) in stale {
        let (Ok(event_id), Ok(room_id)) = (
            id.parse::<OwnedEventId>(),
            room_id.parse::<mxbot_common::matrix_sdk::ruma::OwnedRoomId>(),
        ) else {
            continue;
        };
        let Some(room) = client.get_room(&room_id) else {
            continue;
        };
        let edit = format::quiet(crate::names::mentionify(ctx, &text, &room).await)
            .make_replacement(ReplacementMetadata::new(event_id, None));
        match mxbot_common::send::send_with_retry(&room, edit).await {
            Ok(_) => {
                let mut state = ctx.state.lock().await;
                if let Some(menu) = state.turn_menus.get_mut(&id) {
                    menu.rendered = text;
                }
                if let Err(e) = state.save(&ctx.state_path).await {
                    tracing::warn!("Failed to save an updated turn menu: {e}");
                }
            }
            Err(e) => tracing::warn!("Failed to update a turn menu: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        analytics::DomainEvent,
        domain::{AssignmentSource, CleaningGroup, Person},
        rhythm::{Rhythm, Turn},
        state::add_weeks,
    };

    /// Bob's turns of a Mon–Wed / Thu–Sun group: Thu–Sun in two weeks and
    /// Mon–Wed in three (frozen), alice and carol on the others.
    fn bobs_turns() -> (State, CleaningGroup, (i32, u32), NaiveDate) {
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
        // Only bob's turns matter; fill the weeks up to them for everyone
        // else so the rotation preview gives him nothing earlier.
        for i in 0..=3 {
            let (year, week) = add_weeks(y, w, i);
            for shift in 0..2u8 {
                let who = match (i, shift) {
                    (2, 1) | (3, 0) => 1,
                    (_, 0) => 0,
                    _ => 2,
                };
                state
                    .apply_event(DomainEvent::SlotAssigned {
                        group_id: group.id.clone(),
                        slot_index: 0,
                        iso_year: year,
                        iso_week: week,
                        shift,
                        person_id: Some(ids[who].clone()),
                        source: AssignmentSource::RoundRobin,
                        actor_id: None,
                        previous_person_id: None,
                    })
                    .unwrap();
            }
        }
        let week = add_weeks(y, w, 2);
        (state, group, week, crate::state::today())
    }

    #[test]
    fn pick_a_turn_then_ask_for_a_swap_or_cover() {
        let (state, _, week, today) = bobs_turns();
        let mut menu = new_menu(&state, "@bob:x.org", "!dm:x.org", 2).expect("bob has turns");
        assert_eq!(menu.turns.len(), 2);
        assert_eq!(menu.turns[0].turn(), Turn::new(week.0, week.1, 1));
        let text = menu_text(&state, &menu, false, today);
        assert!(
            text.starts_with("📅 **Your next turns**\n1️⃣ **2nd Floor · Thu–Sun ("),
            "{text}"
        );
        assert!(text.contains("\n2️⃣ **2nd Floor · Mon–Wed ("), "{text}");
        assert!(
            text.ends_with("👆 Tap a number, then 🔄 swap or 🆘 give away."),
            "{text}"
        );

        // An action first asks for a number.
        assert_eq!(tap(&state, &mut menu, "🆘", today), Tapped::Changed);
        assert_eq!(
            menu.feedback.as_deref(),
            Some("👆 Pick a turn first: tap its number.")
        );
        // A number past the list is no button.
        assert_eq!(tap(&state, &mut menu, "3️⃣", today), Tapped::Nothing);

        assert_eq!(tap(&state, &mut menu, "2\u{20e3}", today), Tapped::Changed);
        assert_eq!(menu.selected, Some(1));
        let text = menu_text(&state, &menu, false, today);
        assert!(text.contains("\n2️⃣ 👉 **2nd Floor · Mon–Wed ("), "{text}");
        assert!(
            text.contains("\n👉 **2nd Floor · Mon–Wed (")
                && text.contains("\n🔄 swap · 🆘 give away"),
            "{text}"
        );

        let Tapped::Ask(req) = tap(&state, &mut menu, "🔄", today) else {
            panic!("a swap request");
        };
        assert!(req.swap_only);
        assert_eq!(req.requester, "@bob:x.org");
        assert_eq!(req.turn(), menu.turns[1].turn());
        assert_eq!(menu.selected, None);

        assert_eq!(tap(&state, &mut menu, "1️⃣", today), Tapped::Changed);
        let Tapped::Ask(req) = tap(&state, &mut menu, "🆘", today) else {
            panic!("a request for cover");
        };
        assert!(!req.swap_only);
        assert_eq!(req.turn(), menu.turns[0].turn());
    }

    #[test]
    fn the_menu_shows_what_became_of_a_turn() {
        let (mut state, group, week, today) = bobs_turns();
        let mut menu = new_menu(&state, "@bob:x.org", "!room:x.org", 2).unwrap();
        // Bob asked about Thu–Sun; then carol took it.
        let d = duty(&state, &menu.turns[0]).unwrap();
        state
            .help_requests
            .insert("$req".into(), trades::new_request(&d, "@bob:x.org", None));
        let text = menu_text(&state, &menu, true, today);
        assert!(text.starts_with("📅 **Your next turns** · [bob]"), "{text}");
        assert!(text.contains("· 🆘 asked"), "{text}");
        assert_eq!(tap(&state, &mut menu, "1️⃣", today), Tapped::Changed);
        assert_eq!(tap(&state, &mut menu, "🔄", today), Tapped::Changed);
        assert_eq!(
            menu.feedback.as_deref(),
            Some("☝️ You already asked for that one. See the cleaning room.")
        );

        let carol = state
            .person_by_matrix_id("@carol:x.org")
            .unwrap()
            .id
            .clone();
        state
            .apply_event(DomainEvent::SlotAssigned {
                group_id: group.id.clone(),
                slot_index: 0,
                iso_year: week.0,
                iso_week: week.1,
                shift: 1,
                person_id: Some(carol),
                source: AssignmentSource::Takeover,
                actor_id: None,
                previous_person_id: None,
            })
            .unwrap();
        let text = menu_text(&state, &menu, true, today);
        assert!(text.contains("\n1️⃣ ~~2nd Floor · Thu–Sun ("), "{text}");
        assert!(text.contains("~~ · now [carol]"), "{text}");
        assert_eq!(tap(&state, &mut menu, "1️⃣", today), Tapped::Changed);
        assert_eq!(menu.selected, None);
        assert_eq!(
            menu.feedback.as_deref(),
            Some("🔀 That one isn't yours any more.")
        );
    }

    #[test]
    fn nobody_without_turns_gets_a_menu() {
        let (state, _, _, _) = bobs_turns();
        assert!(new_menu(&state, "@dan:x.org", "!r:x.org", 5).is_none());
    }

    #[test]
    fn a_take_menu_lists_other_peoples_turns_and_takes_one_with_a_confirm() {
        let (mut state, group, week, today) = bobs_turns();
        let mut menu =
            new_take_menu(&state, "@alice:x.org", "!dm:x.org", today).expect("turns to take");
        // Only others' turns of this week and the next two, none over, by date.
        assert!(menu.turns.iter().all(|t| {
            let holder = state
                .slot_assignee(&group, t.slot_index, t.turn())
                .and_then(|p| p.matrix_id.clone());
            holder.as_deref() != Some("@alice:x.org")
        }));
        let last = add_weeks(
            crate::state::current_iso_week().0,
            crate::state::current_iso_week().1,
            2,
        );
        assert!(menu.turns.iter().all(|t| (t.iso_year, t.iso_week) <= last));
        assert!(menu
            .turns
            .iter()
            .any(|t| t.turn() == Turn::new(week.0, week.1, 1)));
        let text = menu_text(&state, &menu, false, today);
        assert!(
            text.starts_with("🙋 **Take over a turn**\n1️⃣ **2nd Floor · "),
            "{text}"
        );
        assert!(text.ends_with("\n\n👆 Tap the one you'll do."), "{text}");
        assert!(!text.contains('—'), "{text}");

        // ✅ first asks for a number; a number picks; nothing changes yet.
        assert_eq!(tap(&state, &mut menu, CONFIRM, today), Tapped::Changed);
        assert_eq!(
            menu.feedback.as_deref(),
            Some("👆 Pick a turn first: tap its number.")
        );
        let i = menu
            .turns
            .iter()
            .position(|t| t.turn() == Turn::new(week.0, week.1, 1))
            .unwrap();
        assert_eq!(tap(&state, &mut menu, NUMBERS[i], today), Tapped::Changed);
        let bob = state.person_by_matrix_id("@bob:x.org").unwrap().id.clone();
        assert_eq!(
            menu.picked_from.as_ref().unwrap().holder.as_ref(),
            Some(&bob)
        );
        let text = menu_text(&state, &menu, false, today);
        assert!(text.contains("\n👉 **2nd Floor · Thu–Sun ("), "{text}");
        assert!(
            text.ends_with(" from [bob](https://matrix.to/#/@bob:x.org)\nTap ✅ to take it."),
            "{text}"
        );
        // 🔄 and 🆘 are no buttons here.
        assert_eq!(tap(&state, &mut menu, "🆘", today), Tapped::Nothing);

        let Tapped::Take(t, expected) = tap(&state, &mut menu, "✅\u{fe0f}", today) else {
            panic!("✅ takes it");
        };
        assert_eq!(expected.as_ref(), Some(&bob));
        let after = take_from_menu(&mut state, &mut menu, &t, expected.as_ref(), today);
        let AfterTake::Notice(notice) = after else {
            panic!("bob is told");
        };
        assert_eq!(notice.requester, "@bob:x.org");
        assert_eq!(menu.feedback.as_deref(), Some("✅ It's yours. Thanks! 💛"));
        let text = menu_text(&state, &menu, false, today);
        assert!(
            text.contains(&format!("{} ✅ **2nd Floor · Thu–Sun (", NUMBERS[i])),
            "{text}"
        );
        assert!(text.contains("· yours"), "{text}");
        assert!(text.ends_with("\n\n✅ It's yours. Thanks! 💛"), "{text}");
        // Tapping it again: it's theirs.
        assert_eq!(tap(&state, &mut menu, NUMBERS[i], today), Tapped::Changed);
        assert_eq!(menu.feedback.as_deref(), Some("👍 That one is yours."));
    }

    #[test]
    fn a_turn_that_changed_hands_after_the_pick_needs_one_more_confirm() {
        let (mut state, group, week, today) = bobs_turns();
        let thu = Turn::new(week.0, week.1, 1);
        let mut menu = new_take_menu(&state, "@alice:x.org", "!dm:x.org", today).unwrap();
        let i = menu.turns.iter().position(|t| t.turn() == thu).unwrap();
        tap(&state, &mut menu, NUMBERS[i], today);
        // Carol takes it in the meantime.
        let carol = state
            .person_by_matrix_id("@carol:x.org")
            .unwrap()
            .id
            .clone();
        let d = duty(&state, &menu.turns[i]).unwrap();
        let bob = state.person_by_matrix_id("@bob:x.org").unwrap().id.clone();
        trades::take_over(&mut state, &d, "@carol:x.org", Some(&bob), today).unwrap();

        let Tapped::Take(t, expected) = tap(&state, &mut menu, CONFIRM, today) else {
            panic!("still open to take");
        };
        assert!(matches!(
            take_from_menu(&mut state, &mut menu, &t, expected.as_ref(), today),
            AfterTake::Nothing
        ));
        assert_eq!(
            state
                .slot_assignee(&group, 0, thu)
                .unwrap()
                .matrix_id
                .as_deref(),
            Some("@carol:x.org")
        );
        assert!(
            menu.feedback
                .as_deref()
                .unwrap()
                .starts_with("🔀 It changed hands, [carol]"),
            "{:?}",
            menu.feedback
        );
        assert_eq!(
            menu.picked_from.as_ref().unwrap().holder.as_ref(),
            Some(&carol)
        );
        // One more ✅, knowing it's carol's: alice has it.
        let Tapped::Take(t, expected) = tap(&state, &mut menu, CONFIRM, today) else {
            panic!("still open to take");
        };
        assert!(matches!(
            take_from_menu(&mut state, &mut menu, &t, expected.as_ref(), today),
            AfterTake::Notice(_)
        ));
        assert_eq!(
            state
                .slot_assignee(&group, 0, thu)
                .unwrap()
                .matrix_id
                .as_deref(),
            Some("@alice:x.org")
        );
    }

    #[test]
    fn takeover_by_command_is_a_menu_with_the_turn_picked() {
        let (state, group, week, today) = bobs_turns();
        let d = Duty {
            group: group.clone(),
            slot_index: 0,
            turn: Turn::new(week.0, week.1, 1),
        };
        let mut menu = take_one(&state, "@alice:x.org", "!room:x.org", &d);
        let text = menu_text(&state, &menu, true, today);
        assert!(
            text.starts_with("🙋 **Take over a turn** · [alice]"),
            "{text}"
        );
        assert!(text.contains("\n1️⃣ 👉 **2nd Floor · Thu–Sun ("), "{text}");
        assert!(text.ends_with("\nTap ✅ to take it."), "{text}");
        assert!(matches!(
            tap(&state, &mut menu, CONFIRM, today),
            Tapped::Take(..)
        ));
    }

    #[test]
    fn a_menu_of_one_turn_has_it_picked_already() {
        let (state, _, _, today) = bobs_turns();
        let mut menu = new_menu(&state, "@bob:x.org", "!dm:x.org", 1).unwrap();
        assert_eq!(menu.selected, Some(0));
        assert!(matches!(
            tap(&state, &mut menu, "🆘", today),
            Tapped::Ask(_)
        ));
    }

    #[test]
    fn nobody_without_groups_gets_a_take_menu() {
        let (state, _, _, today) = bobs_turns();
        assert!(new_take_menu(&state, "@dan:x.org", "!r:x.org", today).is_none());
    }
}
