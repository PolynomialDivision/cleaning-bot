//! Small private-chat boundary. Never authorize from names or m.direct hints.
use crate::{
    domain::{new_calendar_token, CalendarToken},
    state::State,
    BotContext,
};
use mxbot_common::matrix_sdk::{
    ruma::{api::client::state::get_state_events, OwnedUserId},
    Room, RoomState,
};
use serde_json::Value;

pub(crate) fn private_members(events: &[Value], bot: &str, user: &str) -> bool {
    let active: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "m.room.member")
        .filter(|e| {
            matches!(
                e["content"]["membership"].as_str(),
                Some("join" | "invite" | "knock")
            )
        })
        .collect();
    bot != user
        && active.len() == 2
        && [bot, user].iter().all(|id| {
            active
                .iter()
                .any(|e| e["state_key"] == *id && e["content"]["membership"] == "join")
        })
        && events.iter().any(|e| {
            e["type"] == "m.room.encryption" && e["content"]["algorithm"] == "m.megolm.v1.aes-sha2"
        })
}

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
    let Some(main) = client.get_room(&ctx.room_id) else {
        return false;
    };
    if main.state() != RoomState::Joined {
        return false;
    }
    let Ok(trusted) = client
        .send(get_state_events::v3::Request::new(ctx.room_id.clone()))
        .await
    else {
        return false;
    };
    let joined = |id: &str| {
        trusted.room_state.iter().any(|e| {
            let Ok(v) = e.deserialize_as::<Value>() else {
                return false;
            };
            v["type"] == "m.room.member"
                && v["state_key"] == id
                && v["content"]["membership"] == "join"
        })
    };
    if !joined(sender.as_str()) || !joined(bot.as_str()) {
        return false;
    }
    let Ok(private) = client
        .send(get_state_events::v3::Request::new(
            room.room_id().to_owned(),
        ))
        .await
    else {
        return false;
    };
    let events = private
        .room_state
        .iter()
        .filter_map(|e| e.deserialize_as::<Value>().ok())
        .collect::<Vec<_>>();
    private_members(&events, bot.as_str(), sender.as_str())
}

/// The public feed URL for a raw token.
pub fn feed_url(cfg: &crate::config::ICalServerConfig, token: &str) -> String {
    format!("{}/ical/{token}.ics", cfg.public_url.trim_end_matches('/'))
}

/// Recoverable random bearer token. Old hash-only subscriptions stay valid.
/// State and backups must be private; never log tokens or place them in the main room.
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
    use serde_json::json;
    fn member(id: &str, membership: &str) -> Value {
        json!({"type":"m.room.member", "state_key":id, "content":{"membership":membership}})
    }
    #[test]
    fn only_two_joined_members_and_encryption_are_private() {
        let mut events = vec![
            member("@bot:x", "join"),
            member("@a:x", "join"),
            json!({"type":"m.room.encryption","content":{"algorithm":"m.megolm.v1.aes-sha2"}}),
        ];
        assert!(private_members(&events, "@bot:x", "@a:x"));
        assert!(!private_members(&events, "@bot:x", "@other:x"));
        for membership in ["invite", "join", "knock"] {
            events.push(member("@third:x", membership));
            assert!(!private_members(&events, "@bot:x", "@a:x"));
            events.pop();
        }
        events.pop();
        assert!(!private_members(&events, "@bot:x", "@a:x"));
    }
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
