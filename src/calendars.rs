//! Every resident's turns in a Nextcloud calendar of their own (`[nextcloud]`, with
//! `[directory]`).
//!
//! The bot's Nextcloud account owns one calendar per resident with a house account,
//! `cleaning-<login>` named "Cleaning: `<name>`", shared read-only with that resident alone
//! (their Nextcloud user is their directory login). It holds their turns from today on, one
//! event per turn with the turn's UID (the same as in an `!ical` file), and follows every
//! plan change (takeovers, swaps, rhythms, done marks) within a minute: only the events that
//! changed are written. Every `refresh_minutes` (and at the start) everything is compared with
//! the server: calendar, share, every event's ETag; past and obsolete turns go.
//!
//! Someone who no longer has a calendar to get (gone from the directory, or in no group and
//! without turns) loses theirs: unshared, then deleted, but only after a complete comparison
//! with a directory answer in hand, never while the directory is unknown, never all at once
//! because nobody is wanted at all, and never more than [`max_removals`] in one round (a
//! directory answer that is wrong but looks fine).
//!
//! Two logins with the same calendar (their slugs differ only in characters a slug cannot hold,
//! or in case) get none: neither could tell whose turns and share it holds.
//!
//! Residents without a house account get their turns as a file (`!ical`). Once a resident's
//! calendar is shared with them, their selectors and `!ical` say where it is, and the bot tells
//! them once in their private chat (if they have one; a failed note is tried again next round).
//! Without calendars managed (no `[nextcloud]`, no app password), `!ical` sends files again.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use chrono::NaiveDate;
use mxbot_common::{
    calendar::nextcloud::{self, Deleted, Nextcloud, Precondition, Share, Written},
    matrix_sdk::{ruma::OwnedUserId, Client},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

use crate::{
    config::NextcloudConfig, directory::Logins, domain::PersonId, schedule::build_schedule,
    state::State, BotContext,
};

/// The slug prefix of the bot's calendars: only those are ever deleted.
const PREFIX: &str = "cleaning-";
/// DTSTAMP and LAST-MODIFIED of a rendered event until it is written (same length as a real one,
/// so line folding does not change).
const STAMP: &str = "00000000T000000Z";
/// How often a change of the plan is looked for.
const TICK: std::time::Duration = std::time::Duration::from_secs(30);
/// After a failed round, the next complete one at the latest.
const RETRY: std::time::Duration = std::time::Duration::from_secs(120);

/// The calendar of a resident with the login `login`.
pub fn slug(login: &str) -> String {
    let safe: String = login
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{PREFIX}{safe}")
}

/// The calendar's display name.
pub fn calendar_name(display_name: &str) -> String {
    format!("Cleaning: {display_name}")
}

/// One event as it should be: its calendar object (with [`STAMP`] for the time it is written)
/// and a hash of that.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub ics: String,
    pub hash: String,
}

impl Event {
    fn new(ics: String) -> Self {
        let hash = hex::encode(Sha256::digest(ics.as_bytes()));
        Self { ics, hash }
    }

    /// The object as it is written at `now` (`YYYYMMDDTHHMMSSZ`).
    fn stamped(&self, now: &str) -> String {
        self.ics
            .replace(&format!("DTSTAMP:{STAMP}"), &format!("DTSTAMP:{now}"))
            .replace(
                &format!("LAST-MODIFIED:{STAMP}"),
                &format!("LAST-MODIFIED:{now}"),
            )
    }
}

/// One resident's calendar as it should be.
#[derive(Clone, Debug, PartialEq)]
pub struct Wanted {
    pub slug: String,
    pub name: String,
    /// The people in the plan who are this resident (one, or several accounts of theirs).
    pub persons: Vec<PersonId>,
    /// UID → event.
    pub events: BTreeMap<String, Event>,
}

/// Every resident's calendar as it should be, by login: every active person in the plan with a
/// Matrix account the directory knows (`logins`) and something to do with the plan (a group,
/// a turn, a completion), with their turns of the next `weeks` weeks that end `today` or later.
pub fn wanted(
    state: &State,
    logins: &Logins,
    weeks: usize,
    today: NaiveDate,
) -> BTreeMap<String, Wanted> {
    let snapshot = build_schedule(state, weeks);
    let mut out: BTreeMap<String, Wanted> = BTreeMap::new();
    for person in state.persons.iter().filter(|p| p.active) {
        let Some(login) = person
            .matrix_id
            .as_deref()
            .and_then(|m| OwnedUserId::try_from(m).ok())
            .and_then(|m| logins.get(&m))
        else {
            continue;
        };
        if !crate::commands::person_has_activity(state, &person.id) {
            continue;
        }
        let entry = out.entry(login.clone()).or_insert_with(|| Wanted {
            slug: slug(login),
            name: calendar_name(&person.display_name),
            persons: Vec::new(),
            events: BTreeMap::new(),
        });
        entry.persons.push(person.id.clone());
        for a in snapshot
            .for_person(&person.id)
            .into_iter()
            .filter(|a| a.end >= today)
        {
            let ics = crate::ical::render_event(&snapshot, a, &person.id, STAMP);
            entry.events.insert(a.uid.clone(), Event::new(ics));
        }
    }
    for (slug, logins) in shared_slugs(&out) {
        warn!(
            calendar = %slug,
            logins = ?logins,
            "Calendars: these logins would share one calendar; none of them gets one"
        );
        for login in &logins {
            out.remove(login);
        }
    }
    out
}

/// Calendars two or more logins would get (the same slug, also in another case: a server
/// may not tell them apart): slug → those logins.
fn shared_slugs(wanted: &BTreeMap<String, Wanted>) -> BTreeMap<String, Vec<String>> {
    let mut by_slug: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (login, w) in wanted {
        by_slug
            .entry(w.slug.to_lowercase())
            .or_default()
            .push(login.clone());
    }
    by_slug.retain(|_, logins| logins.len() > 1);
    by_slug
}

/// What the bot last wrote, by login (store/nextcloud.json): what a change needs written.
#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Cache {
    #[serde(default)]
    pub calendars: BTreeMap<String, CachedCalendar>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct CachedCalendar {
    pub slug: String,
    pub name: String,
    /// Shared with the resident (Nextcloud has their account and the share).
    #[serde(default)]
    pub shared: bool,
    /// They were told in their private chat.
    #[serde(default)]
    pub announced: bool,
    /// UID → what was written.
    #[serde(default)]
    pub events: BTreeMap<String, CachedEvent>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct CachedEvent {
    pub hash: String,
    #[serde(default)]
    pub etag: Option<String>,
}

/// What a round did.
#[derive(Default, Debug, PartialEq)]
pub struct Outcome {
    pub written: usize,
    pub deleted: usize,
    /// Calendars of people who no longer get one: unshared and deleted.
    pub removed: Vec<String>,
    /// Per resident or step; the rest went on.
    pub errors: Vec<String>,
}

/// Write `ics` as `uid`; a precondition that fails (someone else's version, a stale cache)
/// is resolved by writing anyway: the bot owns the calendar.
async fn write(
    nc: &Nextcloud,
    slug: &str,
    uid: &str,
    ics: &str,
    when: Precondition,
) -> Result<Option<String>> {
    match nc.put_event(slug, uid, ics, &when).await? {
        Written::Stored { etag } => Ok(etag),
        Written::Conflict => match nc
            .put_event(slug, uid, ics, &Precondition::Overwrite)
            .await?
        {
            Written::Stored { etag } => Ok(etag),
            Written::Conflict => bail!("{uid} in {slug}: refused"),
        },
    }
}

async fn delete(nc: &Nextcloud, slug: &str, uid: &str, etag: Option<&str>) -> Result<()> {
    if nc.delete_event(slug, uid, etag).await? == Deleted::Conflict {
        nc.delete_event(slug, uid, None).await?;
    }
    Ok(())
}

/// Compare one resident's calendar with the server completely and fix what differs.
async fn full_one(
    nc: &Nextcloud,
    login: &str,
    want: &Wanted,
    cached: &mut CachedCalendar,
    now: &str,
    outcome: &mut Outcome,
) -> Result<()> {
    if nc.ensure_calendar(&want.slug, &want.name).await? {
        info!(calendar = %want.slug, "Calendars: made");
        cached.events.clear();
        cached.shared = false;
    }
    cached.slug = want.slug.clone();
    cached.name = want.name.clone();

    let share = Share {
        principal: nextcloud::user(login),
        write: false,
    };
    let mut current = nc.shares(&want.slug).await?;
    let (set, remove) = nextcloud::plan_shares(&current, std::slice::from_ref(&share));
    for principal in &remove {
        nc.unshare(&want.slug, principal).await?;
    }
    for s in &set {
        nc.share(&want.slug, s).await?;
    }
    if !set.is_empty() || !remove.is_empty() {
        current = nc.shares(&want.slug).await?;
    }
    // Nextcloud answers a share with someone it has no account of with 200 and shares nothing:
    // until their first login there, `!ical` sends them a file.
    cached.shared = current.contains(&share);

    let server: BTreeMap<String, String> = nc
        .list_events(&want.slug)
        .await?
        .into_iter()
        .map(|e| (e.uid, e.etag))
        .collect();
    for (uid, event) in &want.events {
        let known = cached.events.get(uid);
        let precondition = match server.get(uid) {
            Some(etag) => {
                let same = known.is_some_and(|k| {
                    k.hash == event.hash && k.etag.as_deref().is_none_or(|e| e == etag)
                });
                if same {
                    if let Some(k) = cached.events.get_mut(uid) {
                        k.etag = Some(etag.clone());
                    }
                    continue;
                }
                Precondition::Match(etag.clone())
            }
            None => Precondition::Create,
        };
        let etag = write(nc, &want.slug, uid, &event.stamped(now), precondition).await?;
        outcome.written += 1;
        cached.events.insert(
            uid.clone(),
            CachedEvent {
                hash: event.hash.clone(),
                etag,
            },
        );
    }
    for (uid, etag) in server.iter().filter(|(u, _)| !want.events.contains_key(*u)) {
        delete(nc, &want.slug, uid, Some(etag)).await?;
        outcome.deleted += 1;
    }
    cached.events.retain(|uid, _| want.events.contains_key(uid));
    Ok(())
}

/// Write only what changed since the last round (the cache's view, no reading).
async fn quick_one(
    nc: &Nextcloud,
    want: &Wanted,
    cached: &mut CachedCalendar,
    now: &str,
    outcome: &mut Outcome,
) -> Result<()> {
    if cached.name != want.name {
        nc.ensure_calendar(&want.slug, &want.name).await?;
        cached.name = want.name.clone();
    }
    for (uid, event) in &want.events {
        let known = cached.events.get(uid);
        if known.is_some_and(|k| k.hash == event.hash) {
            continue;
        }
        let precondition = match known {
            Some(CachedEvent {
                etag: Some(etag), ..
            }) => Precondition::Match(etag.clone()),
            Some(_) => Precondition::Overwrite,
            None => Precondition::Create,
        };
        let etag = write(nc, &want.slug, uid, &event.stamped(now), precondition).await?;
        outcome.written += 1;
        cached.events.insert(
            uid.clone(),
            CachedEvent {
                hash: event.hash.clone(),
                etag,
            },
        );
    }
    let gone: Vec<(String, Option<String>)> = cached
        .events
        .iter()
        .filter(|(uid, _)| !want.events.contains_key(*uid))
        .map(|(uid, e)| (uid.clone(), e.etag.clone()))
        .collect();
    for (uid, etag) in gone {
        delete(nc, &want.slug, &uid, etag.as_deref()).await?;
        cached.events.remove(&uid);
        outcome.deleted += 1;
    }
    Ok(())
}

/// The most calendars one round removes, of `known` of the bot's: a third of them, at least 2.
/// More at once is a directory or plan gone wrong (an answer that lost half the house), not
/// people moving out: none is removed then.
pub fn max_removals(known: usize) -> usize {
    (known / 3).max(2)
}

/// The calendars of people who no longer get one: unshared, then deleted.
async fn remove_unwanted(
    nc: &Nextcloud,
    cache: &mut Cache,
    wanted: &BTreeMap<String, Wanted>,
    outcome: &mut Outcome,
) -> Result<()> {
    let keep: BTreeSet<&str> = wanted.values().map(|w| w.slug.as_str()).collect();
    let known: Vec<String> = nc
        .calendars()
        .await?
        .into_iter()
        .map(|c| c.slug)
        .filter(|slug| slug.starts_with(PREFIX))
        .collect();
    let unwanted: Vec<String> = known
        .iter()
        .filter(|slug| !keep.contains(slug.as_str()))
        .cloned()
        .collect();
    if unwanted.len() > max_removals(known.len()) {
        let error = format!(
            "{} of {} calendars would go at once (at most {}): none removed",
            unwanted.len(),
            known.len(),
            max_removals(known.len())
        );
        error!("Calendars: {error}");
        outcome.errors.push(error);
        return Ok(());
    }
    for slug in unwanted {
        let result: Result<()> = async {
            for share in nc.shares(&slug).await? {
                nc.unshare(&slug, &share.principal).await?;
            }
            nc.delete_calendar(&slug).await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                info!(calendar = %slug, "Calendars: unshared and deleted (no longer wanted)");
                outcome.removed.push(slug.clone());
                cache.calendars.retain(|_, c| c.slug != slug);
            }
            Err(e) => outcome.errors.push(format!("removing {slug}: {e:#}")),
        }
    }
    cache
        .calendars
        .retain(|login, _| wanted.contains_key(login));
    Ok(())
}

/// One round. `full`: everything compared with the server, and the calendars nobody gets any
/// more removed; else only what changed since the last round is written (a resident new to
/// the cache gets a full comparison).
pub async fn reconcile(
    nc: &Nextcloud,
    cache: &mut Cache,
    wanted: &BTreeMap<String, Wanted>,
    full: bool,
    now: &str,
) -> Outcome {
    let mut outcome = Outcome::default();
    for (login, want) in wanted {
        let known = cache.calendars.contains_key(login);
        let cached = cache.calendars.entry(login.clone()).or_default();
        let result = if full || !known {
            full_one(nc, login, want, cached, now, &mut outcome).await
        } else {
            quick_one(nc, want, cached, now, &mut outcome).await
        };
        if let Err(e) = result {
            outcome.errors.push(format!("{}: {e:#}", want.slug));
        }
    }
    if full {
        if wanted.is_empty() {
            // A plan or directory that suddenly names nobody is broken: nothing is deleted.
            if !cache.calendars.is_empty() {
                outcome
                    .errors
                    .push("nobody gets a calendar: none removed".to_owned());
            }
        } else if let Err(e) = remove_unwanted(nc, cache, wanted, &mut outcome).await {
            outcome.errors.push(format!("the calendar list: {e:#}"));
        }
    }
    outcome
}

/// People whose calendar is shared with them: person ID → calendar name.
pub fn ready(cache: &Cache, wanted: &BTreeMap<String, Wanted>) -> BTreeMap<PersonId, String> {
    wanted
        .iter()
        .filter(|(login, _)| cache.calendars.get(*login).is_some_and(|c| c.shared))
        .flat_map(|(_, w)| w.persons.iter().map(|p| (p.clone(), w.name.clone())))
        .collect()
}

/// Where a resident finds their calendar (selectors, `!ical`, the note in their chat).
pub fn where_it_is(name: &str, web_url: Option<&str>) -> String {
    let app = web_url.map_or_else(
        || "the cloud's Calendar app".to_owned(),
        |url| format!("the cloud's [Calendar app]({url})"),
    );
    format!(
        "🗓 Your turns are in your own calendar in the cloud, **{name}**: in {app}, \
         or on your phone with DAVx⁵ (Android) or as a CalDAV account (iPhone). \
         It changes by itself when your turns do."
    )
}

/// Nobody's turns are in a calendar the bot keeps (`state.nextcloud_calendars` emptied):
/// whether that changed anything.
pub fn forget_all(state: &mut State) -> bool {
    if state.nextcloud_calendars.is_empty() {
        return false;
    }
    state.nextcloud_calendars.clear();
    true
}

/// The calendars are not kept (no `[nextcloud]`, no `[directory]`, no app password): who was
/// told their turns are in the cloud would never hear of a change, so selectors and `!ical`
/// stop saying so (`!ical` sends a file).
pub async fn not_managed(ctx: &BotContext, client: &Client) {
    let _operation = ctx.operations.lock().await;
    let changed = {
        let mut state = ctx.state.lock().await;
        let changed = forget_all(&mut state);
        if changed {
            if let Err(e) = state.save(&ctx.state_path).await {
                warn!("Calendars: saving the state: {e:#}");
            }
        }
        changed
    };
    if changed {
        info!("Calendars: not kept any more; !ical sends files again");
        crate::onboarding::refresh_all_selectors(ctx, client).await;
    }
}

/// Keep the calendars in line with the plan, until the bot stops.
pub async fn run(ctx: BotContext, client: Client, cfg: NextcloudConfig) {
    if ctx.config.directory.is_none() {
        warn!("Calendars: [nextcloud] needs [directory] (who has a house account); no calendars");
        not_managed(&ctx, &client).await;
        return;
    }
    let password = std::env::var(&cfg.password_env)
        .ok()
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty());
    let Some(password) = password else {
        warn!(
            "Calendars: no app password in {}; no calendars, !ical sends files",
            cfg.password_env
        );
        not_managed(&ctx, &client).await;
        return;
    };
    let nc = match Nextcloud::new(&cfg.account_url, &cfg.username, &password) {
        Ok(nc) => nc,
        Err(e) => {
            warn!("Calendars: {e:#}; no calendars");
            not_managed(&ctx, &client).await;
            return;
        }
    };
    let weeks = cfg.weeks.clamp(1, 104) as usize;
    let web_url = cfg.web_url();
    let cache_path = ctx
        .state_path
        .parent()
        .map(|d| d.join("nextcloud.json"))
        .unwrap_or_else(|| "nextcloud.json".into());
    let mut cache: Cache = mxbot_common::persist::load_json_or_default(&cache_path)
        .await
        .unwrap_or_else(|e| {
            warn!("Calendars: {e:#}; starting from what the server has");
            Cache::default()
        });
    let mut next_full = tokio::time::Instant::now();
    let mut seen: Option<(String, NaiveDate, usize)> = None;
    loop {
        let logins = ctx.logins.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(logins) = logins {
            let today = crate::state::today();
            let fingerprint = ctx.state.lock().await.schedule_fingerprint.clone();
            let key = (fingerprint, today, std::sync::Arc::as_ptr(&logins) as usize);
            let full = tokio::time::Instant::now() >= next_full;
            // the plan is only rendered when something may have changed
            if full || seen.as_ref() != Some(&key) {
                let want = wanted(&*ctx.state.lock().await, &logins, weeks, today);
                let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
                let outcome = reconcile(&nc, &mut cache, &want, full, &now).await;
                if outcome.written + outcome.deleted > 0 || !outcome.removed.is_empty() {
                    info!(
                        written = outcome.written,
                        deleted = outcome.deleted,
                        removed = outcome.removed.len(),
                        full,
                        "Calendars: brought in line with the plan"
                    );
                }
                for e in &outcome.errors {
                    warn!("Calendars: {e}");
                }
                if let Err(e) = mxbot_common::persist::save_json_atomic(&cache_path, &cache).await {
                    warn!("Calendars: saving {}: {e:#}", cache_path.display());
                }
                if outcome.errors.is_empty() {
                    seen = Some(key);
                    if full {
                        next_full = tokio::time::Instant::now() + cfg.refresh();
                    }
                } else {
                    seen = None;
                    next_full = next_full.min(tokio::time::Instant::now() + RETRY);
                }
                publish(&ctx, &client, &mut cache, &want, web_url.as_deref()).await;
                if let Err(e) = mxbot_common::persist::save_json_atomic(&cache_path, &cache).await {
                    warn!("Calendars: saving {}: {e:#}", cache_path.display());
                }
            }
        }
        tokio::time::sleep(TICK).await;
    }
}

/// Who has a calendar now (selectors and `!ical` read it), and a note in the private chat of
/// everyone whose calendar is new. The notes are sent outside `ctx.operations` (a slow
/// homeserver holds up no command), and a resident counts as told only once one arrived.
async fn publish(
    ctx: &BotContext,
    client: &Client,
    cache: &mut Cache,
    want: &BTreeMap<String, Wanted>,
    web_url: Option<&str>,
) {
    let ready = ready(cache, want);
    {
        let _operation = ctx.operations.lock().await;
        let changed = {
            let mut state = ctx.state.lock().await;
            if state.nextcloud_calendars == ready {
                false
            } else {
                state.nextcloud_calendars = ready;
                if let Err(e) = state.save(&ctx.state_path).await {
                    warn!("Calendars: saving the state: {e:#}");
                }
                true
            }
        };
        if changed {
            crate::onboarding::refresh_all_selectors(ctx, client).await;
        }
    }
    announce(cache, want, |w| {
        tell(ctx, client, w.persons.clone(), w.name.clone(), web_url)
    })
    .await;
}

/// Tell everyone whose calendar is shared with them and who was not told yet, with `tell`
/// (true: a note arrived); only they count as told, the others are tried again next time.
async fn announce<F, Fut>(cache: &mut Cache, want: &BTreeMap<String, Wanted>, mut tell: F)
where
    F: FnMut(&Wanted) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for (login, w) in want {
        if !cache
            .calendars
            .get(login)
            .is_some_and(|c| c.shared && !c.announced)
        {
            continue;
        }
        if tell(w).await {
            if let Some(cached) = cache.calendars.get_mut(login) {
                cached.announced = true;
            }
        }
    }
}

/// The note about their calendar in the private chat of one of the resident's accounts: whether
/// one arrived.
async fn tell(
    ctx: &BotContext,
    client: &Client,
    persons: Vec<PersonId>,
    name: String,
    web_url: Option<&str>,
) -> bool {
    let accounts: Vec<OwnedUserId> = {
        let state = ctx.state.lock().await;
        persons
            .iter()
            .filter_map(|p| state.person_by_id(p))
            .filter_map(|p| p.matrix_id.as_deref())
            .filter_map(|m| OwnedUserId::try_from(m).ok())
            .collect()
    };
    let text = where_it_is(&name, web_url);
    for account in accounts {
        let Some(chat) = crate::help_board::private_chat(ctx, client, &account).await else {
            continue;
        };
        let content = crate::format::intentional(crate::format::mentionify(&text));
        match mxbot_common::send::send_with_retry(&chat, content).await {
            Ok(_) => return true,
            Err(e) => warn!("Calendars: telling {account}: {e}"),
        }
    }
    false
}

#[cfg(test)]
pub mod fake {
    //! A small CalDAV server with Nextcloud's sharing, in memory: what the bot's calendars
    //! look like from the outside.
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{Arc, Mutex},
    };

    use mxbot_common::calendar::caldav::{xml_escape, xml_unescape};
    use wiremock::{matchers::any, Mock, MockServer, Request, Respond, ResponseTemplate};

    pub const HOME: &str = "/remote.php/dav/calendars/cleaning-bot/";

    #[derive(Default, Debug, Clone)]
    pub struct Calendar {
        pub name: String,
        /// principal → may write
        pub shares: BTreeMap<String, bool>,
        /// file name → (etag, body)
        pub events: BTreeMap<String, (String, String)>,
    }

    #[derive(Default, Debug)]
    pub struct World {
        /// Nextcloud users (a share with anyone else does nothing).
        pub users: BTreeSet<String>,
        pub calendars: BTreeMap<String, Calendar>,
        /// Requests that change something (method and path).
        pub writes: Vec<String>,
        next: u64,
    }

    #[derive(Clone, Default)]
    pub struct FakeCalDav(pub Arc<Mutex<World>>);

    impl FakeCalDav {
        pub async fn start(users: &[&str]) -> (MockServer, Self) {
            let fake = Self::default();
            fake.0.lock().unwrap().users = users.iter().map(|u| u.to_string()).collect();
            let server = MockServer::start().await;
            Mock::given(any())
                .respond_with(fake.clone())
                .mount(&server)
                .await;
            (server, fake)
        }

        pub fn world(&self) -> std::sync::MutexGuard<'_, World> {
            self.0.lock().unwrap()
        }

        pub fn take_writes(&self) -> Vec<String> {
            std::mem::take(&mut self.world().writes)
        }
    }

    fn multistatus(inner: &str) -> ResponseTemplate {
        ResponseTemplate::new(207).set_body_string(format!(
            r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:cal="urn:ietf:params:xml:ns:caldav">{inner}</d:multistatus>"#
        ))
    }

    fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
        let start = s.find(open)? + open.len();
        let end = s[start..].find(close)? + start;
        Some(&s[start..end])
    }

    impl Respond for FakeCalDav {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let mut w = self.0.lock().unwrap();
            let m = req.method.as_str().to_owned();
            let path = req.url.path().to_owned();
            let body = String::from_utf8_lossy(&req.body).to_string();
            let header = |name: &str| {
                req.headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            if m != "PROPFIND" {
                w.writes.push(format!("{m} {path}"));
            }
            let Some(rest) = path.strip_prefix(HOME) else {
                return ResponseTemplate::new(404);
            };
            let (slug, file) = match rest.split_once('/') {
                Some((s, f)) => (s.to_owned(), f.to_owned()),
                None if rest.is_empty() => (String::new(), String::new()),
                None => return ResponseTemplate::new(404),
            };
            let calendar_xml = |slug: &str, c: &Calendar| {
                format!("<d:response><d:href>{HOME}{slug}/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><cal:calendar/></d:resourcetype><d:displayname>{}</d:displayname><oc:owner-principal>principals/users/cleaning-bot</oc:owner-principal></d:prop></d:propstat></d:response>", xml_escape(&c.name))
            };
            match (m.as_str(), slug.is_empty(), file.is_empty()) {
                ("PROPFIND", true, _) => {
                    let mut out = format!("<d:response><d:href>{HOME}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>");
                    for (s, c) in &w.calendars {
                        out.push_str(&calendar_xml(s, c));
                    }
                    multistatus(&out)
                }
                ("PROPFIND", false, true) => {
                    let Some(c) = w.calendars.get(&slug) else {
                        return ResponseTemplate::new(404);
                    };
                    if header("depth").as_deref() == Some("1") {
                        let mut out = calendar_xml(&slug, c);
                        for (f, (etag, _)) in &c.events {
                            out.push_str(&format!("<d:response><d:href>{HOME}{slug}/{f}</d:href><d:propstat><d:prop><d:getetag>{}</d:getetag></d:prop></d:propstat></d:response>", xml_escape(etag).replace('"', "&quot;")));
                        }
                        multistatus(&out)
                    } else if body.contains("invite") {
                        let users: String = c.shares.iter().map(|(p, write)| {
                            let access = if *write { "<oc:read-write/>" } else { "<oc:read/>" };
                            format!("<oc:user><d:href>{p}</d:href><oc:invite-accepted/><oc:access>{access}</oc:access></oc:user>")
                        }).collect();
                        multistatus(&format!("<d:response><d:href>{HOME}{slug}/</d:href><d:propstat><d:prop><oc:invite>{users}</oc:invite></d:prop></d:propstat></d:response>"))
                    } else {
                        multistatus(&calendar_xml(&slug, c))
                    }
                }
                ("MKCALENDAR", false, true) => {
                    if w.calendars.contains_key(&slug) {
                        return ResponseTemplate::new(405);
                    }
                    let name = between(&body, "<d:displayname>", "</d:displayname>")
                        .map(xml_unescape)
                        .unwrap_or_default();
                    w.calendars.insert(
                        slug,
                        Calendar {
                            name,
                            ..Default::default()
                        },
                    );
                    ResponseTemplate::new(201)
                }
                ("PROPPATCH", false, true) => {
                    let name = between(&body, "<d:displayname>", "</d:displayname>")
                        .map(xml_unescape)
                        .unwrap_or_default();
                    match w.calendars.get_mut(&slug) {
                        Some(c) => {
                            c.name = name;
                            multistatus("")
                        }
                        None => ResponseTemplate::new(404),
                    }
                }
                ("POST", false, true) => {
                    let users = w.users.clone();
                    let Some(c) = w.calendars.get_mut(&slug) else {
                        return ResponseTemplate::new(404);
                    };
                    if let Some(set) = between(&body, "<oc:set>", "</oc:set>") {
                        let principal =
                            xml_unescape(between(set, "<d:href>", "</d:href>").unwrap_or(""));
                        let exists = principal
                            .strip_prefix("principal:principals/users/")
                            .is_some_and(|u| users.contains(u))
                            || principal.starts_with("principal:principals/groups/");
                        if exists {
                            c.shares.insert(principal, set.contains("read-write"));
                        }
                    }
                    if let Some(remove) = between(&body, "<oc:remove>", "</oc:remove>") {
                        let principal =
                            xml_unescape(between(remove, "<d:href>", "</d:href>").unwrap_or(""));
                        c.shares.remove(&principal);
                    }
                    ResponseTemplate::new(200)
                }
                ("DELETE", false, true) => match w.calendars.remove(&slug) {
                    Some(_) => ResponseTemplate::new(204),
                    None => ResponseTemplate::new(404),
                },
                ("PUT", false, false) => {
                    w.next += 1;
                    let etag = format!("\"e{}\"", w.next);
                    let Some(c) = w.calendars.get_mut(&slug) else {
                        return ResponseTemplate::new(404);
                    };
                    let existing = c.events.get(&file).map(|(e, _)| e.clone());
                    if header("if-none-match").as_deref() == Some("*") && existing.is_some() {
                        return ResponseTemplate::new(412);
                    }
                    if let Some(want) = header("if-match") {
                        if existing.as_deref() != Some(want.as_str()) {
                            return ResponseTemplate::new(412);
                        }
                    }
                    c.events.insert(file, (etag.clone(), body));
                    ResponseTemplate::new(if existing.is_some() { 204 } else { 201 })
                        .insert_header("ETag", etag.as_str())
                }
                ("DELETE", false, false) => {
                    let Some(c) = w.calendars.get_mut(&slug) else {
                        return ResponseTemplate::new(404);
                    };
                    let Some((etag, _)) = c.events.get(&file) else {
                        return ResponseTemplate::new(404);
                    };
                    if header("if-match").is_some_and(|want| want != *etag) {
                        return ResponseTemplate::new(412);
                    }
                    c.events.remove(&file);
                    ResponseTemplate::new(204)
                }
                _ => ResponseTemplate::new(405),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeCalDav, HOME};
    use super::*;
    use crate::domain::{CleaningGroup, Person};
    use mxbot_common::matrix_sdk::ruma::UserId;

    const NOW: &str = "20261022T120000Z";

    fn nc(server: &wiremock::MockServer) -> Nextcloud {
        Nextcloud::new(
            &format!("{}{HOME}", server.uri()),
            "cleaning-bot",
            "app-password",
        )
        .unwrap()
    }

    /// anna and ben (house accounts) and carol (matrix.org only) clean the kitchen in turn.
    fn house() -> (State, Logins) {
        let mut state = State::default();
        state.created_at = Some(chrono::Utc::now());
        let mut kitchen = CleaningGroup::new("Kitchen");
        for (mxid, name) in [
            ("@anna:house.org", "Anna"),
            ("@ben:house.org", "Ben"),
            ("@carol:matrix.org", "Carol"),
        ] {
            let mut p = Person::new_matrix(mxid);
            p.display_name = name.into();
            kitchen.member_ids.push(p.id.clone());
            state.persons.push(p);
        }
        state.cleaning_groups.push(kitchen);
        for event in crate::resolver::materialize(&state, 6) {
            state.apply_event(event).unwrap();
        }
        let logins: Logins = [
            ("@anna:house.org", "anna"),
            ("@anna:matrix.org", "anna"),
            ("@ben:house.org", "ben"),
        ]
        .into_iter()
        .map(|(m, l)| (UserId::parse(m).unwrap(), l.to_owned()))
        .collect();
        (state, logins)
    }

    fn uids(fake: &FakeCalDav, slug: &str) -> BTreeSet<String> {
        fake.world().calendars[slug]
            .events
            .keys()
            .map(|f| f.trim_end_matches(".ics").to_owned())
            .collect()
    }

    fn person_id(state: &State, mxid: &str) -> PersonId {
        state.person_by_matrix_id(mxid).unwrap().id.clone()
    }

    #[tokio::test]
    async fn every_resident_with_a_house_account_gets_their_turns_shared_with_them_alone() {
        let (state, logins) = house();
        let (server, fake) = FakeCalDav::start(&["anna", "ben"]).await;
        let nc = nc(&server);
        let today = crate::state::today();
        let want = wanted(&state, &logins, 6, today);
        assert_eq!(
            want.keys().collect::<Vec<_>>(),
            ["anna", "ben"],
            "carol has no house account: no calendar"
        );
        let mut cache = Cache::default();
        let outcome = reconcile(&nc, &mut cache, &want, true, NOW).await;
        assert!(outcome.errors.is_empty(), "{outcome:?}");

        let anna = person_id(&state, "@anna:house.org");
        {
            let world = fake.world();
            let cal = &world.calendars["cleaning-anna"];
            assert_eq!(cal.name, "Cleaning: Anna");
            assert_eq!(
                cal.shares,
                BTreeMap::from([("principal:principals/users/anna".to_owned(), false)])
            );
            let body = &cal.events.values().next().unwrap().1;
            assert!(body.contains("SUMMARY:🧹 Kitchen"), "{body}");
            assert!(body.contains(&format!("DTSTAMP:{NOW}")), "{body}");
            assert!(!body.contains("METHOD:"), "{body}");
        }
        let snapshot = build_schedule(&state, 6);
        let expected: BTreeSet<String> = snapshot
            .for_person(&anna)
            .iter()
            .filter(|a| a.end >= today)
            .map(|a| a.uid.clone())
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(
            uids(&fake, "cleaning-anna"),
            expected,
            "the UIDs of an !ical file"
        );
        assert_eq!(
            ready(&cache, &want),
            BTreeMap::from([
                (anna, "Cleaning: Anna".to_owned()),
                (person_id(&state, "@ben:house.org"), "Cleaning: Ben".into())
            ])
        );
    }

    #[tokio::test]
    async fn nothing_changed_nothing_written_and_a_full_round_only_reads() {
        let (state, logins) = house();
        let (server, fake) = FakeCalDav::start(&["anna", "ben"]).await;
        let nc = nc(&server);
        let want = wanted(&state, &logins, 6, crate::state::today());
        let mut cache = Cache::default();
        reconcile(&nc, &mut cache, &want, true, NOW).await;
        fake.take_writes();
        let again = wanted(&state, &logins, 6, crate::state::today());
        assert_eq!(again, want, "the same plan renders the same events");
        let outcome = reconcile(&nc, &mut cache, &again, false, NOW).await;
        assert_eq!(outcome, Outcome::default());
        let outcome = reconcile(&nc, &mut cache, &again, true, NOW).await;
        assert_eq!(outcome, Outcome::default());
        assert!(fake.take_writes().is_empty(), "only reads");
    }

    #[tokio::test]
    async fn a_swap_moves_the_turn_from_one_calendar_to_the_other() {
        let (mut state, logins) = house();
        let (server, fake) = FakeCalDav::start(&["anna", "ben"]).await;
        let nc = nc(&server);
        let today = crate::state::today();
        let mut cache = Cache::default();
        reconcile(
            &nc,
            &mut cache,
            &wanted(&state, &logins, 6, today),
            true,
            NOW,
        )
        .await;
        let (anna, ben) = (
            person_id(&state, "@anna:house.org"),
            person_id(&state, "@ben:house.org"),
        );
        let before_anna = uids(&fake, "cleaning-anna");
        let before_ben = uids(&fake, "cleaning-ben");
        // anna's first coming turn and ben's swap
        let (first_anna, first_ben) = {
            let upcoming = |p: &PersonId| {
                state
                    .slot_assignments
                    .iter()
                    .position(|a| a.person_id.as_ref() == Some(p))
                    .unwrap()
            };
            (upcoming(&anna), upcoming(&ben))
        };
        state.slot_assignments[first_anna].person_id = Some(ben.clone());
        state.slot_assignments[first_ben].person_id = Some(anna.clone());
        fake.take_writes();
        let want = wanted(&state, &logins, 6, today);
        let outcome = reconcile(&nc, &mut cache, &want, false, NOW).await;
        assert!(outcome.errors.is_empty(), "{outcome:?}");
        assert_eq!((outcome.written, outcome.deleted), (2, 2), "{outcome:?}");
        let after_anna = uids(&fake, "cleaning-anna");
        assert_ne!(after_anna, before_anna);
        assert_eq!(after_anna.len(), before_anna.len());
        assert_ne!(uids(&fake, "cleaning-ben"), before_ben);
        assert_eq!(
            after_anna,
            want["anna"].events.keys().cloned().collect::<BTreeSet<_>>()
        );
        // "Others" names in the description stay in step too: a full round finds nothing
        let outcome = reconcile(&nc, &mut cache, &want, true, NOW).await;
        assert_eq!(outcome, Outcome::default());
    }

    #[tokio::test]
    async fn past_turns_go_and_a_name_change_renames_the_calendar() {
        let (mut state, logins) = house();
        let (server, fake) = FakeCalDav::start(&["anna", "ben"]).await;
        let nc = nc(&server);
        let today = crate::state::today();
        let mut cache = Cache::default();
        reconcile(
            &nc,
            &mut cache,
            &wanted(&state, &logins, 6, today),
            true,
            NOW,
        )
        .await;
        let all = uids(&fake, "cleaning-anna");
        state.persons[0].display_name = "Anna K.".into();
        let later = wanted(&state, &logins, 6, today + chrono::Duration::days(15));
        let outcome = reconcile(&nc, &mut cache, &later, false, NOW).await;
        assert!(outcome.errors.is_empty(), "{outcome:?}");
        assert!(outcome.deleted > 0);
        let left = uids(&fake, "cleaning-anna");
        assert!(left.len() < all.len() && left.is_subset(&all));
        assert_eq!(
            fake.world().calendars["cleaning-anna"].name,
            "Cleaning: Anna K."
        );
    }

    #[tokio::test]
    async fn someone_who_moved_out_loses_the_calendar_but_never_while_nobody_is_wanted() {
        let (state, mut logins) = house();
        let (server, fake) = FakeCalDav::start(&["anna", "ben"]).await;
        let nc = nc(&server);
        let today = crate::state::today();
        let mut cache = Cache::default();
        reconcile(
            &nc,
            &mut cache,
            &wanted(&state, &logins, 6, today),
            true,
            NOW,
        )
        .await;
        // somebody else's calendar of the account is left alone
        fake.world()
            .calendars
            .insert("personal".into(), Default::default());

        // ben is gone from the directory: a quick round deletes nothing
        logins.retain(|_, l| l != "ben");
        let want = wanted(&state, &logins, 6, today);
        let outcome = reconcile(&nc, &mut cache, &want, false, NOW).await;
        assert!(outcome.removed.is_empty());
        assert!(fake.world().calendars.contains_key("cleaning-ben"));
        // the full one does: unshared first, then deleted
        fake.take_writes();
        let outcome = reconcile(&nc, &mut cache, &want, true, NOW).await;
        assert_eq!(outcome.removed, ["cleaning-ben"]);
        let writes = fake.take_writes();
        assert_eq!(
            writes,
            [
                format!("POST {HOME}cleaning-ben/"),
                format!("DELETE {HOME}cleaning-ben/")
            ]
        );
        assert!(!cache.calendars.contains_key("ben"));
        assert_eq!(
            fake.world().calendars.keys().collect::<Vec<_>>(),
            ["cleaning-anna", "personal"]
        );

        // a directory or plan naming nobody removes nothing
        let outcome = reconcile(&nc, &mut cache, &BTreeMap::new(), true, NOW).await;
        assert!(outcome.removed.is_empty() && !outcome.errors.is_empty());
        assert!(fake.world().calendars.contains_key("cleaning-anna"));
    }

    #[tokio::test]
    async fn without_a_nextcloud_account_yet_the_calendar_waits_and_the_link_stays() {
        let (state, logins) = house();
        let (server, fake) = FakeCalDav::start(&["anna"]).await; // ben never logged in there
        let nc = nc(&server);
        let want = wanted(&state, &logins, 6, crate::state::today());
        let mut cache = Cache::default();
        let outcome = reconcile(&nc, &mut cache, &want, true, NOW).await;
        assert!(outcome.errors.is_empty(), "{outcome:?}");
        assert!(fake.world().calendars["cleaning-ben"].shares.is_empty());
        let ready = ready(&cache, &want);
        assert!(ready.contains_key(&person_id(&state, "@anna:house.org")));
        assert!(!ready.contains_key(&person_id(&state, "@ben:house.org")));
        // his first login: the next full round shares it
        fake.world().users.insert("ben".into());
        reconcile(&nc, &mut cache, &want, true, NOW).await;
        assert!(super::ready(&cache, &want).contains_key(&person_id(&state, "@ben:house.org")));
    }

    #[tokio::test]
    async fn an_event_changed_or_lost_on_the_server_is_put_back_by_the_full_round() {
        let (state, logins) = house();
        let (server, fake) = FakeCalDav::start(&["anna", "ben"]).await;
        let nc = nc(&server);
        let want = wanted(&state, &logins, 6, crate::state::today());
        let mut cache = Cache::default();
        reconcile(&nc, &mut cache, &want, true, NOW).await;
        {
            let mut world = fake.world();
            let cal = world.calendars.get_mut("cleaning-anna").unwrap();
            let first = cal.events.keys().next().unwrap().clone();
            cal.events.get_mut(&first).unwrap().0 = "\"changed\"".into();
            let last = cal.events.keys().last().unwrap().clone();
            cal.events.remove(&last);
            cal.shares
                .insert("principal:principals/users/eve".into(), true);
        }
        let outcome = reconcile(&nc, &mut cache, &want, true, NOW).await;
        assert!(outcome.errors.is_empty(), "{outcome:?}");
        assert_eq!(outcome.written, 2);
        assert_eq!(
            fake.world().calendars["cleaning-anna"].shares,
            BTreeMap::from([("principal:principals/users/anna".to_owned(), false)])
        );
    }

    #[tokio::test]
    async fn a_round_that_would_remove_most_calendars_removes_none() {
        let (state, logins) = house();
        let (server, fake) = FakeCalDav::start(&["anna", "ben"]).await;
        let nc = nc(&server);
        let want = wanted(&state, &logins, 6, crate::state::today());
        let mut cache = Cache::default();
        reconcile(&nc, &mut cache, &want, true, NOW).await;
        // three more residents' calendars from before, none of them wanted now: 3 of 5 at once
        for old in ["cleaning-dora", "cleaning-emil", "cleaning-finn"] {
            fake.world()
                .calendars
                .insert(old.into(), Default::default());
        }
        assert_eq!(max_removals(5), 2);
        fake.take_writes();
        let outcome = reconcile(&nc, &mut cache, &want, true, NOW).await;
        assert!(outcome.removed.is_empty(), "{outcome:?}");
        assert_eq!(
            outcome.errors,
            ["3 of 5 calendars would go at once (at most 2): none removed"]
        );
        assert!(fake.take_writes().is_empty(), "nothing unshared or deleted");
        assert_eq!(fake.world().calendars.len(), 5);
        assert_eq!(cache.calendars.len(), 2, "the cache keeps what it knew");
        // two of four: people moving out, removed
        fake.world().calendars.remove("cleaning-finn");
        let outcome = reconcile(&nc, &mut cache, &want, true, NOW).await;
        assert!(outcome.errors.is_empty(), "{outcome:?}");
        assert_eq!(outcome.removed, ["cleaning-dora", "cleaning-emil"]);
        // a big house: a third at once
        assert_eq!(max_removals(0), 2);
        assert_eq!(max_removals(9), 3);
        assert_eq!(max_removals(30), 10);
    }

    #[test]
    fn two_logins_with_one_calendar_get_none() {
        let (state, _) = house();
        let logins = |pairs: &[(&str, &str)]| -> Logins {
            pairs
                .iter()
                .map(|(m, l)| (UserId::parse(*m).unwrap(), l.to_string()))
                .collect()
        };
        let today = crate::state::today();
        // "a b" and "a-b" both make cleaning-a-b
        let clash = logins(&[("@anna:house.org", "a b"), ("@ben:house.org", "a-b")]);
        assert_eq!(slug("a b"), slug("a-b"));
        assert!(wanted(&state, &clash, 6, today).is_empty());
        // so do Ben and ben, on a server that ignores case; carol keeps hers
        let clash = logins(&[
            ("@anna:house.org", "Ben"),
            ("@ben:house.org", "ben"),
            ("@carol:matrix.org", "carol"),
        ]);
        let want = wanted(&state, &clash, 6, today);
        assert_eq!(want.keys().collect::<Vec<_>>(), ["carol"]);
        // one resident's two accounts with the same login share theirs, as before
        let (_, house_logins) = house();
        assert_eq!(wanted(&state, &house_logins, 6, today).len(), 2);
    }

    #[tokio::test]
    async fn a_resident_counts_as_told_only_once_the_note_arrived() {
        let (state, logins) = house();
        let (server, _fake) = FakeCalDav::start(&["anna", "ben"]).await;
        let nc = nc(&server);
        let want = wanted(&state, &logins, 6, crate::state::today());
        let mut cache = Cache::default();
        reconcile(&nc, &mut cache, &want, true, NOW).await;
        let told = |cache: &Cache| {
            cache
                .calendars
                .iter()
                .filter(|(_, c)| c.announced)
                .map(|(l, _)| l.clone())
                .collect::<Vec<_>>()
        };
        // ben's note fails (homeserver down, no private chat): he is tried again
        let mut asked = Vec::new();
        announce(&mut cache, &want, |w| {
            asked.push(w.slug.clone());
            std::future::ready(w.slug == "cleaning-anna")
        })
        .await;
        assert_eq!(asked, ["cleaning-anna", "cleaning-ben"]);
        assert_eq!(told(&cache), ["anna"]);
        let mut asked = Vec::new();
        announce(&mut cache, &want, |w| {
            asked.push(w.slug.clone());
            std::future::ready(true)
        })
        .await;
        assert_eq!(asked, ["cleaning-ben"], "anna is not told twice");
        assert_eq!(told(&cache), ["anna", "ben"]);
        // not shared yet: nobody to tell
        cache.calendars.get_mut("ben").unwrap().announced = false;
        cache.calendars.get_mut("ben").unwrap().shared = false;
        announce(&mut cache, &want, |_| async { panic!("nothing to tell") }).await;
    }

    #[test]
    fn without_calendars_kept_nobody_is_told_their_turns_are_in_the_cloud() {
        let (mut state, _) = house();
        assert!(!forget_all(&mut state));
        let anna = person_id(&state, "@anna:house.org");
        state
            .nextcloud_calendars
            .insert(anna, "Cleaning: Anna".into());
        assert!(forget_all(&mut state));
        assert!(state.nextcloud_calendars.is_empty());
    }

    #[test]
    fn slugs_are_safe() {
        assert_eq!(slug("anna"), "cleaning-anna");
        assert_eq!(slug("Ben.K_2"), "cleaning-Ben.K_2");
        assert_eq!(slug("a b/c@d"), "cleaning-a-b-c-d");
        assert!(where_it_is(
            "Cleaning: Anna",
            Some("https://cloud.example.org/apps/calendar")
        )
        .contains("[Calendar app](https://cloud.example.org/apps/calendar)"));
    }
}
