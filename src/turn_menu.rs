//! The turn menu: someone's next turns as a list to tap — `!next`, or 📅 on
//! the help board. Tap a number to pick a turn, then 🔄 to ask who swaps
//! with you, or 🆘 if you can't make it; either posts a request in the
//! cleaning room (`trades`). Like a group selector, only its owner's taps
//! count, the menu is edited in place, and each tap is taken back so the
//! same button works again. Only each person's latest menu is kept.
//!
//! ```text
//! 📅 Your next turns
//! 1️⃣ 2nd Floor · Thu–Sun (8 – 11 Oct) · later this week
//! 2️⃣ 👉 Kitchen · 13 – 19 Oct · next week
//! 3️⃣ 2nd Floor · Mon–Wed (20 – 22 Oct) · in 2 weeks · 🆘 asked
//!
//! 👉 Kitchen · 13 – 19 Oct — 🔄 swap it · 🆘 I can't make it
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
    format,
    onboarding::{number_index, NUMBERS},
    state::{HelpRequest, MenuTurn, State, TurnMenu},
    trades, view, BotContext,
};

/// Turns a menu shows unless asked for fewer — at most one per number.
pub const DEFAULT: usize = 5;
pub const MAX: usize = NUMBERS.len();

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
        selected: None,
        feedback: None,
        rendered: String::new(),
    })
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
                    line.push_str(" · tentative");
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
        Some(d) => lines.push(format!(
            "👉 **{}** — 🔄 swap it · 🆘 I can't make it",
            label(&d)
        )),
        None => {
            lines.push("Tap a number, then 🔄 to swap it or 🆘 if you can't make it.".to_owned())
        }
    }
    if let Some(feedback) = &menu.feedback {
        lines.push(feedback.clone());
    }
    if any_tentative {
        lines.push("Tentative = not fixed yet; may shift if members change.".to_owned());
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
    /// Ask the room — the request to post.
    Ask(HelpRequest),
}

/// Its owner tapped `key` on `menu`.
pub fn tap(state: &State, menu: &mut TurnMenu, key: &str, today: NaiveDate) -> Tapped {
    if let Some(i) = number_index(key) {
        let Some(t) = menu.turns.get(i) else {
            return Tapped::Nothing;
        };
        let (selected, feedback) = match row(state, menu, t, today) {
            Row::Open | Row::Asked { .. } => (Some(i), None),
            Row::Done => (None, Some("✅ That one is done already.")),
            Row::Moved => (None, Some("That one isn't yours any more.")),
            Row::Over | Row::Gone => (None, Some("That one is over.")),
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
        menu.feedback = Some("👆 Pick a turn first — tap its number.".to_owned());
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
                Some("You already asked for that one — see the cleaning room.".to_owned());
            Tapped::Changed
        }
        _ => {
            menu.feedback = Some("That one isn't open any more.".to_owned());
            Tapped::Changed
        }
    };
    menu.selected = None;
    outcome
}

// ── Matrix ────────────────────────────────────────────────────────────────────

/// Post a menu of `user`'s next `count` turns in `room` (as `relation`, if
/// given) and seed its buttons. `false` when they have no turns — nothing
/// is posted then.
pub async fn post(
    ctx: &BotContext,
    room: &Room,
    user: &UserId,
    count: usize,
    relation: Option<Answer>,
) -> Result<bool> {
    let in_cleaning_room = room.room_id() == ctx.room_id;
    let (menu, text) = {
        let state = ctx.state.lock().await;
        let Some(mut menu) = new_menu(&state, user.as_str(), room.room_id().as_str(), count) else {
            return Ok(false);
        };
        let text = menu_text(&state, &menu, in_cleaning_room, crate::state::today());
        menu.rendered = text.clone();
        (menu, text)
    };
    let mut content = format::intentional(crate::names::mentionify(ctx, &text, room).await);
    content.relates_to = relation;
    let event_id = room.send(content).await?.response.event_id;
    let buttons = menu.turns.len();
    {
        let mut state = ctx.state.lock().await;
        // Only the latest menu per person stays live.
        state.turn_menus.retain(|_, m| m.user_id != user.as_str());
        state.turn_menus.insert(event_id.to_string(), menu);
        state.save(&ctx.state_path).await?;
    }
    for key in NUMBERS[..buttons]
        .iter()
        .copied()
        .chain([trades::SWAP, trades::ASK])
    {
        let reaction = ReactionEventContent::new(Annotation::new(event_id.clone(), key.into()));
        if let Err(e) = room.send(reaction).await {
            tracing::warn!("Failed to seed {key} on a turn menu: {e}");
        }
    }
    Ok(true)
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
        state.turn_menus.insert(reacted_to.to_owned(), menu);
        if let Err(e) = state.save(&ctx.state_path).await {
            tracing::error!("Failed to save a turn menu tap: {e}");
            return true;
        }
        match tapped {
            Tapped::Ask(req) => Some(req),
            _ => None,
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
            (true, true) => format!("🔄 Asked in the cleaning room who swaps **{what}** with you."),
            (true, false) => format!("🆘 Asked in the cleaning room who steps in for **{what}**."),
            (false, _) => {
                "😕 I couldn't ask in the cleaning room — try again in a moment.".to_owned()
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
    crate::onboarding::consume_tap(room, &reaction_id.to_owned(), bot).await;
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
        match room.send(edit).await {
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
            text.ends_with("Tap a number, then 🔄 to swap it or 🆘 if you can't make it."),
            "{text}"
        );

        // An action first asks for a number.
        assert_eq!(tap(&state, &mut menu, "🆘", today), Tapped::Changed);
        assert_eq!(
            menu.feedback.as_deref(),
            Some("👆 Pick a turn first — tap its number.")
        );
        // A number past the list is no button.
        assert_eq!(tap(&state, &mut menu, "3️⃣", today), Tapped::Nothing);

        assert_eq!(tap(&state, &mut menu, "2\u{20e3}", today), Tapped::Changed);
        assert_eq!(menu.selected, Some(1));
        let text = menu_text(&state, &menu, false, today);
        assert!(text.contains("\n2️⃣ 👉 **2nd Floor · Mon–Wed ("), "{text}");
        assert!(
            text.contains("\n👉 **2nd Floor · Mon–Wed (")
                && text.contains("— 🔄 swap it · 🆘 I can't make it"),
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
            Some("You already asked for that one — see the cleaning room.")
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
            Some("That one isn't yours any more.")
        );
    }

    #[test]
    fn nobody_without_turns_gets_a_menu() {
        let (state, _, _, _) = bobs_turns();
        assert!(new_menu(&state, "@dan:x.org", "!r:x.org", 5).is_none());
    }
}
