//! Small private-chat boundary. Never authorize from names or m.direct hints.
use crate::{
    domain::{new_calendar_token, CalendarToken},
    state::State,
    BotContext,
};
use mxbot_common::{
    matrix_sdk::{ruma::OwnedUserId, Room, RoomState},
    private,
};

/// Fetch current state on every request. Fail closed on errors, third parties,
/// pending invitations, loss of trusted-room membership, or missing encryption.
pub async fn authorized(ctx: &BotContext, room: &Room, sender: &OwnedUserId) -> bool {
    if room.room_id() == ctx.room_id || room.state() != RoomState::Joined {
        return false;
    }
    let client = room.client();
    let Some(bot) = client.user_id() else {
        return false;
    };
    // The bot itself must still be in the cleaning room, asked of the server.
    if !matches!(
        private::is_joined_now(&client, &ctx.room_id, bot).await,
        Ok(true)
    ) {
        return false;
    }
    // `sender`, or another account of their resident (`directory`): the one in this chat.
    let linked = ctx.linked_accounts();
    let accounts = linked.linked(sender);
    if accounts.is_empty() {
        return private::is_private_chat_of_member(&client, room.room_id(), &ctx.room_id, sender)
            .await;
    }
    for account in accounts {
        if private::is_private_chat_of_member(&client, room.room_id(), &ctx.room_id, account).await
        {
            return true;
        }
    }
    false
}

/// The public feed URL for a raw token.
pub fn feed_url(cfg: &crate::config::ICalServerConfig, token: &str) -> String {
    format!("{}/ical/{token}.ics", cfg.public_url.trim_end_matches('/'))
}

/// Recoverable random bearer token. Old hash-only subscriptions stay valid.
/// Never logged. A token only grants that person's own turns, but anyone
/// with the link could follow them as that person: the link is shown only
/// in their private chat, never in the cleaning room (`onboarding`, `!ical`).
pub fn calendar_token(state: &mut State, person_id: &str) -> String {
    if let Some(raw) = state
        .calendar_tokens
        .iter()
        .find(|t| !t.revoked && t.person_id == person_id && t.raw_token.is_some())
        .and_then(|t| t.raw_token.clone())
    {
        return raw;
    }
    let (raw, hash) = new_calendar_token();
    state.calendar_tokens.push(CalendarToken {
        id: uuid::Uuid::new_v4().to_string(),
        raw_token: Some(raw.clone()),
        token_hash: hash,
        person_id: person_id.into(),
        created_at: chrono::Utc::now(),
        revoked: false,
    });
    raw
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_is_stable_through_restart_and_rotates_after_revocation() {
        let mut state = State::default();
        let first = calendar_token(&mut state, "alice");
        let mut state: State =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(calendar_token(&mut state, "alice"), first);
        assert_ne!(calendar_token(&mut state, "bob"), first);
        state.calendar_tokens[0].revoked = true;
        assert_ne!(calendar_token(&mut state, "alice"), first);
    }
}
