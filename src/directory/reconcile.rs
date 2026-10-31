//! The plan follows the house's directory (`[directory]`), once per good read:
//!
//! 1. **Link**: each resident's person in the plan is found by the directory id, else by one
//!    of their recognised accounts, and the id is stored (`PersonDirectoryLinked`). Two people
//!    in the plan that are one resident and both in use: nothing is merged, the admins are
//!    told once.
//! 2. **Address**: a resident who chose an account (`Contact::Chosen`) is written to there:
//!    the person's Matrix ID moves to it (`PersonMatrixLinked`), unless another person in the
//!    plan has it (the admins are told once). Not chosen, or chosen wrongly: it stays.
//! 3. **Join**: a group with a `directory_group` (a kitchen) takes in that group's members
//!    (who are residents too, when `residents_group` is set), the way an admin's
//!    `!member add` does. Someone not in the plan yet is added by directory id with their
//!    chosen account, else their house account, as the address.
//! 4. **Leave**: a linked person who is no longer a resident (moved out, deactivated, gone
//!    from the directory) leaves every group; one no longer in a group's `directory_group`
//!    leaves that group. Only after two good reads in a row ask for it, the way an admin's
//!    `!member remove` does (`apply_group_departure`): this week's turn stays, later ones are
//!    handed on, completions and statistics are kept. One admin notice per person.
//!
//! An account two or more residents claim counts for none of them; the admins hear of each
//! once (who claims it).
//!
//! Guards: a read older than the directory's `stale_after` changes nothing, a failed read
//! changes nothing (it never gets here), a group the directory doesn't have is left alone,
//! an empty residents group removes nobody, and more departures from one group in one round
//! than `max(2, 20 % of its people)` ([`mass_limit`]) hold that group's departures (a
//! resident moving out counts in each group they leave): they don't leave, the admins are
//! told once, and `!directory confirm <digest>` lets exactly the held ones go if the
//! directory still says so (the digest is in the notice and in `!directory`). The first
//! departures after following began (a fresh upgrade, or `residents_group` switched on or
//! changed: `DirectoryMemory::reviewed`) are all held that way, however few. People in the
//! plan without a directory id are never touched.
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, SystemTime},
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use mxbot_common::{
    directory::{Contact, People, Person as Resident, Snapshot},
    matrix_sdk::{
        ruma::{events::room::message::RoomMessageEventContent, OwnedUserId},
        Client,
    },
};
use tracing::{info, warn};

use super::{plan_person, split_people, Match};
use crate::{
    analytics::DomainEvent,
    commands::{apply_group_departure, apply_group_join, group_name_of},
    config::DirectorySection,
    domain::{GroupId, PersonId},
    state::{DirectoryDeparture, State},
    BotContext,
};

/// What a round follows.
#[derive(Clone, Debug)]
pub struct Rules {
    /// Empty: nobody leaves for not being a resident.
    pub residents_group: String,
    /// A read older than this changes nothing.
    pub stale_after: Duration,
}

impl Rules {
    pub fn of(cfg: &DirectorySection) -> Rules {
        Rules {
            residents_group: cfg.residents_group.clone(),
            stale_after: cfg.reader.stale_after(),
        }
    }
}

/// Why someone leaves a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    Gone,
    Deactivated,
    /// No longer in the residents group (moved out).
    NotResident(String),
    /// No longer in the group's `directory_group` (another kitchen).
    NotInGroup(String),
}

impl Why {
    fn text(&self) -> String {
        match self {
            Why::Gone => "is no longer in the directory".into(),
            Why::Deactivated => "was deactivated in the directory".into(),
            Why::NotResident(group) => format!("is no longer in «{group}» (moved out)"),
            Why::NotInGroup(group) => format!("is no longer in «{group}»"),
        }
    }
}

/// What a round did.
#[derive(Debug, Default, PartialEq)]
pub struct Round {
    /// For the admins, one per departure (and per join, link problem or hold).
    pub notices: Vec<String>,
    /// The plan changed (joins, departures, addresses): plan and selectors are refreshed.
    pub plan_changed: bool,
}

/// How many departures one round may carry out of `group`: `max(2, 20 %)` of its active
/// people (as mxbot-common's `membership::removal_limit` for a room).
pub fn mass_limit(state: &State, group: &GroupId) -> usize {
    let members = state.group_by_id(group).map_or(0, |g| {
        g.member_ids
            .iter()
            .filter(|p| state.person_by_id(p).is_some_and(|p| p.active))
            .count()
    });
    (members / 5).max(2)
}

/// The groups more people would leave at once than [`mass_limit`]: group → (leaving, limit).
fn over_limit(
    state: &State,
    departures: &BTreeSet<DirectoryDeparture>,
) -> BTreeMap<GroupId, (usize, usize)> {
    let mut leaving: BTreeMap<&GroupId, usize> = BTreeMap::new();
    for (_, group) in departures {
        *leaving.entry(group).or_default() += 1;
    }
    leaving
        .into_iter()
        .map(|(group, n)| (group.clone(), (n, mass_limit(state, group))))
        .filter(|(_, (n, limit))| n > limit)
        .collect()
}

/// The fingerprint of held departures: what `!directory confirm <digest>` names, so a
/// confirmation goes only for the departures the admin saw.
pub fn held_digest(held: &BTreeSet<DirectoryDeparture>) -> String {
    mxbot_common::membership::digest(
        held.iter()
            .map(|(person, group)| format!("{person} {group}")),
    )
}

/// Whether `resident` is in `group` now (active people only are in groups).
fn is_in(resident: Option<&Resident>, group: &str) -> bool {
    resident.is_some_and(|r| r.active && r.groups.contains(group))
}

/// Every departure the directory asks for now, with why: a linked person who is no longer a
/// resident leaves every group they are in; one no longer in a group's `directory_group`
/// leaves that group.
pub fn wanted_departures(
    state: &State,
    snapshot: &Snapshot,
    rules: &Rules,
) -> BTreeMap<DirectoryDeparture, Why> {
    let people = &snapshot.people;
    let mut out = BTreeMap::new();
    let residents = rules.residents_group.as_str();
    let follow_residents = !residents.is_empty()
        && snapshot.has_group(residents)
        && people.members(residents).next().is_some();
    if !residents.is_empty() && !follow_residents {
        warn!(
            group = residents,
            "Directory: the residents group is missing or empty; nobody leaves for it"
        );
    }
    if follow_residents {
        for person in state.persons.iter().filter(|p| p.active) {
            let Some(id) = person.directory_id.as_deref() else {
                continue;
            };
            let resident = people.by_id(id);
            if is_in(resident, residents) {
                continue;
            }
            let why = match resident {
                None => Why::Gone,
                Some(r) if !r.active => Why::Deactivated,
                Some(_) => Why::NotResident(residents.to_owned()),
            };
            for group in state
                .cleaning_groups
                .iter()
                .filter(|g| g.member_ids.contains(&person.id))
            {
                out.insert((person.id.clone(), group.id.clone()), why.clone());
            }
        }
    }
    for group in &state.cleaning_groups {
        let Some(wanted) = group.directory_group.as_deref() else {
            continue;
        };
        if group.house_wide || !snapshot.has_group(wanted) {
            continue;
        }
        for person_id in &group.member_ids {
            let Some(id) = state
                .person_by_id(person_id)
                .filter(|p| p.active)
                .and_then(|p| p.directory_id.as_deref())
            else {
                continue;
            };
            if !is_in(people.by_id(id), wanted) {
                out.entry((person_id.clone(), group.id.clone()))
                    .or_insert_with(|| Why::NotInGroup(wanted.to_owned()));
            }
        }
    }
    out
}

/// The people `departures` concern.
fn persons_of(departures: &BTreeSet<DirectoryDeparture>) -> BTreeSet<&PersonId> {
    departures.iter().map(|(p, _)| p).collect()
}

/// One round on `snapshot` (see the module): changes `state`, never saves it.
pub fn round(
    ctx: &BotContext,
    state: &mut State,
    snapshot: &Snapshot,
    rules: &Rules,
    now: SystemTime,
) -> Round {
    let mut out = Round::default();
    if snapshot.age(now) > rules.stale_after {
        warn!("Directory: the last read is too old; following nothing");
        return out;
    }
    let people = &snapshot.people;
    let mut told: BTreeSet<String> = BTreeSet::new();
    conflicts(people, state, &mut told, &mut out);
    link(state, people, &mut told, &mut out);
    addresses(state, people, &mut told, &mut out);
    joins(ctx, state, snapshot, rules, &mut out);

    // Departures: what two good reads in a row ask for.
    let wanted = wanted_departures(state, snapshot, rules);
    let now_missing: BTreeSet<DirectoryDeparture> = wanted.keys().cloned().collect();
    let read_at = DateTime::<Utc>::from(snapshot.read_at);
    let in_a_row = state.directory.missing_at.is_some_and(|before| {
        before < read_at
            && (read_at - before)
                .to_std()
                .is_ok_and(|gap| gap <= rules.stale_after)
    });
    let confirmed: BTreeSet<DirectoryDeparture> = if in_a_row {
        now_missing
            .intersection(&state.directory.missing)
            .cloned()
            .collect()
    } else {
        BTreeSet::new()
    };
    state.directory.missing = now_missing;
    state.directory.missing_at = Some(read_at);

    // Not reviewed yet under these rules (a fresh upgrade, residents switched on): every
    // departure waits for an admin. A read in a row with the one before that asks for no
    // departure at all (the plan and the directory agree): reviewed.
    let first = state.directory.reviewed.as_deref() != Some(rules.residents_group.as_str());
    if first && in_a_row && wanted.is_empty() {
        state.directory.reviewed = Some(rules.residents_group.clone());
    }
    let over = over_limit(state, &confirmed);
    let held: BTreeSet<DirectoryDeparture> = confirmed
        .iter()
        .filter(|(_, group)| first || over.contains_key(group))
        .cloned()
        .collect();
    let go: BTreeSet<DirectoryDeparture> = confirmed.difference(&held).cloned().collect();
    if held.is_empty() {
        state.directory.held.clear();
    } else if state.directory.held != held {
        warn!(
            departures = held.len(),
            first, "Directory: departures held until an admin confirms"
        );
        out.notices
            .push(held_notice(state, &held, &wanted, &over, first));
        state.directory.held = held;
    }
    if !go.is_empty() {
        out.notices
            .extend(apply_departures(ctx, state, &go, &wanted));
        out.plan_changed = true;
        for d in &go {
            state.directory.missing.remove(d);
        }
    }
    // What is no longer so may be told again when it comes back.
    state.directory.told = told;
    out
}

/// An account two or more residents claim counts for none of them: the admins hear of each
/// once (while it lasts), with who claims it.
fn conflicts(people: &People, state: &State, told: &mut BTreeSet<String>, out: &mut Round) {
    for (account, names) in people.conflicts() {
        let key = format!("conflict:{account}:{}", names.join(","));
        if !state.directory.told.contains(&key) {
            out.notices.push(format!(
                "⚠️ {} is a Matrix account of {} in the directory: it counts for none of them \
                 (nobody's person in the plan is found or addressed by it). Take it off the \
                 one it doesn't belong to.",
                crate::view::user_id_link(account.as_str()),
                names.join(" and ")
            ));
        }
        told.insert(key);
    }
}

/// Step 1: store the directory id of each resident's person in the plan.
fn link(state: &mut State, people: &People, told: &mut BTreeSet<String>, out: &mut Round) {
    for resident in people.active() {
        let found = match plan_person(state, people, resident) {
            Match::Found(person) => Some(person.id.clone()),
            _ => None,
        };
        if let Some(person_id) = found {
            match state.apply_event(DomainEvent::PersonDirectoryLinked {
                person_id: person_id.clone(),
                directory_id: resident.id.clone(),
            }) {
                Ok(()) => {
                    info!(person = %person_id, login = %resident.username, "Directory: linked")
                }
                Err(e) => warn!("Directory: linking {}: {e:#}", resident.username),
            }
        }
        let split = split_people(state, people, resident);
        if split.len() > 1 {
            let key = format!(
                "split:{}:{}",
                resident.id,
                split
                    .iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let who: Vec<String> = split
                .iter()
                .map(|p| crate::commands::person_label(p))
                .collect();
            warn!(login = %resident.username, people = %who.join(", "), "Directory: one resident, several people in the plan; left as they are");
            if !state.directory.told.contains(&key) {
                out.notices.push(format!(
                    "⚠️ {} is {} people in the plan: {}. Nothing was merged: take the extra one \
                     out of its groups (!member remove) and its turns.",
                    resident_name(resident),
                    split.len(),
                    who.join(", ")
                ));
            }
            told.insert(key);
        }
    }
}

/// Step 2: the address of each linked person follows their chosen account.
fn addresses(state: &mut State, people: &People, told: &mut BTreeSet<String>, out: &mut Round) {
    for resident in people.active() {
        let Contact::Chosen(chosen) = people.contact(resident) else {
            continue;
        };
        let (person_id, old) = match plan_person(state, people, resident) {
            Match::Anchored(person) => (person.id.clone(), person.matrix_id.clone()),
            _ => continue,
        };
        if old.as_deref() == Some(chosen.as_str()) {
            continue;
        }
        if let Some(other) = state
            .persons
            .iter()
            .find(|p| p.matrix_id.as_deref() == Some(chosen.as_str()))
        {
            let key = format!("address:{}:{chosen}", resident.id);
            if !state.directory.told.contains(&key) {
                out.notices.push(format!(
                    "⚠️ {} chose {} for the house, but that account is another person in the \
                     plan ({}). They stay at {} until an admin sorts it out.",
                    resident_name(resident),
                    crate::view::user_id_link(chosen.as_str()),
                    crate::commands::person_label(other),
                    old.as_deref()
                        .map(crate::view::user_id_link)
                        .unwrap_or_else(|| "no account".into()),
                ));
            }
            told.insert(key);
            continue;
        }
        match state.apply_event(DomainEvent::PersonMatrixLinked {
            person_id: person_id.clone(),
            matrix_id: chosen.to_string(),
        }) {
            Ok(()) => {
                info!(person = %person_id, from = ?old, to = %chosen, "Directory: the address follows the chosen account");
                // Their welcome was had, whichever account had it.
                if old
                    .as_deref()
                    .is_some_and(|o| state.greeted_users.contains(o))
                {
                    state.greeted_users.insert(chosen.to_string());
                }
                out.plan_changed = true;
            }
            Err(e) => warn!("Directory: moving {}'s address: {e:#}", resident.username),
        }
    }
}

/// Step 3: the members of each followed directory group join its cleaning group.
fn joins(ctx: &BotContext, state: &mut State, snapshot: &Snapshot, rules: &Rules, out: &mut Round) {
    let people = &snapshot.people;
    let residents = rules.residents_group.as_str();
    let followed: Vec<(GroupId, String)> = state
        .cleaning_groups
        .iter()
        .filter(|g| !g.house_wide)
        .filter_map(|g| Some((g.id.clone(), g.directory_group.clone()?)))
        .filter(|(_, wanted)| snapshot.has_group(wanted))
        .collect();
    for (group_id, wanted) in followed {
        let members: Vec<&Resident> = people
            .members(&wanted)
            .filter(|r| residents.is_empty() || r.groups.contains(residents))
            .collect();
        for resident in members {
            let Some(person_id) = person_for(state, people, resident) else {
                continue;
            };
            if state.is_member(&group_id, &person_id) {
                continue;
            }
            // As an admin's `!member add`.
            let joined = crate::house::note_admin_choice(state, &group_id, &person_id, true)
                .and_then(|()| apply_group_join(ctx, state, &group_id, &person_id));
            match joined {
                Ok(_) => {
                    info!(login = %resident.username, group = %wanted, "Directory: joined");
                    out.notices.push(format!(
                        "➕ {} joined {} ({wanted} in the directory).",
                        resident_name(resident),
                        group_name_of(state, &group_id)
                    ));
                    out.plan_changed = true;
                }
                Err(e) => warn!("Directory: {} joining {wanted}: {e:#}", resident.username),
            }
        }
    }
}

/// `resident`'s person in the plan, made when there is none: their chosen account, else their
/// house account, as the address. `None` when it can't be told who they are (several people
/// in use, or no account to write to them at).
fn person_for(state: &mut State, people: &People, resident: &Resident) -> Option<PersonId> {
    let (person, linked) = match plan_person(state, people, resident) {
        Match::Anchored(p) => (Some(p.id.clone()), true),
        Match::Found(p) => (Some(p.id.clone()), false),
        Match::Split(_) => return None,
        Match::None => (None, false),
    };
    if let Some(id) = person {
        if linked {
            return Some(id);
        }
        return state
            .apply_event(DomainEvent::PersonDirectoryLinked {
                person_id: id.clone(),
                directory_id: resident.id.clone(),
            })
            .ok()
            .map(|()| id);
    }
    let account: Option<OwnedUserId> = people
        .contact(resident)
        .account()
        .map(ToOwned::to_owned)
        .or_else(|| resident.house.clone());
    let Some(account) = account else {
        warn!(login = %resident.username, "Directory: no Matrix account to add them with");
        return None;
    };
    if state.person_by_matrix_id(account.as_str()).is_some() {
        // Someone else in the plan (another resident's, or no longer active).
        warn!(login = %resident.username, %account, "Directory: their account is another person in the plan; not added");
        return None;
    }
    let person_id = uuid::Uuid::new_v4().to_string();
    let created = state
        .apply_event(DomainEvent::PersonCreated {
            person_id: person_id.clone(),
            display_name: resident.name.clone(),
            matrix_id: Some(account.to_string()),
        })
        .and_then(|()| {
            state.apply_event(DomainEvent::PersonDirectoryLinked {
                person_id: person_id.clone(),
                directory_id: resident.id.clone(),
            })
        });
    if let Err(e) = created {
        warn!("Directory: adding {}: {e:#}", resident.username);
        return None;
    }
    if !resident.name.is_empty() {
        if let Some(p) = state.persons.iter_mut().find(|p| p.id == person_id) {
            p.display_name = resident.name.clone();
        }
    }
    Some(person_id)
}

/// Carry out `departures` (person, group), house-wide groups last; one notice per person.
pub fn apply_departures(
    ctx: &BotContext,
    state: &mut State,
    departures: &BTreeSet<DirectoryDeparture>,
    why: &BTreeMap<DirectoryDeparture, Why>,
) -> Vec<String> {
    let mut by_person: BTreeMap<&PersonId, Vec<&GroupId>> = BTreeMap::new();
    for (person, group) in departures {
        by_person.entry(person).or_default().push(group);
    }
    let mut notices = Vec::new();
    for (person_id, mut groups) in by_person {
        groups.sort_by_key(|g| state.group_by_id(g).is_some_and(|g| g.house_wide));
        let mut left = Vec::new();
        let mut handed_on = 0;
        for group_id in groups {
            match depart(ctx, state, person_id, group_id) {
                Ok(Some(turns)) => {
                    left.push(group_name_of(state, group_id));
                    handed_on += turns;
                }
                Ok(None) => {}
                Err(e) => warn!("Directory: {person_id} leaving {group_id}: {e:#}"),
            }
        }
        if left.is_empty() {
            continue;
        }
        let name = state
            .person_by_id(person_id)
            .map(crate::commands::person_label)
            .unwrap_or_else(|| person_id.clone());
        let reason = departures
            .iter()
            .find(|(p, _)| p == person_id)
            .and_then(|d| why.get(d))
            .map(Why::text)
            .unwrap_or_default();
        info!(person = %person_id, groups = %left.join(", "), "Directory: left");
        notices.push(format!(
            "👋 {name} {reason}: left {}. {} handed on; this week's turn, completions and \
             statistics stay.",
            left.join(", "),
            crate::view::plural(handed_on, "upcoming turn", "upcoming turns")
        ));
    }
    notices
}

/// `person_id` leaves `group_id` as with `!member remove` (an admin's hand-added place in a
/// house-wide group goes too, so it isn't given back). `None` when they were not in it;
/// else how many upcoming turns were handed on.
fn depart(
    ctx: &BotContext,
    state: &mut State,
    person_id: &PersonId,
    group_id: &GroupId,
) -> Result<Option<usize>> {
    let Some(group) = state.group_by_id(group_id).cloned() else {
        return Ok(None);
    };
    if !group.member_ids.contains(person_id) {
        return Ok(None);
    }
    if group.house_wide && group.house_added.contains(person_id) {
        state.apply_event(DomainEvent::HouseWideSet {
            group_id: group_id.clone(),
            house_wide: true,
            excused: group.house_excused.clone(),
            added: group
                .house_added
                .iter()
                .filter(|p| *p != person_id)
                .cloned()
                .collect(),
        })?;
    }
    state.apply_event(DomainEvent::PersonLeftGroup {
        person_id: person_id.clone(),
        group_id: group_id.clone(),
    })?;
    let departure = apply_group_departure(ctx, state, person_id, group_id)?;
    Ok(Some(departure.vacated.len()))
}

fn resident_name(resident: &Resident) -> String {
    if resident.name.is_empty() || resident.name == resident.username {
        resident.username.clone()
    } else {
        format!("{} ({})", resident.name, resident.username)
    }
}

fn held_notice(
    state: &State,
    held: &BTreeSet<DirectoryDeparture>,
    why: &BTreeMap<DirectoryDeparture, Why>,
    over: &BTreeMap<GroupId, (usize, usize)>,
    first: bool,
) -> String {
    let head = if first {
        format!(
            "✋ The first departures the directory asks for since the plan follows it ({} \
             people): nobody left, an admin looks first.",
            persons_of(held).len()
        )
    } else {
        let groups: Vec<String> = over
            .iter()
            .map(|(group, (n, limit))| {
                format!("{}: {n}, more than {limit}", group_name_of(state, group))
            })
            .collect();
        format!(
            "✋ The directory says more people leave a group at once than it allows ({}): \
             nobody left that group.",
            groups.join("; ")
        )
    };
    format!(
        "{head} If that is right, !directory confirm {} lets exactly these go:\n{}",
        held_digest(held),
        describe(state, held, why)
    )
}

/// One line per departure: "• Mia (@mia:…): Kitchen 1 (is no longer in «residents»)".
fn describe(
    state: &State,
    departures: &BTreeSet<DirectoryDeparture>,
    why: &BTreeMap<DirectoryDeparture, Why>,
) -> String {
    departures
        .iter()
        .map(|d @ (person, group)| {
            let name = state
                .person_by_id(person)
                .map(crate::commands::person_label)
                .unwrap_or_else(|| person.clone());
            let reason = why.get(d).map(Why::text).unwrap_or_default();
            format!("• {name}: {} ({reason})", group_name_of(state, group))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A round on `snapshot` under the bot's operation lock, saved, then the plan and the
/// selectors refreshed and the admins told.
pub async fn run_round(
    ctx: &BotContext,
    client: &Client,
    snapshot: &Snapshot,
    cfg: &DirectorySection,
    now: SystemTime,
) {
    let rules = Rules::of(cfg);
    let outcome = {
        let _operation = ctx.operations.lock().await;
        let mut state = ctx.state.lock().await;
        // Saved when something changed; not for the time of the read alone (after a restart
        // an old time only means one more read before a departure).
        let mark = |s: &State| {
            let mut memory = s.directory.clone();
            memory.missing_at = None;
            (s.event_log.len(), s.greeted_users.len(), memory)
        };
        let before = mark(&state);
        let outcome = round(ctx, &mut state, snapshot, &rules, now);
        if mark(&state) != before || outcome.plan_changed {
            if let Err(e) = state.save(&ctx.state_path).await {
                tracing::error!("Directory: saving the plan: {e:#}");
            }
        }
        outcome
    };
    if outcome.plan_changed {
        crate::onboarding::refresh_all_selectors(ctx, client).await;
        if let Some(room) = client.get_room(&ctx.room_id) {
            let (year, week) = crate::state::current_iso_week();
            crate::scheduler::refresh_pinned_plan(ctx, &room, year, week).await;
        }
    }
    for notice in &outcome.notices {
        tell_admins(ctx, client, notice).await;
    }
}

/// `text` to the admins: in their direct chats with the bot, else in the cleaning room
/// addressed to them.
pub async fn tell_admins(ctx: &BotContext, client: &Client, text: &str) {
    let admins_now = ctx.admin_users.current();
    let dms: Vec<_> = client
        .joined_rooms()
        .into_iter()
        .filter(|room| mxbot_common::rooms::admin_dm_partner(room, &admins_now).is_some())
        .collect();
    let content = |text: &str| -> RoomMessageEventContent {
        crate::format::intentional(crate::format::mentionify(text))
    };
    if dms.is_empty() {
        let mut admins: Vec<String> = admins_now.iter().map(|u| u.to_string()).collect();
        admins.sort();
        let Some(room) = client.get_room(&ctx.room_id) else {
            warn!("Directory: no room to tell the admins: {text}");
            return;
        };
        let message = content(&format!("{text}\n{}", admins.join(" ")));
        if let Err(e) = mxbot_common::send::send_with_retry(&room, message).await {
            warn!("Directory: telling the admins: {e}");
        }
        return;
    }
    for room in dms {
        if let Err(e) = mxbot_common::send::send_with_retry(&room, content(text)).await {
            warn!("Directory: telling an admin: {e}");
        }
    }
}

// ── Admin commands ──────────────────────────────────────────────────────────

/// `!directory` (admin): what the bot follows and what is held. `!directory confirm`: let the
/// held departures go, if the last read still asks for them.
pub(crate) async fn cmd_directory(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    crate::commands::require_admin(ctx, sender)?;
    let Some(cfg) = ctx.config.directory.as_ref() else {
        return Ok(Some(
            "📇 No [directory] in the config: nothing follows it.".into(),
        ));
    };
    let snapshot = super::read(&ctx.directory);
    match args {
        [] => Ok(Some(status(ctx, cfg, snapshot.as_deref()).await)),
        [confirm, rest @ ..] if confirm.eq_ignore_ascii_case("confirm") && rest.len() <= 1 => {
            let Some(snapshot) = snapshot else {
                return Ok(Some("📇 The directory hasn't been read yet.".into()));
            };
            let digest = rest.first().copied().unwrap_or("");
            Ok(Some(
                confirm_held(ctx, &snapshot, &Rules::of(cfg), digest, SystemTime::now()).await?,
            ))
        }
        _ => Ok(Some("💡 Usage: !directory [confirm <digest>]".into())),
    }
}

async fn status(ctx: &BotContext, cfg: &DirectorySection, snapshot: Option<&Snapshot>) -> String {
    let state = ctx.state.lock().await;
    let mut lines = vec![match snapshot {
        Some(s) => format!(
            "📇 Directory read {} min ago: {} residents{}.",
            s.age(SystemTime::now()).as_secs() / 60,
            s.people.active().count(),
            if s.is_stale(&cfg.reader, SystemTime::now()) {
                " (too old: nothing follows it)"
            } else {
                ""
            }
        ),
        None => "📇 The directory hasn't been read yet.".into(),
    }];
    lines.push(if cfg.residents_group.is_empty() {
        "Residents: not followed.".into()
    } else {
        format!("Who leaves «{}» leaves every group.", cfg.residents_group)
    });
    if state.directory.reviewed.as_deref() != Some(cfg.residents_group.as_str()) {
        lines.push("The first departures wait for !directory confirm.".into());
    }
    for g in state.cleaning_groups.iter() {
        if let Some(wanted) = &g.directory_group {
            lines.push(format!("{} follows «{wanted}».", g.name));
        }
    }
    let linked = state
        .persons
        .iter()
        .filter(|p| p.active && p.directory_id.is_some())
        .count();
    lines.push(format!(
        "{linked} people in the plan are linked to the directory."
    ));
    if !state.directory.held.is_empty() {
        let why = snapshot
            .map(|s| wanted_departures(&state, s, &Rules::of(cfg)))
            .unwrap_or_default();
        lines.push(format!(
            "✋ Held (!directory confirm {}):\n{}",
            held_digest(&state.directory.held),
            describe(&state, &state.directory.held, &why)
        ));
    }
    lines.join("\n")
}

/// `!directory confirm <digest>`: the held departures, if `digest` is theirs ([`held_digest`],
/// the one the admin saw) and `snapshot` (fresh) still asks for each. Another digest, or none:
/// nothing is done, the answer shows what is held and its digest.
pub async fn confirm_held(
    ctx: &BotContext,
    snapshot: &Snapshot,
    rules: &Rules,
    digest: &str,
    now: SystemTime,
) -> Result<String> {
    let mut state = ctx.state.lock().await;
    if state.directory.held.is_empty() {
        return Ok("🤷 Nothing is held.".into());
    }
    let held_now = held_digest(&state.directory.held);
    if !held_now.eq_ignore_ascii_case(digest.trim()) {
        let why = wanted_departures(&state, snapshot, rules);
        let named = if digest.trim().is_empty() {
            "no digest was given".to_owned()
        } else {
            format!("not «{}»", digest.trim())
        };
        return Ok(format!(
            "⛔ What is held is {held_now} ({named}): nothing was done. If these are right, \
             !directory confirm {held_now}:\n{}",
            describe(&state, &state.directory.held, &why)
        ));
    }
    if snapshot.age(now) > rules.stale_after {
        return Ok(
            "⏳ The last read of the directory is too old. Try again after the next one.".into(),
        );
    }
    let wanted = wanted_departures(&state, snapshot, rules);
    let held = state.directory.held.clone();
    if !held.iter().all(|d| wanted.contains_key(d)) {
        return Ok(
            "🔄 The directory changed since: nothing was done. The next read holds what is left."
                .into(),
        );
    }
    let listed = describe(&state, &held, &wanted);
    let notices = apply_departures(ctx, &mut state, &held, &wanted);
    for d in &held {
        state.directory.missing.remove(d);
    }
    state.directory.held.clear();
    // An admin looked at the first departures: from now on the limits alone hold them.
    state.directory.reviewed = Some(rules.residents_group.clone());
    state.save(&ctx.state_path).await?;
    Ok(if notices.is_empty() {
        format!("✅ Confirmed {held_now}; they had left already:\n{listed}")
    } else {
        format!("✅ Confirmed {held_now}:\n{listed}\n{}", notices.join("\n"))
    })
}

/// `!groups directory <group> <directory group | off>` (admin): the group follows a group
/// of the directory (a kitchen), or its members choose themselves again.
pub(crate) async fn cmd_groups_directory(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    crate::commands::require_admin(ctx, sender)?;
    const USAGE: &str = "💡 Usage: !groups directory <group> <directory group | off> · \
                         e.g. kitchen-floor-1: its members are in the group";
    let (group_name, wanted) = match args {
        [group, wanted] => (*group, *wanted),
        _ => return Ok(Some(USAGE.into())),
    };
    let wanted = (!wanted.eq_ignore_ascii_case("off")).then(|| wanted.trim().to_owned());
    if wanted
        .as_deref()
        .is_some_and(|w| w.is_empty() || w.chars().any(char::is_whitespace))
    {
        return Ok(Some(USAGE.into()));
    }
    let mut state = ctx.state.lock().await;
    let Some(group) = state.group_by_name(group_name).cloned() else {
        return Ok(Some(crate::commands::group_not_found(group_name)));
    };
    if group.house_wide && wanted.is_some() {
        return Ok(Some(format!(
            "🏠 «{}» is for the whole house; it can't follow a group of the directory.",
            group.name
        )));
    }
    if group.directory_group == wanted {
        return Ok(Some(format!(
            "«{}» is so already. No changes made.",
            group.name
        )));
    }
    state.apply_event(DomainEvent::GroupDirectorySet {
        group_id: group.id.clone(),
        directory_group: wanted.clone(),
    })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(match wanted {
        Some(w) => {
            let mut reply = format!(
                "📇 «{}» follows «{w}» from the next read of the directory (within {} min): \
                 its members join; who leaves «{w}» leaves the group (after two reads), \
                 turns handed on as with !member remove. !join and !leave don't change it.",
                group.name,
                ctx.config
                    .directory
                    .as_ref()
                    .map_or(15, |d| d.reader.refresh_minutes)
            );
            if ctx.config.directory.is_none() {
                reply.push_str("\n⚠️ There is no [directory] in the config: nothing follows it.");
            }
            reply
        }
        None => format!(
            "«{}» no longer follows the directory: its members choose themselves.",
            group.name
        ),
    }))
}
