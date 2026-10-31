//! Small private-chat boundary. Never authorize from names or m.direct hints.
use crate::BotContext;
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
    let accounts = ctx.recognised(sender);
    if accounts.is_empty() {
        return private::is_private_chat_of_member(&client, room.room_id(), &ctx.room_id, sender)
            .await;
    }
    for account in &accounts {
        if private::is_private_chat_of_member(&client, room.room_id(), &ctx.room_id, account).await
        {
            return true;
        }
    }
    false
}
