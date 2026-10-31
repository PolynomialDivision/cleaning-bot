//! Who is who in the house's directory (its identity provider, `[directory]`): the people
//! living here, keyed by the directory's id of them (Authentik's `uuid`), their Matrix
//! accounts and their groups (mxbot-common `directory`).
//!
//! The plan keeps its own people (`domain::Person`) with one Matrix ID each, the address the
//! bot writes to them at, and, once matched, the directory id (`Person::directory_id`). An
//! event from any of a resident's recognised accounts (their house account, the links an
//! admin entered, the links they proved, none in conflict) is handled as that person's
//! (`person_account`); private chats with any of them count (`recognised`).
//!
//! Matching a person in the plan to the directory: by directory id first, else by one of the
//! resident's recognised accounts; then the id is stored (`reconcile`). Nothing is merged:
//! when two people in the plan are one resident and both are in use (a group, a turn, a
//! completion), both stay as they are and the admins are told once. An account two residents
//! claim counts for neither.
//!
//! Rights never come from a link: an admin is an account in `[security] admin_users`, the
//! account that wrote (`BotContext::is_admin`), never another account of theirs.
//!
//! The directory is read at startup and every `refresh_minutes`. While it can't be read,
//! the last answer stays for recognising accounts and nothing follows it (`reconcile`).
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::SystemTime,
};

use mxbot_common::{
    directory::{Directory, People, Person as Resident, Snapshot},
    matrix_sdk::{
        ruma::{OwnedUserId, UserId},
        Client,
    },
    supervise::Alive,
};
use tracing::warn;

use crate::{config::DirectorySection, domain::Person, state::State, BotContext};

pub mod reconcile;
#[cfg(test)]
mod tests;

/// The last good read of the directory, shared by the bot and its website; `None` until the
/// first one.
pub type Shared = Arc<RwLock<Option<Arc<Snapshot>>>>;

/// The last good read, if there was one.
pub fn read(shared: &Shared) -> Option<Arc<Snapshot>> {
    shared.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Which person in the plan a resident is.
#[derive(Debug)]
pub enum Match<'a> {
    /// Linked by the directory id before.
    Anchored(&'a Person),
    /// Not linked yet, found by one of their accounts (the one in use, when there are more).
    Found(&'a Person),
    /// Several people in the plan with their accounts are in use: nothing is merged.
    Split(Vec<&'a Person>),
    None,
}

/// The plan's active people who may be `resident`: linked to them, or not linked to anyone
/// and with one of their recognised accounts.
fn candidates<'a>(state: &'a State, people: &People, resident: &Resident) -> Vec<&'a Person> {
    let accounts: Vec<&UserId> = people.accounts(resident).collect();
    state
        .persons
        .iter()
        .filter(|p| p.active)
        .filter(|p| match p.directory_id.as_deref() {
            Some(id) => id == resident.id,
            None => p
                .matrix_id
                .as_deref()
                .is_some_and(|m| accounts.iter().any(|a| a.as_str() == m)),
        })
        .collect()
}

/// Which person in the plan `resident` is (see `Match`).
pub fn plan_person<'a>(state: &'a State, people: &People, resident: &Resident) -> Match<'a> {
    let found = candidates(state, people, resident);
    if let Some(anchored) = found
        .iter()
        .find(|p| p.directory_id.as_deref() == Some(resident.id.as_str()))
    {
        return Match::Anchored(anchored);
    }
    match found.as_slice() {
        [] => Match::None,
        [one] => Match::Found(one),
        several => {
            let used: Vec<&Person> = several
                .iter()
                .copied()
                .filter(|p| crate::commands::person_has_activity(state, &p.id))
                .collect();
            match used.as_slice() {
                [one] => Match::Found(one),
                // None in use: the first the plan met.
                [] => Match::Found(several[0]),
                _ => Match::Split(used),
            }
        }
    }
}

/// The people in the plan who are `resident` and in use, when there is more than one: what
/// an admin sorts out (nothing is merged). Empty otherwise.
pub fn split_people<'a>(state: &'a State, people: &People, resident: &Resident) -> Vec<&'a Person> {
    match plan_person(state, people, resident) {
        Match::Split(used) => used,
        Match::Anchored(anchor) => {
            let mut used: Vec<&Person> = candidates(state, people, resident)
                .into_iter()
                .filter(|p| p.id != anchor.id)
                .filter(|p| crate::commands::person_has_activity(state, &p.id))
                .collect();
            if !used.is_empty() {
                used.insert(0, anchor);
            }
            used
        }
        _ => Vec::new(),
    }
}

/// The account the plan knows `user`'s person by: their address in the plan (the person
/// linked to their directory id, or found by one of their accounts); else `user` itself
/// (also when two people in the plan are theirs and both in use: nothing is merged).
pub fn person_account(people: Option<&People>, state: &State, user: &UserId) -> OwnedUserId {
    let Some((people, resident)) = people.and_then(|p| Some((p, p.owner_of(user)?))) else {
        return user.to_owned();
    };
    let address = |p: &Person| {
        p.matrix_id
            .as_deref()
            .and_then(|m| UserId::parse(m).ok())
            .unwrap_or_else(|| user.to_owned())
    };
    match plan_person(state, people, resident) {
        Match::Anchored(anchor) => {
            // `user` as a person of its own, in use: they stay two (an admin sorts it out).
            let own = state
                .persons
                .iter()
                .find(|p| p.active && p.matrix_id.as_deref() == Some(user.as_str()));
            match own {
                Some(own)
                    if own.id != anchor.id
                        && own.directory_id.is_none()
                        && crate::commands::person_has_activity(state, &own.id) =>
                {
                    user.to_owned()
                }
                _ => address(anchor),
            }
        }
        Match::Found(person) => address(person),
        Match::Split(_) | Match::None => user.to_owned(),
    }
}

/// Every recognised account of `user`'s resident, `user` among them (none in conflict, none
/// of a deactivated person); empty when the directory doesn't know `user`.
pub fn recognised(people: Option<&People>, user: &UserId) -> Vec<OwnedUserId> {
    let Some((people, resident)) = people.and_then(|p| Some((p, p.owner_of(user)?))) else {
        return Vec::new();
    };
    people.accounts(resident).map(UserId::to_owned).collect()
}

/// Each resident's directory login (their username, which is also their Nextcloud user:
/// `calendars`), by directory id and by each of their recognised accounts. Deactivated
/// people have none; an account two residents claim counts for neither.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Logins {
    by_account: HashMap<OwnedUserId, String>,
    by_id: HashMap<String, String>,
}

impl Logins {
    /// The login of `person` in the plan: by their directory id, else by their address.
    pub fn of(&self, person: &Person) -> Option<&String> {
        if let Some(id) = person.directory_id.as_deref() {
            return self.by_id.get(id);
        }
        let account = UserId::parse(person.matrix_id.as_deref()?).ok()?;
        self.by_account.get(&account)
    }
}

#[cfg(test)]
impl Logins {
    /// Only the logins `keep` keeps (someone gone from the directory).
    pub fn retain(&mut self, keep: impl Fn(&String) -> bool) {
        self.by_account.retain(|_, l| keep(l));
        self.by_id.retain(|_, l| keep(l));
    }
}

impl FromIterator<(OwnedUserId, String)> for Logins {
    fn from_iter<I: IntoIterator<Item = (OwnedUserId, String)>>(iter: I) -> Self {
        Logins {
            by_account: iter.into_iter().collect(),
            by_id: HashMap::new(),
        }
    }
}

pub fn logins(people: &People) -> Logins {
    let mut out = Logins::default();
    for resident in people.active() {
        out.by_id
            .insert(resident.id.clone(), resident.username.clone());
        for account in people.accounts(resident) {
            out.by_account
                .insert(account.to_owned(), resident.username.clone());
        }
    }
    out
}

/// The reader for `cfg`; `None` (logged) when it can't be set up, and every Matrix account is
/// a person of its own.
pub fn open(ctx: &BotContext, cfg: &DirectorySection) -> Option<Directory> {
    match UserId::parse(ctx.config.matrix.user_id.as_str())
        .map_err(anyhow::Error::from)
        .and_then(|me| Directory::new(&cfg.reader, &me))
    {
        Ok(d) => Some(d),
        Err(e) => {
            warn!("Directory: {e:#}; every Matrix account is a person of its own");
            None
        }
    }
}

/// The groups a read asks about: the residents' and every group's `directory_group`.
pub fn wanted_groups(state: &State, cfg: &DirectorySection) -> BTreeSet<String> {
    state
        .cleaning_groups
        .iter()
        .filter_map(|g| g.directory_group.clone())
        .chain(Some(cfg.residents_group.clone()).filter(|g| !g.is_empty()))
        .collect()
}

/// Read the directory now and every `refresh_minutes` into `ctx.directory` (and the logins
/// into `ctx.logins`), and once the bot has synced (`synced`), follow it
/// (`reconcile::run_round`). `alive`: ok after a good read; a directory that is down
/// (unreachable, timing out, 5xx, 429: `directory::is_outage`) is another service's outage
/// (`Alive::unavailable`: in the status file and its alerts, never unhealthy, no restart); one
/// that refuses the token (401/403) or answers what the bot cannot read is failing (unhealthy
/// after its time: the config or the bot needs fixing). Either way the last answer stays for
/// recognising accounts.
pub async fn run(
    ctx: BotContext,
    client: Client,
    directory: Directory,
    cfg: DirectorySection,
    admin_groups: Vec<String>,
    alive: Alive,
    synced: Arc<AtomicBool>,
) {
    alive.expect_every(cfg.reader.refresh());
    let mut reported: Option<BTreeSet<String>> = None;
    loop {
        let wanted = wanted_groups(&*ctx.state.lock().await, &cfg);
        match directory.snapshot(&wanted).await {
            Ok(snapshot) => {
                alive.ok();
                // `[security] admin_groups`: house accounts only, while this read may be used.
                if !admin_groups.is_empty() {
                    ctx.admin_users
                        .follow(&snapshot, &cfg.reader, &admin_groups);
                }
                let conflicts: BTreeSet<String> = snapshot
                    .people
                    .conflicts()
                    .into_iter()
                    .map(|(account, names)| format!("{account} ({})", names.join(", ")))
                    .collect();
                if reported.as_ref() != Some(&conflicts) {
                    for conflict in &conflicts {
                        warn!(%conflict, "Directory: an account two residents claim counts for neither");
                    }
                    reported = Some(conflicts);
                }
                *ctx.logins.write().unwrap_or_else(|e| e.into_inner()) =
                    Some(Arc::new(logins(&snapshot.people)));
                let snapshot = Arc::new(snapshot);
                *ctx.directory.write().unwrap_or_else(|e| e.into_inner()) =
                    Some(Arc::clone(&snapshot));
                // Not before the first sync: the rooms to tell the admins in are known then.
                while !synced.load(Ordering::Acquire) {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    alive.tick();
                }
                reconcile::run_round(&ctx, &client, &snapshot, &cfg, SystemTime::now()).await;
            }
            Err(e) => {
                warn!("Directory: {e:#}; keeping the last answer, following nothing");
                report_failed_read(&alive, &e);
            }
        }
        tokio::time::sleep(cfg.reader.refresh()).await;
    }
}

/// A read that failed, for the status file and the heartbeat: the directory down is another
/// service's outage (`unavailable`, never unhealthy); a refusal or an answer the bot cannot
/// read is a failure of this task (unhealthy after its time).
fn report_failed_read(alive: &Alive, e: &anyhow::Error) {
    if mxbot_common::directory::is_outage(e) {
        alive.unavailable(format!("directory: {e:#}"));
    } else {
        alive.failed(format!("directory: {e:#}"));
    }
}
