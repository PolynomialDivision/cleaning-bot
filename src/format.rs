//! Message formatting. Mention pills and markup are shared fleet-wide via
//! `mxbot_common::format`.
//!
//! Two ways to show a Matrix user, both rendered as a user pill:
//!   * `@user:server` (`view::mention`) — a pill *and* an `m.mentions`
//!     entry: notifies them. For messages addressed to someone (the weekly
//!     plan, reminders, swap requests).
//!   * `[name](https://matrix.to/#/@user:server)` (`view::user_link`) — the
//!     same pill, no `m.mentions` entry: notifies nobody. For read-only views.

use mxbot_common::matrix_sdk::ruma::events::{room::message::RoomMessageEventContent, Mentions};

pub use mxbot_common::format::{
    extract_mxids, fetch_names, mentionify, mentionify_rich, mentionify_with_names,
};

/// Notify exactly who `content` mentions — nobody when it has no `@mxid`
/// pills. `m.mentions` is always set, even empty: an event without it falls
/// back to the legacy push rules, which highlight anyone whose display name
/// or username merely appears in the body — the names in a plan, say.
pub fn intentional(mut content: RoomMessageEventContent) -> RoomMessageEventContent {
    content.mentions.get_or_insert_with(Mentions::new);
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mxid_pills_notify_and_m_mentions_is_always_set() {
        let mention = intentional(mentionify("⬜ @mia:example.org"));
        let ids: Vec<String> = mention
            .mentions
            .unwrap()
            .user_ids
            .iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(ids, ["@mia:example.org"]);

        let link = intentional(mentionify(
            "⬜ [mia](https://matrix.to/#/@mia:example.org) · Dan",
        ));
        assert!(link.mentions.as_ref().unwrap().user_ids.is_empty());
        let json = serde_json::to_value(&link).unwrap();
        assert_eq!(json["m.mentions"], serde_json::json!({}));
        assert_eq!(json["body"], "⬜ mia · Dan");
        assert_eq!(
            json["formatted_body"],
            r#"⬜ <a href="https://matrix.to/#/@mia:example.org">mia</a> · Dan"#
        );
    }
}
