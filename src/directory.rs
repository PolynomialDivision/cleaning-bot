//! Who is who in the house's directory (its identity provider, `[directory]`): a
//! resident's house account and the other Matrix accounts an admin entered for
//! them, so the plan knows them as one person whichever account writes.
//!
//! The plan keeps one Matrix ID per person (the account it met them by). An
//! event from another of their accounts is handled as that person's
//! (`person_account`); private chats stay with the account that is in them
//! (`private::authorized` tries every linked account). Admins are per account
//! in the config, and a linked account of an admin is an admin too: the
//! directory's admins say who is who.
//!
//! Nothing is merged. When two of one resident's accounts are already two
//! people in the plan, the one in use (a group, a turn, a completion) is the
//! person, the other only what its welcome left behind; when both are in use,
//! both stay as they are (logged, for an admin to sort out).
//! An account listed for two residents counts for neither. Read again every
//! `refresh_minutes`; while it can't be read, the last answer stays.
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use anyhow::{Context, Result};
use mxbot_common::matrix_sdk::ruma::{OwnedUserId, UserId};
use serde::Deserialize;
use tracing::{info, warn};

use crate::{
    config::{DirectoryConfig, DirectoryProvider},
    state::State,
    BotContext,
};

/// Residents with more than one account.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Accounts {
    people: Vec<Vec<OwnedUserId>>,
    of: HashMap<OwnedUserId, usize>,
}

impl Accounts {
    /// From each resident's accounts. Returns the accounts listed for more
    /// than one resident too, which count for none of them.
    pub fn new(residents: Vec<Vec<OwnedUserId>>) -> (Self, Vec<OwnedUserId>) {
        let mut owners: HashMap<&OwnedUserId, BTreeSet<usize>> = HashMap::new();
        for (i, accounts) in residents.iter().enumerate() {
            for a in accounts {
                owners.entry(a).or_default().insert(i);
            }
        }
        let mut shared: Vec<OwnedUserId> = owners
            .iter()
            .filter(|(_, o)| o.len() > 1)
            .map(|(a, _)| (*a).clone())
            .collect();
        shared.sort();
        let mut out = Accounts::default();
        for accounts in &residents {
            let mut own: Vec<OwnedUserId> = Vec::new();
            for a in accounts {
                if !shared.contains(a) && !own.contains(a) {
                    own.push(a.clone());
                }
            }
            if own.len() > 1 {
                for a in &own {
                    out.of.insert(a.clone(), out.people.len());
                }
                out.people.push(own);
            }
        }
        (out, shared)
    }

    /// Every account of the resident `user` is, `user` among them; empty when
    /// `user` has no others.
    pub fn linked(&self, user: &UserId) -> &[OwnedUserId] {
        self.of
            .get(user)
            .map_or(&[][..], |&i| self.people[i].as_slice())
    }

    pub fn residents(&self) -> usize {
        self.people.len()
    }

    /// Residents with two or more people in `state`'s plan that are in use.
    pub fn split_people(&self, state: &State) -> Vec<Vec<String>> {
        self.people
            .iter()
            .filter_map(|accounts| {
                let used: Vec<String> = in_use(state, known_people(state, accounts).collect())
                    .into_iter()
                    .map(|(account, _)| account.to_owned())
                    .collect();
                (used.len() > 1).then_some(used)
            })
            .collect()
    }
}

/// The plan's active people among `accounts`: (Matrix ID, person ID).
fn known_people<'a>(
    state: &'a State,
    accounts: &'a [OwnedUserId],
) -> impl Iterator<Item = (&'a str, &'a str)> {
    state
        .persons
        .iter()
        .filter(|p| p.active)
        .filter_map(|p| Some((p.matrix_id.as_deref()?, p.id.as_str())))
        .filter(|(m, _)| accounts.iter().any(|a| a.as_str() == *m))
}

/// Those of `people` in a group, with a turn or with a completion: the rest
/// are what a welcome left behind (it makes a person for every newcomer).
fn in_use<'a>(state: &State, people: Vec<(&'a str, &'a str)>) -> Vec<(&'a str, &'a str)> {
    people
        .into_iter()
        .filter(|(_, id)| crate::commands::person_has_activity(state, id))
        .collect()
}

/// The account the plan knows `user`'s resident by: the one of their accounts
/// with a person in the plan, or, with several, the one of them in use; else
/// `user` itself (also when several are in use: nothing is merged).
pub fn person_account(accounts: &Accounts, state: &State, user: &UserId) -> OwnedUserId {
    let known: Vec<(&str, &str)> = known_people(state, accounts.linked(user)).collect();
    let chosen = match known.as_slice() {
        [one] => Some(one.0),
        [] => None,
        _ => match in_use(state, known.clone()).as_slice() {
            [one] => Some(one.0),
            _ => None,
        },
    };
    chosen
        .and_then(|account| UserId::parse(account).ok())
        .unwrap_or_else(|| user.to_owned())
}

// ── Reading the directory ─────────────────────────────────────────────────────

/// One user record, as far as it matters here.
#[derive(Deserialize, Debug)]
struct User {
    username: String,
    #[serde(default = "yes")]
    is_active: bool,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    attributes: serde_json::Map<String, serde_json::Value>,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct Page {
    results: Vec<User>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Deserialize)]
struct Pagination {
    /// The next page's number, 0 on the last one.
    #[serde(default)]
    next: serde_json::Value,
}

/// A resident's accounts: the house account, then the others in their order.
/// Matrix IDs that don't parse are left out.
fn accounts_of(user: &User, cfg: &DirectoryConfig) -> Vec<OwnedUserId> {
    let localpart = cfg
        .localpart_attribute
        .as_ref()
        .and_then(|a| user.attributes.get(a))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or(&user.username)
        .to_lowercase();
    let mut out: Vec<OwnedUserId> = Vec::new();
    if let Ok(id) = UserId::parse(format!("@{localpart}:{}", cfg.server_name)) {
        out.push(id);
    }
    let listed: Vec<String> = match user.attributes.get(&cfg.matrix_ids_attribute) {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_owned)
            .collect(),
        Some(serde_json::Value::String(s)) => {
            s.split([',', ' ', '\n', '\t']).map(str::to_owned).collect()
        }
        _ => Vec::new(),
    };
    for id in listed.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match UserId::parse(id) {
            Ok(id) if !out.contains(&id) => out.push(id),
            Ok(_) => {}
            Err(_) => warn!(user = %user.username, "Directory: not a Matrix ID, left out"),
        }
    }
    out
}

/// The residents among `users`: active people, not service accounts.
fn residents(users: &[User], cfg: &DirectoryConfig) -> Vec<Vec<OwnedUserId>> {
    users
        .iter()
        .filter(|u| u.is_active && !u.kind.contains("service_account"))
        .map(|u| accounts_of(u, cfg))
        .collect()
}

async fn fetch(client: &reqwest::Client, cfg: &DirectoryConfig, token: &str) -> Result<Vec<User>> {
    let DirectoryProvider::Authentik = cfg.provider;
    let base = cfg.api_url.trim_end_matches('/');
    let mut users = Vec::new();
    let mut page = 1u64;
    // At most 50 pages of 200: far more than a house.
    for _ in 0..50 {
        let url = format!("{base}/core/users/?is_active=true&page_size=200&page={page}");
        let answer: Page = client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .context("reading the directory")?
            .error_for_status()
            .context("reading the directory")?
            .json()
            .await
            .context("the directory's answer")?;
        users.extend(answer.results);
        match answer.pagination.and_then(|p| p.next.as_u64()) {
            Some(next) if next > page => page = next,
            _ => return Ok(users),
        }
    }
    anyhow::bail!("the directory has more than 50 pages of users")
}

/// Read the directory now and every `refresh_minutes`, into `ctx.accounts`.
pub async fn run(ctx: BotContext, cfg: DirectoryConfig) {
    let token = match std::env::var(&cfg.token_env) {
        Ok(t) if !t.trim().is_empty() => t.trim().to_owned(),
        _ => {
            warn!(env = %cfg.token_env, "Directory: no token, every Matrix account is a person of its own");
            return;
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("Directory: no HTTP client: {e}");
            return;
        }
    };
    let every = Duration::from_secs(cfg.refresh_minutes.max(1) * 60);
    let mut reported: Option<(Accounts, Vec<Vec<String>>)> = None;
    loop {
        match fetch(&client, &cfg, &token).await {
            Ok(users) => {
                let (accounts, shared) = Accounts::new(residents(&users, &cfg));
                let split = accounts.split_people(&*ctx.state.lock().await);
                let changed = reported
                    .as_ref()
                    .is_none_or(|(a, s)| *a != accounts || *s != split);
                if changed {
                    info!(
                        residents = accounts.residents(),
                        "Directory: residents with more than one Matrix account"
                    );
                    for account in &shared {
                        warn!(%account, "Directory: listed for more than one resident, counts for none");
                    }
                    for people in &split {
                        warn!(
                            people = %people.join(", "),
                            "Directory: one resident, several people in the plan; left as they are"
                        );
                    }
                }
                *ctx.accounts.write().unwrap_or_else(|e| e.into_inner()) = accounts.clone();
                reported = Some((accounts, split));
            }
            Err(e) => warn!("Directory: {e:#}; keeping the last answer"),
        }
        tokio::time::sleep(every).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CleaningGroup, Person};

    fn id(s: &str) -> OwnedUserId {
        UserId::parse(s).unwrap()
    }

    fn cfg(api_url: &str) -> DirectoryConfig {
        DirectoryConfig {
            provider: DirectoryProvider::Authentik,
            api_url: api_url.to_owned(),
            server_name: "house.org".to_owned(),
            localpart_attribute: Some("matrix_localpart".to_owned()),
            matrix_ids_attribute: "matrix_ids".to_owned(),
            token_env: "UNUSED".to_owned(),
            refresh_minutes: 15,
        }
    }

    fn users(json: serde_json::Value) -> Vec<User> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn a_resident_is_their_house_account_and_the_listed_ones() {
        let list = users(serde_json::json!([
            {"username": "Nick", "type": "internal",
             "attributes": {"matrix_ids": ["@schnick:matrix.org", "not an id", "@nick:house.org"]}},
            {"username": "anna", "type": "external", "attributes": {"matrix_ids": "@anna:matrix.org, @anna:example.org"}},
            {"username": "pinned", "type": "internal", "attributes": {"matrix_localpart": "pin"}},
            {"username": "gone", "is_active": false, "attributes": {"matrix_ids": ["@gone:matrix.org"]}},
            {"username": "house-infra-reader", "type": "service_account", "attributes": {}},
        ]));
        let r = residents(&list, &cfg("x"));
        assert_eq!(
            r,
            vec![
                vec![id("@nick:house.org"), id("@schnick:matrix.org")],
                vec![
                    id("@anna:house.org"),
                    id("@anna:matrix.org"),
                    id("@anna:example.org")
                ],
                vec![id("@pin:house.org")],
            ]
        );
    }

    #[test]
    fn an_account_listed_for_two_residents_counts_for_neither() {
        let (accounts, shared) = Accounts::new(vec![
            vec![
                id("@a:house.org"),
                id("@shared:matrix.org"),
                id("@a:matrix.org"),
            ],
            vec![id("@b:house.org"), id("@shared:matrix.org")],
        ]);
        assert_eq!(shared, vec![id("@shared:matrix.org")]);
        assert_eq!(accounts.residents(), 1);
        assert!(accounts.linked(&id("@shared:matrix.org")).is_empty());
        assert!(accounts.linked(&id("@b:house.org")).is_empty());
        assert_eq!(accounts.linked(&id("@a:matrix.org")).len(), 2);
    }

    #[test]
    fn another_account_acts_as_the_person_the_plan_knows() {
        let (accounts, _) =
            Accounts::new(vec![vec![id("@nick:house.org"), id("@schnick:matrix.org")]]);
        let mut state = State::default();
        // Nobody known yet: each account as itself (the first to act becomes the person).
        assert_eq!(
            person_account(&accounts, &state, &id("@schnick:matrix.org")),
            id("@schnick:matrix.org")
        );
        state
            .persons
            .push(Person::new_matrix("@schnick:matrix.org"));
        assert_eq!(
            person_account(&accounts, &state, &id("@nick:house.org")),
            id("@schnick:matrix.org")
        );
        assert_eq!(
            person_account(&accounts, &state, &id("@other:house.org")),
            id("@other:house.org")
        );
        assert!(accounts.split_people(&state).is_empty());
        // Both in the plan, the house account only as what its welcome left:
        // the one in use is the person.
        state.persons.push(Person::new_matrix("@nick:house.org"));
        let mut group = CleaningGroup::new("Kitchen");
        group.member_ids = vec![state.persons[0].id.clone()];
        state.cleaning_groups.push(group);
        assert_eq!(
            person_account(&accounts, &state, &id("@nick:house.org")),
            id("@schnick:matrix.org")
        );
        assert!(accounts.split_people(&state).is_empty());
        // Both in use: nothing merged, each stays itself.
        let second = state.persons[1].id.clone();
        state.cleaning_groups[0].member_ids.push(second);
        assert_eq!(
            person_account(&accounts, &state, &id("@nick:house.org")),
            id("@nick:house.org")
        );
        assert_eq!(
            accounts.split_people(&state),
            vec![vec![
                "@schnick:matrix.org".to_owned(),
                "@nick:house.org".to_owned()
            ]]
        );
        // A person no longer active doesn't count.
        state.persons[1].active = false;
        assert_eq!(
            person_account(&accounts, &state, &id("@nick:house.org")),
            id("@schnick:matrix.org")
        );
    }

    #[tokio::test]
    async fn reads_every_page_with_the_token() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        for (page, next, name) in [("1", 2, "one"), ("2", 0, "two")] {
            Mock::given(method("GET"))
                .and(path("/api/v3/core/users/"))
                .and(query_param("page", page))
                .and(query_param("is_active", "true"))
                .and(header("authorization", "Bearer secret"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "pagination": {"next": next},
                    "results": [{"username": name, "type": "internal", "attributes": {}}],
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        let client = reqwest::Client::new();
        let list = fetch(
            &client,
            &cfg(&format!("{}/api/v3/", server.uri())),
            "secret",
        )
        .await
        .unwrap();
        let names: Vec<&str> = list.iter().map(|u| u.username.as_str()).collect();
        assert_eq!(names, ["one", "two"]);

        let refused = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&refused)
            .await;
        assert!(fetch(&client, &cfg(&refused.uri()), "secret")
            .await
            .is_err());
    }
}
