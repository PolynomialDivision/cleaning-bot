//! Matrix display names, resolved in one place.
//!
//! What a person with a Matrix ID is called, in this order:
//!   1. their display name as a member of the room at hand;
//!   2. the `displayname` of their global Matrix profile
//!      (`GET /_matrix/client/v3/profile/{userId}`) — also when the bot
//!      shares no room with them;
//!   3. the name stored on their `Person` (what 1 or 2 found last time, or
//!      the name an admin gave them);
//!   4. the localpart of their Matrix ID.
//!
//! 1 and 2 are asked by `refresh` and `refresh_one`, which store what they
//! find on the existing `Person` (never creating one), so plans and PDFs
//! render 3 without asking again. A profile lookup is never required: it is
//! bounded in time, its answer is cached — failures too — and a 403, 404,
//! network or federation error simply leaves 3 in place.
//! Message pills (`mentionify`) use 1, 3 and 4 and never ask a server.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use mxbot_common::format::sanitize_display_name;
use mxbot_common::matrix_sdk::{
    ruma::{events::room::message::RoomMessageEventContent, OwnedUserId, UserId},
    Client, Room,
};
use tracing::{debug, warn};

use crate::{state::State, BotContext};

/// How long a profile answer is trusted: a name, or that there is none
/// (also 403/404: the user is gone or their server won't say).
const PROFILE_KEPT: Duration = Duration::from_secs(6 * 60 * 60);
/// After a network or federation failure, try again this much later.
const PROFILE_RETRY: Duration = Duration::from_secs(15 * 60);
/// Longest wait for one profile.
const PROFILE_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for everyone's names (startup, PDF) and for one person's
/// (on every command).
pub const EVERYONE: Duration = Duration::from_secs(8);
pub const ONE: Duration = Duration::from_secs(3);

/// What a profile lookup found.
#[derive(Debug, PartialEq)]
enum Profile {
    Name(String),
    /// No name set, or 403/404.
    Nameless,
    /// Network, federation, timeout or a server error: ask again later.
    Failed,
}

/// A cached profile answer: when it expires, and the name if any.
type Answer = (Instant, Option<String>);
/// Profile answers by Matrix ID.
static PROFILES: LazyLock<Mutex<HashMap<String, Answer>>> = LazyLock::new(Default::default);

fn cached(mxid: &str) -> Option<Option<String>> {
    let profiles = PROFILES.lock().unwrap_or_else(|e| e.into_inner());
    profiles
        .get(mxid)
        .filter(|(until, _)| Instant::now() < *until)
        .map(|(_, name)| name.clone())
}

fn remember(mxid: &str, profile: &Profile) {
    let (kept, name) = match profile {
        Profile::Name(name) => (PROFILE_KEPT, Some(name.clone())),
        Profile::Nameless => (PROFILE_KEPT, None),
        Profile::Failed => (PROFILE_RETRY, None),
    };
    let mut profiles = PROFILES.lock().unwrap_or_else(|e| e.into_inner());
    profiles.insert(mxid.to_owned(), (Instant::now() + kept, name));
}

/// A display name worth keeping: cleaned up, and not just the Matrix ID.
fn usable(name: &str, mxid: &str) -> Option<String> {
    sanitize_display_name(name).filter(|n| n != mxid)
}

/// The global profile's display name of `user`.
async fn fetch_profile(client: &Client, user: &UserId) -> Profile {
    let account = client.account();
    match tokio::time::timeout(PROFILE_TIMEOUT, account.fetch_user_profile_of(user)).await {
        Ok(Ok(profile)) => profile
            .get("displayname")
            .and_then(|v| v.as_str())
            .and_then(|n| usable(n, user.as_str()))
            .map_or(Profile::Nameless, Profile::Name),
        Ok(Err(e)) => {
            let status = e.as_client_api_error().map(|e| e.status_code.as_u16());
            debug!("No Matrix profile for {user}: {e}");
            match status {
                Some(403 | 404) => Profile::Nameless,
                _ => Profile::Failed,
            }
        }
        Err(_) => {
            debug!("Matrix profile of {user}: timed out");
            Profile::Failed
        }
    }
}

/// `user`'s display name as a member of `room`, if they have one there.
async fn member_name(room: Option<&Room>, user: &UserId) -> Option<String> {
    let member = room?.get_member(user).await.ok()??;
    usable(member.display_name()?, user.as_str())
}

/// What Matrix calls each of `mxids` right now: their display name in
/// `room` (1), else their global profile's (2). Profiles are asked for all
/// at once and only until `budget` runs out; who has no answer by then is
/// left out (and asked again next time). IDs that don't parse are skipped.
pub async fn live(
    client: &Client,
    room: Option<&Room>,
    mxids: &[&str],
    budget: Duration,
) -> HashMap<String, String> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut names = HashMap::new();
    let mut lookups = tokio::task::JoinSet::new();
    for &raw in mxids {
        let Ok(user) = OwnedUserId::try_from(raw) else {
            continue;
        };
        if let Some(name) = member_name(room, &user).await {
            names.insert(raw.to_owned(), name);
            continue;
        }
        match cached(raw) {
            Some(Some(name)) => {
                names.insert(raw.to_owned(), name);
            }
            Some(None) => {}
            None => {
                let client = client.clone();
                lookups.spawn(async move {
                    let profile = fetch_profile(&client, &user).await;
                    (user, profile)
                });
            }
        }
    }
    while let Ok(Some(done)) = tokio::time::timeout_at(deadline, lookups.join_next()).await {
        let Ok((user, profile)) = done else {
            continue;
        };
        remember(user.as_str(), &profile);
        if let Profile::Name(name) = profile {
            names.insert(user.to_string(), name);
        }
    }
    // Dropping the set cancels lookups still running.
    names
}

/// Store `live` names on the people with those Matrix IDs. Only existing
/// people are updated — nobody is created. True if a name changed.
pub fn store(state: &mut State, live: &HashMap<String, String>) -> bool {
    let mut changed = false;
    for person in &mut state.persons {
        let Some(name) = person.matrix_id.as_deref().and_then(|m| live.get(m)) else {
            continue;
        };
        if *name != person.display_name {
            person.display_name = name.clone();
            changed = true;
        }
    }
    changed
}

/// What to call `mxid` when nothing live is known: the stored name (3),
/// else the localpart (4).
pub fn fallback(stored: Option<&str>, mxid: &str) -> String {
    stored
        .and_then(|n| usable(n, mxid))
        .unwrap_or_else(|| localpart(mxid).to_owned())
}

fn localpart(mxid: &str) -> &str {
    mxid.strip_prefix('@')
        .and_then(|s| s.split(':').next())
        .filter(|s| !s.is_empty())
        .unwrap_or(mxid)
}

/// Ask Matrix for `mxids`' names and store them; saves if one changed.
async fn update(
    ctx: &BotContext,
    client: &Client,
    room: Option<&Room>,
    mxids: &[&str],
    budget: Duration,
) {
    if mxids.is_empty() {
        return;
    }
    let found = live(client, room, mxids, budget).await;
    if found.is_empty() {
        return;
    }
    let mut state = ctx.state.lock().await;
    if store(&mut state, &found) {
        if let Err(e) = state.save(&ctx.state_path).await {
            warn!("Failed to save refreshed display names: {e}");
        }
    }
}

/// Refresh everyone with a Matrix ID — at startup and before a PDF. `room`
/// is where to look for member names first (the cleaning room).
pub async fn refresh(ctx: &BotContext, client: &Client, room: Option<&Room>) {
    let mxids: Vec<String> = {
        let state = ctx.state.lock().await;
        state
            .persons
            .iter()
            .filter_map(|p| p.matrix_id.clone())
            .collect()
    };
    let refs: Vec<&str> = mxids.iter().map(String::as_str).collect();
    update(ctx, client, room, &refs, EVERYONE).await;
}

/// Refresh one person (who just sent a command in `room`), if they are one.
pub async fn refresh_one(ctx: &BotContext, room: &Room, mxid: &str) {
    if ctx.state.lock().await.person_by_matrix_id(mxid).is_none() {
        return;
    }
    update(ctx, &room.client(), Some(room), &[mxid], ONE).await;
}

/// What to show for each of `mxids` in `room` without asking a server: the
/// member name there (1), else the stored name (3, which holds what a
/// profile said), else the localpart (4).
pub async fn labels(ctx: &BotContext, room: &Room, mxids: &[String]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let mut unknown = Vec::new();
    for mxid in mxids {
        let member = match OwnedUserId::try_from(mxid.as_str()) {
            Ok(user) => member_name(Some(room), &user).await,
            Err(_) => None,
        };
        match member {
            Some(name) => {
                names.insert(mxid.clone(), name);
            }
            None => unknown.push(mxid),
        }
    }
    if !unknown.is_empty() {
        let state = ctx.state.lock().await;
        for mxid in unknown {
            let stored = state
                .person_by_matrix_id(mxid)
                .map(|p| p.display_name.as_str());
            names.insert(mxid.clone(), fallback(stored, mxid));
        }
    }
    names
}

/// `text` with its `@user:server` mentions as pills named by `labels`.
/// Must not be called while holding the state lock.
pub async fn mentionify(ctx: &BotContext, text: &str, room: &Room) -> RoomMessageEventContent {
    let mxids = crate::format::extract_mxids(text);
    let names = labels(ctx, room, &mxids).await;
    crate::format::mentionify_with_names(text, &names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Person;

    #[test]
    fn stored_names_are_updated_never_created() {
        let mut state = State::default();
        let mut alice = Person::new_matrix("@alice:example.org");
        alice.display_name = "alice".into();
        state.persons.push(alice);
        state.persons.push(Person::new_named("Bob"));
        let live: HashMap<String, String> = [
            ("@alice:example.org".to_owned(), "Alice A.".to_owned()),
            // Not a known person: no new record.
            ("@stranger:example.org".to_owned(), "Stranger".to_owned()),
        ]
        .into();
        assert!(store(&mut state, &live));
        assert_eq!(state.persons.len(), 2);
        assert_eq!(state.persons[0].display_name, "Alice A.");
        assert_eq!(state.persons[1].display_name, "Bob");
        // Same names again: nothing to save.
        assert!(!store(&mut state, &live));
    }

    #[test]
    fn without_a_live_name_the_stored_one_then_the_localpart() {
        let mxid = "@mia:example.org";
        assert_eq!(fallback(Some("Mia M."), mxid), "Mia M.");
        assert_eq!(fallback(Some("  "), mxid), "mia");
        assert_eq!(fallback(Some(mxid), mxid), "mia");
        assert_eq!(fallback(None, mxid), "mia");
        assert_eq!(fallback(None, "not-an-id"), "not-an-id");
    }

    #[test]
    fn profile_answers_are_cached_failures_briefly() {
        remember("@found:example.org", &Profile::Name("Found".into()));
        remember("@gone:example.org", &Profile::Nameless);
        remember("@down:example.org", &Profile::Failed);
        assert_eq!(cached("@found:example.org"), Some(Some("Found".into())));
        assert_eq!(cached("@gone:example.org"), Some(None));
        // A failure is not asked again right away either…
        assert_eq!(cached("@down:example.org"), Some(None));
        assert_eq!(cached("@never:example.org"), None);
        // …but sooner than an answer.
        let profiles = PROFILES.lock().unwrap();
        assert!(profiles["@down:example.org"].0 < profiles["@gone:example.org"].0);
    }

    /// A client logged in to a fake homeserver.
    async fn client(server: &wiremock::MockServer) -> Client {
        use mxbot_common::matrix_sdk::{
            authentication::matrix::MatrixSession,
            ruma::{api::MatrixVersion, owned_device_id, owned_user_id},
            store::RoomLoadSettings,
            SessionMeta, SessionTokens,
        };
        let client = Client::builder()
            .homeserver_url(server.uri())
            .server_versions([MatrixVersion::V1_1])
            .build()
            .await
            .unwrap();
        let session = MatrixSession {
            meta: SessionMeta {
                user_id: owned_user_id!("@bot:example.org"),
                device_id: owned_device_id!("BOT"),
            },
            tokens: SessionTokens {
                access_token: "token".into(),
                refresh_token: None,
            },
        };
        client
            .matrix_auth()
            .restore_session(session, RoomLoadSettings::default())
            .await
            .unwrap();
        client
    }

    /// `GET /profile/{user}` for a user whose ID contains `who`.
    fn profile(who: &str, answer: wiremock::ResponseTemplate) -> wiremock::Mock {
        use wiremock::matchers::{method, path_regex};
        wiremock::Mock::given(method("GET"))
            .and(path_regex(format!("^/_matrix/client/v3/profile/.*{who}")))
            .respond_with(answer)
    }

    #[tokio::test]
    async fn global_profiles_fill_in_where_no_room_is_shared() {
        use wiremock::ResponseTemplate as R;
        let server = wiremock::MockServer::start().await;
        let error = |code: u16, errcode: &str| {
            R::new(code).set_body_json(serde_json::json!({"errcode": errcode, "error": "no"}))
        };
        // Asked once: the second lookup comes from the cache.
        profile(
            "p1-ann",
            R::new(200).set_body_json(serde_json::json!({"displayname": " Ann\nE. "})),
        )
        .expect(1)
        .mount(&server)
        .await;
        profile(
            "p1-nameless",
            R::new(200).set_body_json(serde_json::json!({})),
        )
        .expect(1)
        .mount(&server)
        .await;
        profile("p1-gone", error(404, "M_NOT_FOUND"))
            .expect(1)
            .mount(&server)
            .await;
        profile("p1-hidden", error(403, "M_FORBIDDEN"))
            .expect(1)
            .mount(&server)
            .await;
        // A remote server that is down: the homeserver answers 502.
        profile("p1-down", error(502, "M_UNKNOWN"))
            .mount(&server)
            .await;
        let client = client(&server).await;
        let ids = [
            "@p1-ann:example.org",
            "@p1-nameless:example.org",
            "@p1-gone:remote.example",
            "@p1-hidden:remote.example",
            "@p1-down:remote.example",
            "not a matrix id",
        ];
        for _ in 0..2 {
            let names = live(&client, None, &ids, EVERYONE).await;
            assert_eq!(
                names,
                HashMap::from([("@p1-ann:example.org".to_owned(), "Ann E.".to_owned())])
            );
        }
        assert_eq!(cached("@p1-gone:remote.example"), Some(None));
        assert_eq!(cached("@p1-hidden:remote.example"), Some(None));
        // Failed: kept briefly, then asked again.
        let profiles = PROFILES.lock().unwrap();
        let retry = profiles["@p1-down:remote.example"].0;
        assert!(retry < profiles["@p1-gone:remote.example"].0);
        assert!(retry < profiles["@p1-hidden:remote.example"].0);
        assert!(retry <= Instant::now() + PROFILE_RETRY);
    }

    #[tokio::test]
    async fn a_slow_profile_server_cannot_hold_things_up() {
        let server = wiremock::MockServer::start().await;
        profile(
            "p2-slow",
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"displayname": "Slow"}))
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
        profile(
            "p2-quick",
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"displayname": "Quick"})),
        )
        .mount(&server)
        .await;
        let client = client(&server).await;
        let started = Instant::now();
        let names = live(
            &client,
            None,
            &["@p2-slow:example.org", "@p2-quick:example.org"],
            Duration::from_secs(1),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            names.get("@p2-quick:example.org").map(String::as_str),
            Some("Quick")
        );
        assert!(!names.contains_key("@p2-slow:example.org"));
        // Not given up on: asked again next time.
        assert_eq!(cached("@p2-slow:example.org"), None);
    }

    #[test]
    fn a_display_name_must_be_more_than_the_id_or_blank() {
        assert_eq!(usable(" Ann\nB ", "@ann:x"), Some("Ann B".into()));
        assert_eq!(usable("@ann:x", "@ann:x"), None);
        assert_eq!(usable("\n", "@ann:x"), None);
    }
}
