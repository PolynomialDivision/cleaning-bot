use std::collections::HashMap;

use matrix_sdk::{Room, ruma::OwnedUserId};
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

/// Scan `text` for Matrix user IDs (`@localpart:server`) and return a
/// `RoomMessageEventContent` with an HTML body where each ID is a clickable
/// mention pill showing the localpart.  Also converts `**bold**` markers to
/// `<strong>` and `\n` to `<br>`.  Falls back to plain text when no
/// HTML-specific transforms apply.
pub fn mentionify(text: &str) -> RoomMessageEventContent {
    build(text, |token| default_label(token).to_owned())
}

/// Like `mentionify`, but looks up display names from `names`
/// (key = full MXID, value = display name) so the pill shows the
/// friendly name instead of the localpart.
/// The plain-text body is also updated: `@user:server` → `Display Name`.
pub fn mentionify_with_names(text: &str, names: &HashMap<String, String>) -> RoomMessageEventContent {
    build(text, |token| {
        names
            .get(token)
            .cloned()
            .unwrap_or_else(|| default_label(token).to_owned())
    })
}

/// Scan `text` for Matrix user IDs, fetch their display names from room state,
/// and return a `RoomMessageEventContent` with display-name mention pills.
/// Equivalent to calling `extract_mxids` → `fetch_names` → `mentionify_with_names`.
pub async fn mentionify_rich(text: &str, room: &Room) -> RoomMessageEventContent {
    let mxids = extract_mxids(text);
    if mxids.is_empty() {
        return mentionify(text);
    }
    let refs: Vec<&str> = mxids.iter().map(String::as_str).collect();
    let names = fetch_names(room, &refs).await;
    mentionify_with_names(text, &names)
}

/// Extract all `@localpart:server` MXID tokens from `text`, deduplicating.
/// Used by `mentionify_rich` and the scheduler to find who appears in a message.
pub fn extract_mxids(text: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        if text.as_bytes()[pos] == b'@' {
            let token_len = text[pos..]
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(c, ',' | '!' | '?' | '*' | ')' | ']' | '"' | '\'')
                })
                .unwrap_or(text.len() - pos);
            let token = &text[pos..pos + token_len];
            if token.len() > 4 && token.contains(':') {
                let owned = token.to_owned();
                if !result.contains(&owned) {
                    result.push(owned);
                }
            }
            pos += token_len.max(1);
        } else {
            pos += text[pos..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
    }
    result
}

/// Fetch display names for the given user ID strings from room state.
/// Falls back to the localpart if the member record is not found.
pub async fn fetch_names(room: &Room, user_ids: &[&str]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for &uid_str in user_ids {
        if let Ok(uid) = OwnedUserId::try_from(uid_str) {
            if let Ok(Some(member)) = room.get_member(&uid).await {
                let name = member
                    .display_name()
                    .unwrap_or_else(|| member.user_id().localpart())
                    .to_owned();
                map.insert(uid_str.to_owned(), name);
            }
        }
    }
    map
}

// ── Internals ─────────────────────────────────────────────────────────────────

fn default_label(token: &str) -> &str {
    token
        .split(':')
        .next()
        .unwrap_or("")
        .trim_start_matches('@')
}

/// Build a `RoomMessageEventContent` by scanning `text` for:
///   • `@localpart:server` MXID tokens  → clickable mention pills in HTML
///   • `**text**` bold markers          → `<strong>` in HTML, stripped from plain
///   • `\n`                             → `<br>` in HTML
///
/// `label_for(mxid)` returns the display label for a given MXID.
fn build(text: &str, label_for: impl Fn(&str) -> String) -> RoomMessageEventContent {
    let mut plain   = String::with_capacity(text.len());
    let mut html    = String::with_capacity(text.len() * 2);
    let mut pos     = 0;
    let mut found   = false; // true when HTML output differs from plain
    let mut in_bold = false;

    while pos < text.len() {
        // ── **bold** markers ──────────────────────────────────────────────────
        if text.as_bytes().get(pos) == Some(&b'*')
            && text.as_bytes().get(pos + 1) == Some(&b'*')
        {
            if in_bold {
                html.push_str("</strong>");
            } else {
                html.push_str("<strong>");
            }
            in_bold = !in_bold;
            found   = true;
            pos    += 2;
            continue;
        }

        // ── @user:server MXID pills ───────────────────────────────────────────
        if text.as_bytes()[pos] == b'@' {
            let token_len = text[pos..]
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(c, ',' | '!' | '?' | '*' | ')' | ']' | '"' | '\'')
                })
                .unwrap_or(text.len() - pos);

            let token = &text[pos..pos + token_len];

            if token.len() > 4 && token.contains(':') {
                let label = label_for(token);
                plain.push_str(&label);
                html.push_str(&format!(
                    r#"<a href="https://matrix.to/#/{token}">{label}</a>"#
                ));
                found = true;
                pos += token_len;
                continue;
            }
        }

        // ── Regular character ─────────────────────────────────────────────────
        let ch = text[pos..].chars().next().unwrap();
        plain.push(ch);
        match ch {
            '&'  => html.push_str("&amp;"),
            '<'  => html.push_str("&lt;"),
            '>'  => html.push_str("&gt;"),
            '"'  => html.push_str("&quot;"),
            '\n' => { html.push_str("<br>"); found = true; }
            _    => html.push(ch),
        }
        pos += ch.len_utf8();
    }

    // Close any unclosed bold tag (shouldn't happen with well-formed input).
    if in_bold {
        html.push_str("</strong>");
    }

    if found {
        RoomMessageEventContent::text_html(plain, html)
    } else {
        RoomMessageEventContent::text_plain(text)
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::ruma::events::room::message::MessageType;
    use super::*;

    fn bodies(c: &RoomMessageEventContent) -> (String, Option<String>) {
        match &c.msgtype {
            MessageType::Text(t) => (
                t.body.clone(),
                t.formatted.as_ref().map(|f| f.body.clone()),
            ),
            _ => panic!("unexpected msgtype"),
        }
    }

    #[test]
    fn replaces_single_mxid() {
        let c = mentionify("Hello @alice:example.org!");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains(r#"href="https://matrix.to/#/@alice:example.org""#));
        assert!(html.contains(">alice<"));
    }

    #[test]
    fn replaces_multiple_mxids() {
        let c = mentionify("@a:x.org and @b:y.org");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains(">a<"));
        assert!(html.contains(">b<"));
    }

    #[test]
    fn no_mxid_no_special_returns_plain() {
        let c = mentionify("no mentions here");
        let (_, html) = bodies(&c);
        assert!(html.is_none());
    }

    #[test]
    fn escapes_html_outside_mxid() {
        let c = mentionify("x < y & @u:s.org");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains("&lt;"));
        assert!(html.contains("&amp;"));
    }

    #[test]
    fn newline_becomes_br() {
        let c = mentionify("line one\nline two");
        let (plain, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains("<br>"), "html={html}");
        assert!(plain.contains('\n'), "plain should keep newline");
    }

    #[test]
    fn bold_markers_become_strong() {
        let c = mentionify("Status: **Floor 1** done");
        let (plain, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains("<strong>Floor 1</strong>"), "html={html}");
        assert!(!plain.contains('*'), "plain={plain}");
        assert!(plain.contains("Floor 1"));
    }

    #[test]
    fn multiline_with_mxid() {
        let c = mentionify("Reminder\nResponsible: @alice:example.org");
        let (_, html) = bodies(&c);
        let html = html.expect("should have HTML body");
        assert!(html.contains("<br>"));
        assert!(html.contains(r#"href="https://matrix.to/#/@alice:example.org""#));
    }
}
