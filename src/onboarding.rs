//! The welcome and the group selector.
//!
//! Every Matrix user gets one welcome, ever — the first time they show up
//! here: joining the room, or their first command or reaction in it. That
//! holds whether or not an admin added or linked them before; the groups
//! they're already in show as joined. The welcome *is* a group selector:
//! the active groups, numbered 1️⃣ 2️⃣ 3️⃣ …, theirs marked ✅. `!mygroups`
//! posts a fresh selector (without the intro) any time.
//!
//! Each tap on a number toggles that group — join if they're not in it,
//! leave if they are — so it works the same however they got in. The
//! selector is edited to show the new state, with a one-line confirmation
//! (or why not) in it; no extra message. Once the tap is saved the bot
//! redacts the reaction (`consume_tap`), so the same number can be tapped
//! again. Without the power to do that the reaction stays, and taking it
//! back by hand undoes exactly that tap (when it still applies). Every tap
//! is recorded by its reaction event ID, so a re-delivered event counts
//! once, also across restarts.
//!
//! A selector belongs to one user and one room: the cleaning room, or a
//! verified private chat (`private`) — the only place a calendar feed link
//! is ever shown.

use anyhow::Result;
use mxbot_common::matrix_sdk::{
    ruma::{
        events::{
            reaction::ReactionEventContent,
            relation::Annotation,
            room::message::{ReplacementMetadata, RoomMessageEventContent},
            Mentions,
        },
        OwnedEventId, OwnedUserId, RoomId,
    },
    Room,
};

use crate::{
    analytics::DomainEvent,
    commands::{group_name_of, join_group, leave_group},
    format,
    state::{GroupSelector, SelectorTap, State, TapEffect},
    view, BotContext,
};

/// The numbers to tap, in order; groups beyond them aren't offered.
pub const NUMBERS: [&str; 10] = ["1️⃣", "2️⃣", "3️⃣", "4️⃣", "5️⃣", "6️⃣", "7️⃣", "8️⃣", "9️⃣", "🔟"];

/// Whether `user_id` still gets their welcome — and from now on not
/// anymore. Persist the state before sending, so a restart can't send it
/// twice.
pub fn claim_welcome(state: &mut State, user_id: &str) -> bool {
    state.greeted_users.insert(user_id.to_owned())
}

/// A selector for `user_id`, offering every active group.
pub fn new_selector(state: &State, user_id: &str, welcome: bool) -> GroupSelector {
    let mut selector = GroupSelector {
        user_id: user_id.to_owned(),
        group_ids: state
            .cleaning_groups
            .iter()
            .filter(|g| g.is_active)
            .take(NUMBERS.len())
            .map(|g| g.id.clone())
            .collect(),
        welcome,
        ..GroupSelector::default()
    };
    selector.rendered = selector_text(state, &selector);
    selector
}

/// The selector as it should read now:
///
/// ```text
/// 👋 Welcome, @mia!
/// 🧹 I'm the cleaning bot: I keep track of whose turn it is and remind
/// you when it's yours.
///
/// Tap a number to join or leave a group:
/// 1️⃣ ✅ **2nd Floor**
/// 2️⃣ 3+4 Floor
/// ✅ Joined **2nd Floor**
///
/// 📅 Next: 2nd Floor · 5 – 11 Oct
/// 🗓 Your calendar: send !ical to @bot in a private chat
/// !mygroups reopens this · !join / !leave · !help
/// ```
pub fn selector_text(state: &State, selector: &GroupSelector) -> String {
    selector_text_with(state, selector, &crate::schedule::build_schedule(state, 52))
}

/// `selector_text` with the schedule already built — one for many selectors.
fn selector_text_with(
    state: &State,
    selector: &GroupSelector,
    schedule: &crate::schedule::ScheduleSnapshot,
) -> String {
    let person = state.person_by_matrix_id(&selector.user_id);
    let mut lines = Vec::new();
    let tap_line = if selector.welcome {
        // The welcome is addressed to them: a real mention.
        lines.push(format!("👋 Welcome, {}!", selector.user_id));
        lines.push(
            "🧹 I'm the cleaning bot: I keep track of whose turn it is and remind you \
             when it's yours."
                .into(),
        );
        lines.push(String::new());
        "Tap a number to join or leave a group:".to_owned()
    } else {
        let who = person.map_or_else(|| view::user_id_link(&selector.user_id), view::user_link);
        format!("🏠 Groups for {who} — tap a number to join or leave:")
    };

    let mut choices = Vec::new();
    for (number, group_id) in NUMBERS.iter().zip(&selector.group_ids) {
        let Some(group) = state.group_by_id(group_id) else {
            continue;
        };
        let paused = if group.is_active { "" } else { " · paused" };
        let joined = person.is_some_and(|p| state.is_member(group_id, &p.id));
        choices.push(if joined {
            format!("{number} ✅ **{}**{paused}", group.name)
        } else {
            format!("{number} {}{paused}", group.name)
        });
    }
    if state.cleaning_groups.iter().filter(|g| g.is_active).count() > NUMBERS.len() {
        choices.push("More groups: !groups · !join <group>".into());
    }
    if choices.is_empty() {
        lines.push("There are no cleaning groups yet — an admin will set them up.".into());
    } else {
        lines.push(tap_line);
        lines.extend(choices);
    }
    if let Some(feedback) = current_feedback(state, selector) {
        lines.push(feedback.to_owned());
    }
    lines.push(String::new());
    if let Some(person) = person.filter(|p| {
        state
            .cleaning_groups
            .iter()
            .any(|g| g.member_ids.contains(&p.id))
    }) {
        let today = crate::state::today();
        match schedule
            .for_person(&person.id)
            .into_iter()
            .find(|a| !a.is_completed && a.end >= today)
        {
            Some(next) => lines.push(format!(
                "📅 Next: {} · {}",
                next.group_name, next.period_label
            )),
            None => lines.push("📅 No upcoming turn yet.".into()),
        }
    }
    lines.push(match &selector.calendar_url {
        Some(url) => format!("🗓 [Your calendar]({url}) · keep this link private"),
        None => format!(
            "🗓 Your calendar: send !ical to {} in a private chat",
            selector
                .contact_url
                .as_deref()
                .map_or_else(|| "me".to_owned(), |url| format!("[me]({url})"))
        ),
    });
    lines.push("!mygroups reopens this · !join / !leave · !help".into());
    lines.join("\n")
}

/// The last tap's confirmation (or why it did nothing) — until the
/// selector owner's memberships change some other way (`!join`, an admin,
/// another selector), when it would only confuse.
fn current_feedback<'a>(state: &State, selector: &'a GroupSelector) -> Option<&'a str> {
    let feedback = selector.feedback.as_deref()?;
    let Some(since) = selector.feedback_at else {
        return Some(feedback);
    };
    let person_id = state.person_by_matrix_id(&selector.user_id).map(|p| &p.id);
    let changed = state.event_log.iter().skip(since).any(|e| match &e.event {
        DomainEvent::PersonJoinedGroup { person_id: p, .. }
        | DomainEvent::PersonLeftGroup { person_id: p, .. } => Some(p) == person_id,
        _ => false,
    });
    (!changed).then_some(feedback)
}

/// A reaction `key` by `sender` on the selector `selector_id`: toggle that
/// group. Returns the confirmation to reply with, or `None` when the
/// reaction is no tap (someone else's, another emoji, seen or taken back before).
pub fn tap(
    ctx: &BotContext,
    state: &mut State,
    selector_id: &str,
    reaction_id: &str,
    sender: &str,
    key: &str,
) -> Result<Option<String>> {
    let Some(selector) = state.group_selectors.get(selector_id) else {
        return Ok(None);
    };
    if selector.user_id != sender
        || selector.taps.contains_key(reaction_id)
        || state.redacted_reactions.contains(reaction_id)
    {
        return Ok(None);
    }
    let Some(group_id) = number_index(key).and_then(|i| selector.group_ids.get(i).cloned()) else {
        return Ok(None);
    };
    let joined = state
        .person_by_matrix_id(sender)
        .is_some_and(|p| state.is_member(&group_id, &p.id));
    let (effect, reply) = if state.group_by_id(&group_id).is_none() {
        (TapEffect::Nothing, None)
    } else if joined {
        toggle(ctx, state, sender, &group_id, false)?
    } else {
        toggle(ctx, state, sender, &group_id, true)?
    };
    let applied_at_event = Some(state.event_log.len());
    if let Some(selector) = state.group_selectors.get_mut(selector_id) {
        selector.feedback = reply.clone();
        selector.feedback_at = applied_at_event;
        selector.taps.insert(
            reaction_id.to_owned(),
            SelectorTap {
                group_id,
                effect,
                applied_at_event,
                undone: false,
            },
        );
    }
    Ok(reply)
}

/// A redacted reaction: if it was a tap, undo what it did — as long as
/// that still applies (they may have left by command since). Returns the
/// selector's event ID and the confirmation to reply with.
pub fn untap(
    ctx: &BotContext,
    state: &mut State,
    reaction_id: &str,
) -> Result<Option<(String, String)>> {
    let Some((selector_id, user_id, tap)) = state.group_selectors.iter_mut().find_map(|(id, s)| {
        let tap = s.taps.get_mut(reaction_id).filter(|t| !t.undone)?;
        tap.undone = true;
        Some((id.clone(), s.user_id.clone(), tap.clone()))
    }) else {
        return Ok(None);
    };
    if let Some(index) = tap.applied_at_event {
        let pid = state.person_by_matrix_id(&user_id).map(|p| &p.id);
        if state.event_log.iter().skip(index).any(|e| match &e.event {
            DomainEvent::PersonJoinedGroup {
                person_id,
                group_id,
            }
            | DomainEvent::PersonLeftGroup {
                person_id,
                group_id,
            } => Some(person_id) == pid && *group_id == tap.group_id,
            _ => false,
        }) {
            return Ok(None);
        }
    }
    let joined = state
        .person_by_matrix_id(&user_id)
        .is_some_and(|p| state.is_member(&tap.group_id, &p.id));
    let reply = match tap.effect {
        TapEffect::Joined if joined => toggle(ctx, state, &user_id, &tap.group_id, false)?.1,
        TapEffect::Left if !joined && state.group_by_id(&tap.group_id).is_some() => {
            toggle(ctx, state, &user_id, &tap.group_id, true)?.1
        }
        _ => None,
    };
    let at = state.event_log.len();
    if let Some(s) = state.group_selectors.get_mut(&selector_id) {
        s.feedback = reply.clone();
        s.feedback_at = Some(at);
    }
    Ok(reply.map(|r| (selector_id, r)))
}

/// Join or leave, with the short confirmation (or why not).
fn toggle(
    ctx: &BotContext,
    state: &mut State,
    mxid: &str,
    group_id: &crate::domain::GroupId,
    join: bool,
) -> Result<(TapEffect, Option<String>)> {
    let outcome = if join {
        join_group(ctx, state, mxid, group_id)?
    } else {
        leave_group(ctx, state, mxid, group_id)?
    };
    let group = group_name_of(state, group_id);
    Ok(match (outcome, join) {
        (Ok(_summary), true) => (TapEffect::Joined, Some(format!("✅ Joined **{group}**"))),
        (Ok(_summary), false) => (TapEffect::Left, Some(format!("👋 Left **{group}**"))),
        (Err(why), _) => (TapEffect::Nothing, Some(why)),
    })
}

/// Which number a reaction key is — with or without the emoji variation
/// selector some clients leave out.
fn number_index(key: &str) -> Option<usize> {
    let bare = |s: &str| s.replace('\u{fe0f}', "");
    NUMBERS.iter().position(|n| bare(n) == bare(key))
}

/// Mark everyone as welcomed who has used the bot before welcomes were
/// tracked per user — the old greeting's list, plus anyone who has done,
/// swapped or taken over a turn, reacted to a plan or made a calendar
/// feed. Members who were only added or linked by an admin still get
/// theirs. Runs once; returns whether anything changed.
pub fn migrate_welcomes(state: &mut State) -> bool {
    if state.welcomes_migrated {
        return false;
    }
    let mut active: std::collections::HashSet<&str> = std::collections::HashSet::new();
    active.extend(state.completions.iter().map(|c| c.completed_by_id.as_str()));
    active.extend(
        state
            .reaction_dones
            .values()
            .map(|r| r.completed_by_id.as_str()),
    );
    active.extend(state.calendar_tokens.iter().map(|t| t.person_id.as_str()));
    let mut welcomed: Vec<String> = state
        .persons
        .iter()
        .filter(|p| active.contains(p.id.as_str()))
        .filter_map(|p| p.matrix_id.clone())
        .collect();
    // Who assigned, took over or accepted a turn (by Matrix ID).
    welcomed.extend(state.event_log.iter().filter_map(|e| match &e.event {
        DomainEvent::SlotAssigned { actor_id, .. } => actor_id.clone(),
        _ => None,
    }));
    welcomed.extend(
        state
            .swap_requests
            .iter()
            .flat_map(|s| [s.requester.clone(), s.target.clone()]),
    );
    state.greeted_users.extend(welcomed);
    state.welcomes_migrated = true;
    true
}

// ── Matrix ────────────────────────────────────────────────────────────────────

/// Welcome `user_id` if they haven't had it yet.
pub async fn welcome_if_new(ctx: &BotContext, room: &Room, user_id: &OwnedUserId) {
    if room.room_id() != ctx.room_id {
        return;
    }
    if ctx
        .state
        .lock()
        .await
        .greeted_users
        .contains(user_id.as_str())
    {
        return;
    }
    for candidate in room.client().joined_rooms() {
        if candidate.room_id() != ctx.room_id
            && candidate
                .direct_targets()
                .iter()
                .any(|t| t.as_user_id() == Some(user_id.as_ref()))
            && crate::private::authorized(ctx, &candidate, user_id).await
        {
            if let Err(e) = post_selector(ctx, &candidate, user_id.as_str(), true).await {
                tracing::error!("Private welcome failed: {e}");
            }
            return;
        }
    }
    if let Err(e) = post_selector(ctx, room, user_id.as_str(), true).await {
        tracing::error!("Failed to welcome {user_id}: {e}");
    }
}

/// Post a selector for `user_id` in `room` and seed the number reactions to
/// tap. It replaces the user's previous selector in that room.
pub async fn post_selector(
    ctx: &BotContext,
    room: &Room,
    user_id: &str,
    welcome: bool,
) -> Result<()> {
    if room.room_id() != ctx.room_id {
        let user = OwnedUserId::try_from(user_id)?;
        anyhow::ensure!(
            crate::private::authorized(ctx, room, &user).await,
            "Private room not authorized"
        );
    }
    let private = room.room_id() != ctx.room_id;
    let pending_key = format!("{}|{user_id}", room.room_id());
    let mut state = ctx.state.lock().await;
    let first_welcome = welcome && !state.greeted_users.contains(user_id);
    let pending = state
        .pending_welcomes
        .get(&pending_key)
        .filter(|_| first_welcome)
        .cloned();
    let selector = match pending {
        // A welcome that may have gone out before a crash: send exactly it
        // again. Its transaction ID makes the server hand back the event
        // already sent, whose text then matches `rendered`.
        Some(pending) => pending,
        None => {
            let mut selector = new_selector(&state, user_id, welcome);
            selector.room_id = room.room_id().to_string();
            selector.contact_url =
                Some(format!("https://matrix.to/#/{}", ctx.config.matrix.user_id));
            // The feed link only ever goes into a verified private chat.
            if let (true, Some(cfg)) = (private, &ctx.config.ical_server) {
                state.apply_event(DomainEvent::PersonCreated {
                    person_id: uuid::Uuid::new_v4().to_string(),
                    display_name: user_id.into(),
                    matrix_id: Some(user_id.into()),
                })?;
                let person_id = state
                    .person_by_matrix_id(user_id)
                    .map(|p| p.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("{user_id} has no person record"))?;
                let token = crate::private::calendar_token(&mut state, &person_id);
                selector.calendar_url = Some(crate::private::feed_url(cfg, &token));
            }
            selector.rendered = selector_text(&state, &selector);
            if first_welcome {
                state
                    .pending_welcomes
                    .insert(pending_key.clone(), selector.clone());
            }
            selector
        }
    };
    state.save(&ctx.state_path).await?;
    drop(state);
    let content = format::intentional(format::mentionify(&selector.rendered));
    let event_id = if first_welcome {
        let txn = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, pending_key.as_bytes());
        room.send(content)
            .with_transaction_id(format!("welcome-{txn}").into())
            .await?
            .response
            .event_id
    } else {
        room.send(content).await?.response.event_id
    };
    let numbers = selector.group_ids.len();
    {
        let mut state = ctx.state.lock().await;
        state.greeted_users.insert(user_id.to_owned());
        state.pending_welcomes.remove(&pending_key);
        state
            .group_selectors
            .retain(|_, s| s.user_id != user_id || !in_room(s, room.room_id(), ctx));
        state.group_selectors.insert(event_id.to_string(), selector);
        state.save(&ctx.state_path).await?;
    }
    for number in &NUMBERS[..numbers] {
        let reaction =
            ReactionEventContent::new(Annotation::new(event_id.clone(), (*number).to_owned()));
        if let Err(e) = room.send(reaction).await {
            tracing::warn!("Failed to seed {number} on a group selector: {e}");
        }
    }
    Ok(())
}

/// Take a tap's reaction away once it is saved, so the number can be
/// tapped again (and toggles back). The redaction is the bot's, so it is
/// no undo. Without the power to redact others' events the reaction just
/// stays: the tap is recorded by its ID and counts once anyway, and
/// removing it by hand undoes it.
pub async fn consume_tap(room: &Room, reaction_id: &OwnedEventId, bot: &OwnedUserId) {
    let allowed = room
        .power_levels()
        .await
        .is_ok_and(|levels| levels.user_can_redact_event_of_other(bot));
    if !allowed {
        tracing::debug!(
            "No power to redact taps in {}; leaving them",
            room.room_id()
        );
        return;
    }
    if let Err(e) = room.redact(reaction_id, None, None).await {
        tracing::warn!("Failed to clear a group selector tap: {e}");
    }
}

/// Whether `selector` stands in `room`. Selectors from before they knew
/// their room are all in the cleaning room.
pub fn in_room(selector: &GroupSelector, room_id: &RoomId, ctx: &BotContext) -> bool {
    if selector.room_id.is_empty() {
        room_id == ctx.room_id
    } else {
        selector.room_id == room_id.as_str()
    }
}

/// `refresh_selectors` in every room that has one — after a change made
/// in one room (a private chat, say) that selectors elsewhere show.
pub async fn refresh_all_selectors(ctx: &BotContext, client: &mxbot_common::matrix_sdk::Client) {
    let rooms: std::collections::BTreeSet<String> = ctx
        .state
        .lock()
        .await
        .group_selectors
        .values()
        .map(|s| {
            if s.room_id.is_empty() {
                ctx.room_id.to_string()
            } else {
                s.room_id.clone()
            }
        })
        .collect();
    for room_id in rooms {
        let Ok(room_id) = mxbot_common::matrix_sdk::ruma::OwnedRoomId::try_from(room_id) else {
            continue;
        };
        if let Some(room) = client.get_room(&room_id) {
            refresh_selectors(ctx, &room).await;
        }
    }
}

/// Edit every selector whose text no longer matches the state — after a
/// tap, any membership command, and on startup. Unchanged ones are left
/// alone; an edit never notifies anyone again.
pub async fn refresh_selectors(ctx: &BotContext, room: &Room) {
    let stale: Vec<(String, GroupSelector, String)> = {
        let state = ctx.state.lock().await;
        if !state
            .group_selectors
            .values()
            .any(|s| in_room(s, room.room_id(), ctx))
        {
            return;
        }
        let schedule = crate::schedule::build_schedule(&state, 52);
        state
            .group_selectors
            .iter()
            .filter(|(_, s)| in_room(s, room.room_id(), ctx))
            .filter_map(|(id, s)| {
                let text = selector_text_with(&state, s, &schedule);
                (text != s.rendered).then(|| (id.clone(), s.clone(), text))
            })
            .collect()
    };
    for (id, selector, text) in stale {
        if room.room_id() != ctx.room_id {
            let Ok(user) = OwnedUserId::try_from(selector.user_id.as_str()) else {
                continue;
            };
            if !crate::private::authorized(ctx, room, &user).await {
                continue;
            }
        }
        let Ok(event_id) = OwnedEventId::try_from(id.as_str()) else {
            continue;
        };
        let already_mentioned = if selector.welcome {
            OwnedUserId::try_from(selector.user_id.as_str())
                .map(|u| Mentions::with_user_ids([u]))
                .unwrap_or_default()
        } else {
            Mentions::new()
        };
        let edit: RoomMessageEventContent = format::quiet(format::mentionify(&text))
            .make_replacement(ReplacementMetadata::new(event_id, Some(already_mentioned)));
        match room.send(edit).await {
            Ok(_) => {
                let mut state = ctx.state.lock().await;
                if let Some(s) = state.group_selectors.get_mut(&id) {
                    s.rendered = text;
                }
                if let Err(e) = state.save(&ctx.state_path).await {
                    tracing::error!("Failed to save a refreshed group selector: {e}");
                }
            }
            Err(e) => tracing::warn!("Failed to refresh group selector {id}: {e}"),
        }
    }
}
