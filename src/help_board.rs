//! The help board: `!help post` (admin) puts a friendly overview in the
//! cleaning room, with reactions that do things for whoever taps them —
//! their next turns, the plan, their groups, a PDF, or a private chat.
//!
//! A tap is answered privately when there's a verified private chat with
//! that person (`private`), else as a reply to the board, addressed to them.
//! Like a group selector tap, the reaction is then taken away again, so the
//! board keeps its clean row of buttons and the same one works next time.
//!
//! 💬 makes the bot invite the person to a new encrypted chat: the bot
//! invites rather than waits to be invited, so it needs no invite allowlist
//! for it. Once they join, it greets them there with their group selector
//! and — with calendar feeds set up — their calendar link.

use anyhow::Result;
use mxbot_common::matrix_sdk::{
    ruma::{
        events::{
            reaction::ReactionEventContent,
            relation::{Annotation, Reply},
            room::message::{Relation, RoomMessageEventContent},
        },
        OwnedEventId, OwnedRoomId, OwnedUserId, UserId,
    },
    Client, Room,
};

use crate::{format, BotContext};

/// What a reaction on the board asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Next,
    Plan,
    Groups,
    Pdf,
    PrivateChat,
}

/// The board's buttons, in order.
pub const BUTTONS: [(&str, Action); 5] = [
    ("📅", Action::Next),
    ("📋", Action::Plan),
    ("👥", Action::Groups),
    ("📄", Action::Pdf),
    ("💬", Action::PrivateChat),
];

/// The action of reaction `key` — with or without the emoji variation
/// selector some clients leave out.
pub fn action_for(key: &str) -> Option<Action> {
    let bare = |s: &str| s.replace('\u{fe0f}', "");
    BUTTONS
        .iter()
        .find(|(emoji, _)| bare(emoji) == bare(key))
        .map(|(_, action)| *action)
}

/// The board, a little playful.
pub fn board_text() -> String {
    "🤖 **Bip bup bup! I'm the cleaning bot** 🧹\n\
     I keep track of whose turn it is, give you a nudge when it's yours, \
     and do a tiny happy dance when it's done ✨\n\
     \n\
     📋 Every Monday the plan gets pinned up top — react ✅ on it when your part is done.\n\
     ⏰ When your turn starts or is about to run out, I'll send a short reminder.\n\
     🆘 Can't make it? React 🆘 on the plan — someone can take your turn or swap theirs for it.\n\
     \n\
     **Tap a button below and I'll do it for you:**\n\
     📅 your next turns — swap one, or say you can't make it\n\
     📋 the plan for the next weeks\n\
     👥 join or leave groups\n\
     📄 a printable plan (PDF)\n\
     💬 a private chat with me\n\
     \n\
     Rather type? !next · !plan · !mygroups · !sos · !help"
        .to_owned()
}

/// Post a board in the cleaning room and seed its buttons.
pub async fn post(ctx: &BotContext, room: &Room) -> Result<OwnedEventId> {
    let content = format::intentional(format::mentionify(&board_text()));
    let event_id = room.send(content).await?.response.event_id;
    {
        let mut state = ctx.state.lock().await;
        state.help_boards.insert(event_id.to_string());
        state.save(&ctx.state_path).await?;
    }
    for (emoji, _) in BUTTONS {
        let reaction = ReactionEventContent::new(Annotation::new(event_id.clone(), emoji.into()));
        if let Err(e) = room.send(reaction).await {
            tracing::warn!("Failed to seed {emoji} on the help board: {e}");
        }
    }
    Ok(event_id)
}

/// `user` tapped `action` on `board` (in the cleaning room `room`).
pub async fn run(
    ctx: &BotContext,
    client: &Client,
    room: &Room,
    board: &OwnedEventId,
    user: &OwnedUserId,
    action: Action,
) {
    if action == Action::PrivateChat {
        if let Err(e) = open_private_chat(ctx, client, user).await {
            tracing::warn!("Could not open a private chat with {user}: {e}");
            answer(
                room,
                board,
                user,
                "😕 I couldn't start a private chat with you — try again later, or invite me yourself.",
            )
            .await;
        }
        return;
    }
    let private = private_chat(ctx, client, user).await;
    let target = private.as_ref().unwrap_or(room);
    let result: Result<Option<String>> = match action {
        Action::Next => {
            // The menu of their turns — the plain list when they have none.
            let relation = private
                .is_none()
                .then(|| Relation::Reply(Reply::with_event_id(board.clone())));
            match crate::turn_menu::post(ctx, target, user, crate::turn_menu::DEFAULT, relation)
                .await
            {
                Ok(true) => Ok(None),
                Ok(false) => crate::commands::cmd_next(ctx, user, &[]).await,
                Err(e) => Err(e),
            }
        }
        Action::Plan => Ok(Some(crate::commands::plan_text(
            &*ctx.state.lock().await,
            6,
        ))),
        Action::Groups => crate::onboarding::post_selector(ctx, target, user.as_str(), false)
            .await
            .map(|()| None),
        Action::Pdf => {
            let answer_to = private
                .is_none()
                .then(|| Relation::Reply(Reply::with_event_id(board.clone())));
            crate::commands::cmd_pdf(ctx, user, target, &[], answer_to)
                .await
                // A reply only when it went wrong; the file speaks for itself.
                .map(|reply| reply.map(|c| c.body().to_owned()))
        }
        Action::PrivateChat => unreachable!("handled above"),
    };
    let text = match result {
        Ok(Some(text)) => text,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("Help board {action:?} for {user} failed: {e}");
            "😕 That didn't work — try the command instead.".to_owned()
        }
    };
    match private {
        Some(chat) => {
            let content = format::intentional(format::mentionify(&text));
            if let Err(e) = chat.send(content).await {
                tracing::warn!("Failed to answer {user} privately: {e}");
            }
        }
        None => answer(room, board, user, &text).await,
    }
}

async fn answer(room: &Room, board: &OwnedEventId, user: &UserId, text: &str) {
    if let Err(e) = room.send(answer_content(board, user, text)).await {
        tracing::warn!("Failed to answer a help board tap: {e}");
    }
}

/// `text` for `user`, as a reply to the board — addressed to them, so they
/// see it's theirs.
fn answer_content(board: &OwnedEventId, user: &UserId, text: &str) -> RoomMessageEventContent {
    let mut content = format::intentional(format::mentionify(&format!("{user}\n{text}")));
    content.relates_to = Some(Relation::Reply(Reply::with_event_id(board.clone())));
    content
}

/// The verified private chat with `user`, if there is one: one the bot
/// opened for them, or another direct chat — either only if it passes the
/// `private` check right now.
pub async fn private_chat(ctx: &BotContext, client: &Client, user: &OwnedUserId) -> Option<Room> {
    let known = ctx
        .state
        .lock()
        .await
        .private_chats
        .get(user.as_str())
        .and_then(|chat| OwnedRoomId::try_from(chat.room_id.as_str()).ok())
        .and_then(|id| client.get_room(&id));
    for room in known.into_iter().chain(client.get_dm_rooms(user)) {
        if crate::private::authorized(ctx, &room, user).await {
            return Some(room);
        }
    }
    None
}

/// Say hello in the private chat with `user` — or, without one yet, invite
/// them to a new encrypted one (greeted once they join, `greet_on_join`).
async fn open_private_chat(ctx: &BotContext, client: &Client, user: &OwnedUserId) -> Result<()> {
    if let Some(chat) = private_chat(ctx, client, user).await {
        let content = format::intentional(format::mentionify(
            "👋 Here I am! Ask me anything you'd ask in the cleaning room — !next · !plan · !mygroups · !done · !ical",
        ));
        chat.send(content).await?;
        return Ok(());
    }
    // Still waiting for them to accept an earlier invite: invite again
    // rather than open a second chat.
    let pending = ctx
        .state
        .lock()
        .await
        .private_chats
        .get(user.as_str())
        .filter(|chat| !chat.greeted)
        .and_then(|chat| OwnedRoomId::try_from(chat.room_id.as_str()).ok())
        .and_then(|id| client.get_room(&id));
    if let Some(room) = pending {
        if room.invite_user_by_id(user).await.is_ok() {
            return Ok(());
        }
    }
    let room = client.create_dm(user).await?;
    let mut state = ctx.state.lock().await;
    state.private_chats.insert(
        user.to_string(),
        crate::state::PrivateChat {
            room_id: room.room_id().to_string(),
            greeted: false,
        },
    );
    state.save(&ctx.state_path).await?;
    Ok(())
}

/// `user` joined `room`: if it's the private chat the bot opened for them,
/// greet them there — once.
pub async fn greet_on_join(ctx: &BotContext, room: &Room, user: &OwnedUserId) {
    let ours = ctx
        .state
        .lock()
        .await
        .private_chats
        .get(user.as_str())
        .is_some_and(|chat| chat.room_id == room.room_id().as_str() && !chat.greeted);
    if !ours || !crate::private::authorized(ctx, room, user).await {
        return;
    }
    {
        let mut state = ctx.state.lock().await;
        if let Some(chat) = state.private_chats.get_mut(user.as_str()) {
            chat.greeted = true;
        }
        if let Err(e) = state.save(&ctx.state_path).await {
            tracing::error!("Failed to save the private chat with {user}: {e}");
            return;
        }
    }
    let hello = format::intentional(format::mentionify(&private_hello()));
    if let Err(e) = room.send(hello).await {
        tracing::warn!("Failed to greet {user} privately: {e}");
        return;
    }
    if let Err(e) = crate::onboarding::post_selector(ctx, room, user.as_str(), false).await {
        tracing::warn!("Failed to post {user}'s private group selector: {e}");
    }
}

fn private_hello() -> String {
    "👋 Psst — it's just us here!\n\
     Ask me what you'd ask in the cleaning room, nobody else sees it:\n\
     📅 !next · 📋 !plan · ✅ !done · 🗓 !ical · ❓ !help\n\
     Your groups (and your calendar link) come right below 👇"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_button_is_on_the_board_and_found_with_or_without_fe0f() {
        let board = board_text();
        for (emoji, action) in BUTTONS {
            assert!(board.contains(&format!("\n{emoji} ")), "{emoji} missing");
            assert_eq!(action_for(emoji), Some(action));
            assert_eq!(action_for(&format!("{emoji}\u{fe0f}")), Some(action));
        }
        assert_eq!(action_for("✅"), None);
        // The board pings nobody.
        let content = format::intentional(format::mentionify(&board));
        assert!(content.mentions.unwrap().user_ids.is_empty());
    }

    #[test]
    fn an_answer_on_the_board_is_addressed_to_whoever_tapped() {
        use mxbot_common::matrix_sdk::ruma::{owned_event_id, user_id};
        let board = owned_event_id!("$board");
        let content = answer_content(&board, user_id!("@mia:example.org"), "📅 Your next turns");
        assert!(matches!(
            &content.relates_to,
            Some(Relation::Reply(reply)) if reply.in_reply_to.event_id == board
        ));
        let ids: Vec<String> = content
            .mentions
            .unwrap()
            .user_ids
            .iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(ids, ["@mia:example.org"]);
    }
}
