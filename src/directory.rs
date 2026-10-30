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

use mxbot_common::{
    directory::{Directory, DirectoryConfig},
    matrix_sdk::ruma::{OwnedUserId, UserId},
    supervise::Alive,
};
use tracing::{info, warn};

use crate::{state::State, BotContext};

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

/// Each house account's directory login (its username, which is also the resident's
/// Nextcloud user: `calendars`), for every Matrix account of each resident. An account
/// listed for two residents counts for neither.
pub type Logins = HashMap<OwnedUserId, String>;

pub fn logins(people: &[mxbot_common::directory::Person]) -> Logins {
    let mut out: Logins = HashMap::new();
    let mut shared: BTreeSet<OwnedUserId> = BTreeSet::new();
    for person in people {
        for account in &person.accounts {
            match out.get(account) {
                Some(other) if *other != person.username => {
                    shared.insert(account.clone());
                }
                _ => {
                    out.insert(account.clone(), person.username.clone());
                }
            }
        }
    }
    for account in &shared {
        out.remove(account);
    }
    out
}

/// The reader for `cfg`; `None` (logged) when it can't be set up, and every Matrix account is
/// a person of its own.
pub fn open(ctx: &BotContext, cfg: &DirectoryConfig) -> Option<Directory> {
    match UserId::parse(ctx.config.matrix.user_id.as_str())
        .map_err(anyhow::Error::from)
        .and_then(|me| Directory::new(cfg, &me))
    {
        Ok(d) => Some(d),
        Err(e) => {
            warn!("Directory: {e:#}; every Matrix account is a person of its own");
            None
        }
    }
}

/// Read the directory now and every `refresh_minutes`, into `ctx.accounts` (and the logins
/// into `ctx.logins`). `alive` ticks once per read.
pub async fn run(ctx: BotContext, directory: Directory, cfg: DirectoryConfig, alive: Alive) {
    let mut reported: Option<(Accounts, Vec<Vec<String>>)> = None;
    loop {
        alive.tick();
        match directory.people().await {
            Ok(people) => {
                *ctx.logins.write().unwrap_or_else(|e| e.into_inner()) =
                    Some(std::sync::Arc::new(logins(&people)));
                let (accounts, shared) =
                    Accounts::new(people.into_iter().map(|p| p.accounts).collect());
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
        tokio::time::sleep(cfg.refresh()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CleaningGroup, Person};

    fn id(s: &str) -> OwnedUserId {
        UserId::parse(s).unwrap()
    }

    #[test]
    fn every_account_of_a_resident_has_their_login() {
        let person = |username: &str, accounts: &[&str]| mxbot_common::directory::Person {
            username: username.to_owned(),
            name: String::new(),
            accounts: accounts.iter().map(|a| id(a)).collect(),
            groups: Default::default(),
        };
        let logins = logins(&[
            person(
                "anna",
                &["@anna:house.org", "@anna:matrix.org", "@both:matrix.org"],
            ),
            person("ben", &["@ben:house.org", "@both:matrix.org"]),
        ]);
        assert_eq!(
            logins.get(&id("@anna:matrix.org")).map(String::as_str),
            Some("anna")
        );
        assert_eq!(
            logins.get(&id("@ben:house.org")).map(String::as_str),
            Some("ben")
        );
        assert_eq!(logins.get(&id("@both:matrix.org")), None);
        assert_eq!(logins.len(), 3);
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
}
