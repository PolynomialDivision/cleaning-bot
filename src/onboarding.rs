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
//! leave if they are — so it works the same however they got in. Taking
//! the reaction back undoes exactly that tap (when it still applies). The
//! selector is edited to show the new state, and a short confirmation
//! replies to it. Every tap is recorded by its reaction event ID, so a
//! re-delivered event counts once, also across restarts.

use anyhow::Result;
use mxbot_common::{
    matrix_sdk::{
        ruma::{
            events::{
                reaction::ReactionEventContent,
                relation::Annotation,
                room::message::{ReplacementMetadata, RoomMessageEventContent},
                Mentions,
            },
            OwnedEventId, OwnedUserId,
        },
        Room,
    },
    send::in_thread,
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
/// 👋 Welcome, @mia! I'm the cleaning bot: I keep track of whose turn it
/// is and remind you when it's yours.
///
/// Tap a number to join or leave a group:
/// 1️⃣ ✅ **2nd Floor**
/// 2️⃣ 3+4 Floor
///
/// !mygroups brings this back anytime.
/// ```
pub fn selector_text(state: &State, selector: &GroupSelector) -> String {
    let person = state.person_by_matrix_id(&selector.user_id);
    let mut lines = Vec::new();
    let tap_line = if selector.welcome {
        // The welcome is addressed to them: a real mention.
        lines.push(format!(
            "👋 Welcome, {}! I'm the cleaning bot: I keep track of whose turn it is \
             and remind you when it's yours.",
            selector.user_id
        ));
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
    if choices.is_empty() {
        lines.push("There are no cleaning groups yet — an admin will set them up.".into());
    } else {
        lines.push(tap_line);
        lines.extend(choices);
    }
    if selector.welcome {
        lines.push(String::new());
        lines.push("!mygroups brings this back anytime.".into());
    }
    lines.join("\n")
}

/// A reaction `key` by `sender` on the selector `selector_id`: toggle that
/// group. Returns the confirmation to reply with, or `None` when the
/// reaction is no tap (someone else's, another emoji, seen before).
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
    if selector.user_id != sender || selector.taps.contains_key(reaction_id) {
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
    if let Some(selector) = state.group_selectors.get_mut(selector_id) {
        selector.taps.insert(
            reaction_id.to_owned(),
            SelectorTap {
                group_id,
                effect,
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
    let who = state
        .person_by_matrix_id(mxid)
        .map_or_else(|| view::user_id_link(mxid), view::user_link);
    let group = group_name_of(state, group_id);
    Ok(match (outcome, join) {
        (Ok(summary), true) => (
            TapEffect::Joined,
            Some(format!("✅ {who} joined **{group}**\n{summary}")),
        ),
        (Ok(summary), false) => (
            TapEffect::Left,
            Some(format!("👋 {who} left **{group}**\n{summary}")),
        ),
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
    {
        let mut state = ctx.state.lock().await;
        if !claim_welcome(&mut state, user_id.as_str()) {
            return;
        }
        if let Err(e) = state.save(&ctx.state_path).await {
            tracing::error!("Failed to save the welcome of {user_id}: {e}");
        }
    }
    if let Err(e) = post_selector(ctx, room, user_id.as_str(), true).await {
        tracing::error!("Failed to welcome {user_id}: {e}");
    }
}

/// Post a selector for `user_id` and seed the number reactions to tap. It
/// stands in the main timeline — taps are answered in its thread — and
/// replaces the user's previous selector.
pub async fn post_selector(
    ctx: &BotContext,
    room: &Room,
    user_id: &str,
    welcome: bool,
) -> Result<()> {
    let selector = new_selector(&*ctx.state.lock().await, user_id, welcome);
    let content = format::intentional(format::mentionify(&selector.rendered));
    let event_id = room.send(content).await?.response.event_id;
    let numbers = selector.group_ids.len();
    {
        let mut state = ctx.state.lock().await;
        state.group_selectors.retain(|_, s| s.user_id != user_id);
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

/// Answer a tap in the selector's thread.
pub async fn reply_to_tap(room: &Room, selector_id: &str, text: &str) {
    let Ok(selector) = OwnedEventId::try_from(selector_id) else {
        return;
    };
    let content = format::intentional(format::mentionify(text));
    if let Err(e) = room
        .send(in_thread(content, selector.clone(), selector))
        .await
    {
        tracing::warn!("Failed to confirm a group selector tap: {e}");
    }
}

/// Edit every selector whose text no longer matches the state — after a
/// tap, any membership command, and on startup. Unchanged ones are left
/// alone; an edit never notifies anyone again.
pub async fn refresh_selectors(ctx: &BotContext, room: &Room) {
    let stale: Vec<(String, GroupSelector, String)> = {
        let state = ctx.state.lock().await;
        state
            .group_selectors
            .iter()
            .filter_map(|(id, s)| {
                let text = selector_text(&state, s);
                (text != s.rendered).then(|| (id.clone(), s.clone(), text))
            })
            .collect()
    };
    for (id, selector, text) in stale {
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
        let edit: RoomMessageEventContent = format::intentional(format::mentionify(&text))
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
