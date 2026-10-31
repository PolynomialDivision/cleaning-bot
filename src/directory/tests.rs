//! The plan following the house's directory (`directory`, `reconcile`): one person per
//! resident, their chosen account as the address, kitchens, moving out, rights.

use std::{
    collections::{BTreeSet, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};

use chrono::Utc;
use mxbot_common::{
    directory::{Choice, Link, People, Person as Resident, Snapshot},
    matrix_sdk::ruma::{OwnedRoomId, OwnedUserId, UserId},
};
use tokio::sync::Mutex;

use super::{
    logins, person_account, recognised,
    reconcile::{confirm_held, held_digest, mass_limit, round, Round, Rules},
    report_failed_read,
};
use crate::{
    analytics::DomainEvent,
    config::Config,
    domain::{CleaningGroup, GroupId, Person, PersonId},
    state::{current_iso_week, State},
    BotContext,
};

const MIN: Duration = Duration::from_secs(60);

fn id(s: &str) -> OwnedUserId {
    UserId::parse(s).unwrap()
}

/// A resident as the directory reads them: house account `@<username>:house.org`, in
/// `residents`, nothing chosen.
struct R(Resident);

impl R {
    fn new(uuid: &str, username: &str) -> R {
        let house = id(&format!("@{username}:house.org"));
        R(Resident {
            id: uuid.to_owned(),
            username: username.to_owned(),
            name: String::new(),
            active: true,
            accounts: vec![house.clone()],
            groups: BTreeSet::from(["residents".to_owned()]),
            house: Some(house),
            links: Vec::new(),
            choice: Choice::None,
            stay_until: None,
        })
    }

    /// A link (`matrix_ids`: an admin entered it or the person proved it, the same).
    fn link(mut self, account: &str) -> R {
        self.0.accounts.push(id(account));
        self.0.links.push(Link { id: id(account) });
        self
    }

    fn chose(mut self, account: &str) -> R {
        self.0.choice = Choice::Account(id(account));
        self
    }

    fn groups(mut self, groups: &[&str]) -> R {
        self.0.groups = groups.iter().map(|g| g.to_string()).collect();
        self
    }

    fn named(mut self, name: &str) -> R {
        self.0.name = name.to_owned();
        self
    }

    fn deactivated(mut self) -> R {
        self.0.active = false;
        self.0.groups.clear();
        self
    }
}

fn people(residents: Vec<R>) -> People {
    People::new(residents.into_iter().map(|r| r.0).collect())
}

/// A read at `at` of `residents`; the groups that exist: residents, subtenants,
/// former-residents and the kitchens 1 and 3.
fn snapshot(residents: Vec<R>, at: SystemTime) -> Snapshot {
    Snapshot {
        people: people(residents),
        groups: [
            "residents",
            "subtenants",
            "former-residents",
            "kitchen-floor-1",
            "kitchen-floor-3",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        read_at: at,
    }
}

fn rules() -> Rules {
    rules_of(&["residents"])
}

/// Who lives here: in any of `groups`.
fn rules_of(groups: &[&str]) -> Rules {
    Rules {
        residents_groups: groups.iter().map(|g| (*g).to_owned()).collect(),
        stale_after: 60 * MIN,
    }
}

/// The house's model: full members and subtenants live here.
fn with_subtenants() -> Rules {
    rules_of(&["residents", "subtenants"])
}

fn context(state: State) -> (BotContext, PathBuf) {
    let config: Config = toml::from_str(
        r#"
        [matrix]
        homeserver = "https://matrix.house.org"
        user_id = "@cleaningbot:house.org"
        access_token = "test"
        device_id = "TEST"

        [security]
        admin_users = ["@admin:house.org"]

        [schedule]
        room_id = "!room:house.org"
        interval_weeks = 1
        materialize_weeks = 4

        [directory]
        provider = "authentik"
        api_url = "https://login.house.org/api/v3"
    "#,
    )
    .unwrap();
    let path = std::env::temp_dir().join(format!(
        "cleaning-bot-directory-test-{}.json",
        uuid::Uuid::new_v4()
    ));
    let ctx = BotContext {
        operations: Arc::new(Mutex::new(())),
        state: Arc::new(Mutex::new(state)),
        state_path: path.clone(),
        config: Arc::new(config),
        admin_users: HashSet::from([id("@admin:house.org")]).into(),
        room_id: OwnedRoomId::try_from("!room:house.org").unwrap(),
        directory: Default::default(),
        logins: Default::default(),
        acting: None,
    };
    (ctx, path)
}

/// Add a person with `mxid` (linked to `uuid`, if given) to `groups`.
fn add(state: &mut State, mxid: &str, uuid: Option<&str>, groups: &[&GroupId]) -> PersonId {
    let mut p = Person::new_matrix(mxid);
    p.directory_id = uuid.map(str::to_owned);
    let pid = p.id.clone();
    state.persons.push(p);
    for g in groups {
        state
            .cleaning_groups
            .iter_mut()
            .find(|c| &c.id == *g)
            .unwrap()
            .member_ids
            .push(pid.clone());
    }
    pid
}

fn group(state: &mut State, name: &str, directory_group: Option<&str>) -> GroupId {
    let mut g = CleaningGroup::new(name);
    g.directory_group = directory_group.map(str::to_owned);
    let gid = g.id.clone();
    state.cleaning_groups.push(g);
    gid
}

/// The departures were reviewed under `rules()` before (a bot that has followed the directory
/// for a while): the limits alone hold them.
fn reviewed(state: &mut State) {
    state.directory.reviewed = Some(rules().key());
}

fn plan(state: &mut State) {
    for ev in crate::resolver::materialize(state, 4) {
        state.apply_event(ev).unwrap();
    }
}

fn person<'a>(state: &'a State, pid: &PersonId) -> &'a Person {
    state.person_by_id(pid).unwrap()
}

fn future_turns(state: &State, pid: &PersonId, gid: &GroupId) -> usize {
    let now = current_iso_week();
    state
        .slot_assignments
        .iter()
        .filter(|a| &a.group_id == gid && a.person_id.as_ref() == Some(pid))
        .filter(|a| (a.iso_year, a.iso_week) > now)
        .count()
}

fn current_turns(state: &State, gid: &GroupId) -> Vec<Option<PersonId>> {
    let now = current_iso_week();
    state
        .slot_assignments
        .iter()
        .filter(|a| &a.group_id == gid && (a.iso_year, a.iso_week) == now)
        .map(|a| a.person_id.clone())
        .collect()
}

/// A completion of `pid` in `gid`, last week: history that must stay.
fn completed_last_week(state: &mut State, pid: &PersonId, gid: &GroupId) {
    let (y, w) = current_iso_week();
    let (y, w) = crate::state::add_weeks(y, w, -1);
    state
        .apply_event(DomainEvent::CleaningCompleted {
            completed_on: None,
            group_id: gid.clone(),
            slot_id: None,
            person_id: pid.clone(),
            responsible_person_ids: vec![pid.clone()],
            iso_year: y,
            iso_week: w,
            shift: 0,
        })
        .unwrap();
}

fn logged(state: &State, pred: impl Fn(&DomainEvent) -> bool) -> usize {
    state.event_log.iter().filter(|e| pred(&e.event)).count()
}

/// The log of `before` is the start of `state`'s: nothing lost or rewritten.
fn log_kept(before: &State, state: &State) {
    assert!(state.event_log.len() >= before.event_log.len());
    assert!(state
        .event_log
        .iter()
        .zip(&before.event_log)
        .all(|(a, b)| a.id == b.id && a.timestamp == b.timestamp));
}

// ── A, B, C: one person per resident, the chosen account is the address ─────

#[test]
fn a_resident_with_a_house_and_an_external_account_is_one_person() {
    // A: the plan met Anna on matrix.org; her house account is hers too (admin link).
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let kitchen = group(&mut state, "Kitchen", None);
    let floor = group(&mut state, "Floor", None);
    let anna = add(&mut state, "@anna:matrix.org", None, &[&kitchen]);
    plan(&mut state);
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let dir = || vec![R::new("u-anna", "anna").link("@anna:matrix.org")];
    let read = snapshot(dir(), t0);

    // Before the link: the house account acts as the matrix.org one.
    assert_eq!(
        person_account(Some(&read.people), &state, &id("@anna:house.org")),
        id("@anna:matrix.org")
    );
    let before = state.persons.len();
    let out = round(&ctx, &mut state, &read, &rules(), t0);
    assert_eq!(
        person(&state, &anna).directory_id.as_deref(),
        Some("u-anna")
    );
    assert_eq!(
        logged(&state, |e| matches!(
            e,
            DomainEvent::PersonDirectoryLinked { .. }
        )),
        1
    );
    assert!(out.notices.is_empty(), "{out:?}");
    // C: nothing chosen, the address stays.
    assert_eq!(
        person(&state, &anna).matrix_id.as_deref(),
        Some("@anna:matrix.org")
    );
    // Joining from the house account: the same person, never a second.
    let account = person_account(Some(&read.people), &state, &id("@anna:house.org"));
    assert_eq!(account, id("@anna:matrix.org"));
    crate::commands::join_group(&ctx, &mut state, account.as_str(), &floor)
        .unwrap()
        .unwrap();
    assert_eq!(state.persons.len(), before);
    assert!(state.is_member(&floor, &anna));
    // A second round: nothing new.
    let log = state.event_log.len();
    let t1 = t0 + 15 * MIN;
    let out = round(&ctx, &mut state, &snapshot(dir(), t1), &rules(), t1);
    assert_eq!(out, Round::default());
    assert_eq!(state.event_log.len(), log);
    // Once linked, found by the id even when the account is no longer listed.
    let read = snapshot(vec![R::new("u-anna", "anna")], t0 + 30 * MIN);
    assert_eq!(
        person_account(Some(&read.people), &state, &id("@anna:house.org")),
        id("@anna:matrix.org")
    );
}

#[test]
fn the_chosen_account_becomes_the_address_and_unchosen_or_invalid_stays() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let kitchen = group(&mut state, "Kitchen", None);
    let anna = add(&mut state, "@anna:matrix.org", None, &[&kitchen]);
    let ben = add(&mut state, "@ben:house.org", None, &[&kitchen]);
    let cleo = add(&mut state, "@cleo:house.org", None, &[&kitchen]);
    state.greeted_users.insert("@anna:matrix.org".into());
    plan(&mut state);
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let read = snapshot(
        vec![
            // A: chose her house account.
            R::new("u-anna", "anna")
                .link("@anna:matrix.org")
                .chose("@anna:house.org"),
            // B: chose one of his links.
            R::new("u-ben", "ben")
                .link("@ben:matrix.org")
                .chose("@ben:matrix.org"),
            // Invalid: an account not linked to her is no address (a link would be).
            R::new("u-cleo", "cleo")
                .link("@cleo:matrix.org")
                .chose("@cleo:elsewhere.org"),
        ],
        t0,
    );
    let out = round(&ctx, &mut state, &read, &rules(), t0);
    assert!(out.plan_changed);
    assert_eq!(
        person(&state, &anna).matrix_id.as_deref(),
        Some("@anna:house.org")
    );
    assert_eq!(
        person(&state, &ben).matrix_id.as_deref(),
        Some("@ben:matrix.org")
    );
    assert_eq!(
        person(&state, &cleo).matrix_id.as_deref(),
        Some("@cleo:house.org")
    );
    assert_eq!(
        logged(&state, |e| matches!(
            e,
            DomainEvent::PersonMatrixLinked { .. }
        )),
        2
    );
    // Her welcome was had: not again at the new address.
    assert!(state.greeted_users.contains("@anna:house.org"));
    // Either account is Anna in the plan.
    for account in ["@anna:matrix.org", "@anna:house.org"] {
        assert_eq!(
            person_account(Some(&read.people), &state, &id(account)),
            id("@anna:house.org")
        );
    }
    assert_eq!(state.persons.len(), 3);
}

#[test]
fn an_address_held_by_another_person_does_not_move_and_the_admins_are_told_once() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let kitchen = group(&mut state, "Kitchen", None);
    let anna = add(&mut state, "@anna:matrix.org", Some("u-anna"), &[&kitchen]);
    // Someone else in the plan has the account Anna chose (an old record).
    add(&mut state, "@anna:house.org", Some("u-other"), &[]);
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let dir = || vec![R::new("u-anna", "anna").chose("@anna:house.org")];
    let out = round(&ctx, &mut state, &snapshot(dir(), t0), &rules(), t0);
    assert_eq!(out.notices.len(), 1, "{out:?}");
    assert!(out.notices[0].contains("another person in the plan"));
    assert_eq!(
        person(&state, &anna).matrix_id.as_deref(),
        Some("@anna:matrix.org")
    );
    let t1 = t0 + 15 * MIN;
    let out = round(&ctx, &mut state, &snapshot(dir(), t1), &rules(), t1);
    assert!(out.notices.is_empty());
}

// ── H: the preferred account changes ────────────────────────────────────────

#[test]
fn a_changed_choice_moves_the_address_and_old_chats_stay_theirs() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let kitchen = group(&mut state, "Kitchen", None);
    let mia = add(&mut state, "@mia:house.org", Some("u-mia"), &[&kitchen]);
    // The private chat the bot opened for her old address.
    state.private_chats.insert(
        "@mia:house.org".into(),
        crate::state::PrivateChat {
            room_id: "!chat:house.org".into(),
            greeted: true,
        },
    );
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let read = snapshot(
        vec![R::new("u-mia", "mia")
            .link("@mia:matrix.org")
            .chose("@mia:matrix.org")],
        t0,
    );
    round(&ctx, &mut state, &read, &rules(), t0);
    assert_eq!(
        person(&state, &mia).matrix_id.as_deref(),
        Some("@mia:matrix.org")
    );
    // Commands from the old account are hers.
    assert_eq!(
        person_account(Some(&read.people), &state, &id("@mia:house.org")),
        id("@mia:matrix.org")
    );
    // The old chat is found under one of her recognised accounts (`help_board::private_chat`
    // looks there; `private::authorized` accepts any of them in the chat).
    let accounts = recognised(Some(&read.people), &id("@mia:matrix.org"));
    assert!(accounts.contains(&id("@mia:house.org")));
    assert!(accounts
        .iter()
        .any(|a| state.private_chats.contains_key(a.as_str())));
    assert!(state.private_chats.contains_key("@mia:house.org"));
}

// ── D: a kitchen move ───────────────────────────────────────────────────────

#[test]
fn a_kitchen_move_leaves_the_old_kitchen_after_two_reads_and_joins_the_new_one() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let k1 = group(&mut state, "Kitchen 1", Some("kitchen-floor-1"));
    let k3 = group(&mut state, "Kitchen 3", Some("kitchen-floor-3"));
    let ids: Vec<PersonId> = ["ana", "bo", "cy", "di"]
        .iter()
        .map(|n| {
            add(
                &mut state,
                &format!("@{n}:house.org"),
                Some(&format!("u-{n}")),
                &[&k1],
            )
        })
        .collect();
    let eve = add(&mut state, "@eve:house.org", Some("u-eve"), &[&k3]);
    plan(&mut state);
    reviewed(&mut state);
    let bo = ids[1].clone();
    completed_last_week(&mut state, &bo, &k1);
    assert!(future_turns(&state, &bo, &k1) > 0);
    let this_week = current_turns(&state, &k1);
    let before = state.clone();
    let (ctx, _) = context(State::default());

    let dir = || {
        vec![
            R::new("u-ana", "ana").groups(&["residents", "kitchen-floor-1"]),
            R::new("u-bo", "bo").groups(&["residents", "kitchen-floor-3"]),
            R::new("u-cy", "cy").groups(&["residents", "kitchen-floor-1"]),
            R::new("u-di", "di").groups(&["residents", "kitchen-floor-1"]),
            R::new("u-eve", "eve").groups(&["residents", "kitchen-floor-3"]),
        ]
    };
    let t0 = SystemTime::now();
    let out = round(&ctx, &mut state, &snapshot(dir(), t0), &rules(), t0);
    // Joins right away, through `!member add`'s path; still in Kitchen 1 after one read.
    assert!(state.is_member(&k3, &bo) && state.is_member(&k3, &eve));
    assert!(state.is_member(&k1, &bo));
    assert_eq!(out.notices.len(), 1, "{out:?}");
    assert!(out.notices[0].contains("joined Kitchen 3"));

    let t1 = t0 + 15 * MIN;
    let out = round(&ctx, &mut state, &snapshot(dir(), t1), &rules(), t1);
    assert!(!state.is_member(&k1, &bo));
    assert!(state.is_member(&k3, &bo));
    assert_eq!(out.notices.len(), 1, "{out:?}");
    assert!(
        out.notices[0].contains("left Kitchen 1"),
        "{}",
        out.notices[0]
    );
    // Upcoming turns handed on, this week's as it was, history kept.
    assert_eq!(future_turns(&state, &bo, &k1), 0);
    assert_eq!(current_turns(&state, &k1), this_week);
    assert_eq!(state.completions.len(), before.completions.len());
    assert!(state.completions.iter().any(|c| c.completed_by_id == bo));
    log_kept(&before, &state);
    // A third read changes nothing.
    let n = state.event_log.len();
    let t2 = t1 + 15 * MIN;
    assert_eq!(
        round(&ctx, &mut state, &snapshot(dir(), t2), &rules(), t2),
        Round::default()
    );
    assert_eq!(state.event_log.len(), n);
}

#[test]
fn a_kitchen_newcomer_is_added_by_directory_id_with_the_right_address() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let k1 = group(&mut state, "Kitchen 1", Some("kitchen-floor-1"));
    let free = group(&mut state, "Garden", None);
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let dir = || {
        vec![
            // Not chosen: the house account is the address.
            R::new("u-nia", "nia")
                .named("Nia N.")
                .groups(&["residents", "kitchen-floor-1"]),
            // Chosen: that account.
            R::new("u-ole", "ole")
                .link("@ole:matrix.org")
                .chose("@ole:matrix.org")
                .groups(&["residents", "kitchen-floor-1"]),
            // In the kitchen but not a resident (a guest): not added.
            R::new("u-gus", "gus").groups(&["kitchen-floor-1"]),
        ]
    };
    round(&ctx, &mut state, &snapshot(dir(), t0), &rules(), t0);
    let nia = state.person_by_matrix_id("@nia:house.org").unwrap();
    assert_eq!(nia.directory_id.as_deref(), Some("u-nia"));
    assert_eq!(nia.display_name, "Nia N.");
    let ole = state.person_by_matrix_id("@ole:matrix.org").unwrap();
    assert_eq!(ole.directory_id.as_deref(), Some("u-ole"));
    assert!(state.person_by_matrix_id("@gus:house.org").is_none());
    assert_eq!(state.group_by_id(&k1).unwrap().member_ids.len(), 2);
    assert!(state.group_by_id(&free).unwrap().member_ids.is_empty());
    // Again: nobody twice.
    let t1 = t0 + 15 * MIN;
    round(&ctx, &mut state, &snapshot(dir(), t1), &rules(), t1);
    assert_eq!(state.persons.len(), 2);
    assert_eq!(state.group_by_id(&k1).unwrap().member_ids.len(), 2);
    // Self-service doesn't change a followed group.
    let refused = crate::commands::leave_group(&ctx, &mut state, "@nia:house.org", &k1).unwrap();
    assert!(refused.unwrap_err().contains("kitchen-floor-1"));
    let refused = crate::commands::join_group(&ctx, &mut state, "@gus:house.org", &k1).unwrap();
    assert!(refused.unwrap_err().contains("kitchen-floor-1"));
}

// ── E, F: moving out, deactivated, gone ─────────────────────────────────────

/// Mia in the kitchen and on the staircase (house-wide), with history; three neighbours.
fn mia_and_neighbours() -> (State, PersonId, GroupId, GroupId) {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let kitchen = group(&mut state, "Kitchen", None);
    let stairs = group(&mut state, "Staircase", None);
    let mia = add(&mut state, "@mia:house.org", None, &[&kitchen]);
    for n in ["ana", "bo", "cy"] {
        add(&mut state, &format!("@{n}:house.org"), None, &[&kitchen]);
    }
    let (ctx, _) = context(State::default());
    state
        .apply_event(DomainEvent::HouseWideSet {
            group_id: stairs.clone(),
            house_wide: true,
            excused: Vec::new(),
            added: Vec::new(),
        })
        .unwrap();
    crate::house::sync(&ctx, &mut state, crate::house::Leavers::Go).unwrap();
    plan(&mut state);
    reviewed(&mut state);
    completed_last_week(&mut state, &mia, &kitchen);
    (state, mia, kitchen, stairs)
}

fn neighbours() -> Vec<R> {
    ["ana", "bo", "cy"]
        .iter()
        .map(|n| R::new(&format!("u-{n}"), n))
        .collect()
}

/// Mia as `mia_now` says (`None`: gone from the directory) leaves after two reads.
fn leaves_after_two_reads(mia_now: Option<fn() -> R>, reason: &str) {
    let (mut state, mia, kitchen, stairs) = mia_and_neighbours();
    assert!(state.is_member(&stairs, &mia));
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    // The first read links everyone.
    let mut dir = neighbours();
    dir.push(R::new("u-mia", "mia"));
    round(&ctx, &mut state, &snapshot(dir, t0), &rules(), t0);
    assert_eq!(person(&state, &mia).directory_id.as_deref(), Some("u-mia"));
    let before = state.clone();

    let read = |at| {
        let mut dir = neighbours();
        dir.extend(mia_now.map(|mia| mia()));
        snapshot(dir, at)
    };
    let t1 = t0 + 15 * MIN;
    let out = round(&ctx, &mut state, &read(t1), &rules(), t1);
    assert!(out.notices.is_empty() && !out.plan_changed, "{out:?}");
    assert!(state.is_member(&kitchen, &mia) && state.is_member(&stairs, &mia));
    let t2 = t1 + 15 * MIN;
    let out = round(&ctx, &mut state, &read(t2), &rules(), t2);
    assert!(!state.is_member(&kitchen, &mia) && !state.is_member(&stairs, &mia));
    assert_eq!(out.notices.len(), 1, "{out:?}");
    assert!(out.notices[0].contains(reason), "{}", out.notices[0]);
    assert!(out.notices[0].contains("Kitchen") && out.notices[0].contains("Staircase"));
    assert_eq!(future_turns(&state, &mia, &kitchen), 0);
    assert_eq!(future_turns(&state, &mia, &stairs), 0);
    // History kept: the person, their completions, the whole log.
    assert!(state.person_by_id(&mia).is_some());
    assert_eq!(state.completions.len(), before.completions.len());
    log_kept(&before, &state);
    // The staircase doesn't take them back.
    crate::house::sync(&ctx, &mut state, crate::house::Leavers::Go).unwrap();
    assert!(!state.is_member(&stairs, &mia));
}

#[test]
fn moving_out_leaves_every_group_after_two_reads_with_history_kept() {
    leaves_after_two_reads(
        Some(|| R::new("u-mia", "mia").groups(&["former-residents"])),
        "moved out",
    );
}

#[test]
fn a_deactivated_resident_leaves_the_same_way() {
    leaves_after_two_reads(Some(|| R::new("u-mia", "mia").deactivated()), "deactivated");
}

#[test]
fn a_resident_gone_from_the_directory_leaves_the_same_way() {
    leaves_after_two_reads(None, "no longer in the directory");
}

#[test]
fn people_without_a_directory_id_are_never_touched() {
    let (mut state, mia, kitchen, _) = mia_and_neighbours();
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    // Mia's account is unknown to the directory: never linked, never leaves.
    for i in 0..3 {
        let at = t0 + i * 15 * MIN;
        round(&ctx, &mut state, &snapshot(neighbours(), at), &rules(), at);
    }
    assert!(person(&state, &mia).directory_id.is_none());
    assert!(state.is_member(&kitchen, &mia));
}

// ── Subtenants: living here for a while, a kitchen of their own ─────────────

/// Kitchen 3 follows kitchen-floor-3 with eve (a resident) in it; the plan doesn't know sub
/// yet. Reviewed under `rules`.
fn kitchen_with_eve(rules: &Rules) -> (State, GroupId, PersonId) {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let k3 = group(&mut state, "Kitchen 3", Some("kitchen-floor-3"));
    let eve = add(&mut state, "@eve:house.org", Some("u-eve"), &[&k3]);
    plan(&mut state);
    state.directory.reviewed = Some(rules.key());
    (state, k3, eve)
}

fn eve() -> R {
    R::new("u-eve", "eve").groups(&["residents", "kitchen-floor-3"])
}

fn sub() -> R {
    R::new("u-sub", "sub")
        .named("Sam Sub")
        .groups(&["subtenants", "kitchen-floor-3"])
}

#[test]
fn a_subtenant_joins_their_kitchen_and_never_counts_as_moved_out() {
    let rules = with_subtenants();
    let (mut state, k3, eve_id) = kitchen_with_eve(&rules);
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let out = round(
        &ctx,
        &mut state,
        &snapshot(vec![eve(), sub()], t0),
        &rules,
        t0,
    );
    let sub_id = state
        .persons
        .iter()
        .find(|p| p.directory_id.as_deref() == Some("u-sub"))
        .map(|p| p.id.clone())
        .expect("added by directory id");
    assert_eq!(
        person(&state, &sub_id).matrix_id.as_deref(),
        Some("@sub:house.org")
    );
    assert!(state.is_member(&k3, &sub_id) && state.is_member(&k3, &eve_id));
    assert_eq!(out.notices.len(), 1, "{out:?}");
    assert!(
        out.notices[0].contains("Sam Sub (sub) joined Kitchen 3"),
        "{}",
        out.notices[0]
    );
    // Many reads later: nobody leaves, nothing is held.
    for i in 1..4 {
        let at = t0 + i * 15 * MIN;
        let out = round(
            &ctx,
            &mut state,
            &snapshot(vec![eve(), sub()], at),
            &rules,
            at,
        );
        assert_eq!(out, Round::default());
    }
    assert!(state.is_member(&k3, &sub_id) && state.is_member(&k3, &eve_id));
    assert!(state.directory.held.is_empty() && state.directory.missing.is_empty());

    // The residents alone (a config before subtenants): the kitchen doesn't take them in, and
    // one who is in it already counts as moved out.
    let residents_only = rules_of(&["residents"]);
    let (mut fresh, k3, _) = kitchen_with_eve(&residents_only);
    round(
        &ctx,
        &mut fresh,
        &snapshot(vec![eve(), sub()], t0),
        &residents_only,
        t0,
    );
    assert_eq!(fresh.persons.len(), 1, "sub is not added");
    assert_eq!(
        fresh
            .cleaning_groups
            .iter()
            .find(|g| g.id == k3)
            .unwrap()
            .member_ids
            .len(),
        1
    );
}

#[test]
fn a_subtenant_moving_out_leaves_their_kitchen_after_two_reads() {
    let rules = with_subtenants();
    let (mut state, k3, eve_id) = kitchen_with_eve(&rules);
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    round(
        &ctx,
        &mut state,
        &snapshot(vec![eve(), sub()], t0),
        &rules,
        t0,
    );
    let sub_id = state
        .persons
        .iter()
        .find(|p| p.directory_id.as_deref() == Some("u-sub"))
        .map(|p| p.id.clone())
        .unwrap();
    // subtenants → former-residents, still in the kitchen's group in the directory
    let gone = || {
        vec![
            eve(),
            R::new("u-sub", "sub").groups(&["former-residents", "kitchen-floor-3"]),
        ]
    };
    let t1 = t0 + 15 * MIN;
    let out = round(&ctx, &mut state, &snapshot(gone(), t1), &rules, t1);
    assert!(out.notices.is_empty(), "{out:?}");
    assert!(state.is_member(&k3, &sub_id), "one read is not enough");
    let t2 = t1 + 15 * MIN;
    let out = round(&ctx, &mut state, &snapshot(gone(), t2), &rules, t2);
    assert!(!state.is_member(&k3, &sub_id));
    assert!(state.is_member(&k3, &eve_id), "the resident stays");
    assert_eq!(out.notices.len(), 1, "{out:?}");
    assert!(
        out.notices[0].contains("is in none of «residents», «subtenants» any more (moved out)"),
        "{}",
        out.notices[0]
    );
    assert!(out.notices[0].contains("left Kitchen 3"));
    // and isn't taken in again by the kitchen's group
    let t3 = t2 + 15 * MIN;
    round(&ctx, &mut state, &snapshot(gone(), t3), &rules, t3);
    assert!(!state.is_member(&k3, &sub_id));
}

#[test]
fn residents_moving_out_leave_the_same_way_with_subtenants_followed() {
    // Mia (residents → former-residents) under the list: as with "residents" alone, and the
    // subtenants group existing with nobody in it is no reason to stop.
    let (mut state, mia, kitchen, stairs) = mia_and_neighbours();
    state.directory.reviewed = Some(with_subtenants().key());
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let mut dir = neighbours();
    dir.push(R::new("u-mia", "mia"));
    round(&ctx, &mut state, &snapshot(dir, t0), &with_subtenants(), t0);
    let read = |at| {
        let mut dir = neighbours();
        dir.push(R::new("u-mia", "mia").groups(&["former-residents"]));
        snapshot(dir, at)
    };
    for i in 1..3 {
        let at = t0 + i * 15 * MIN;
        round(&ctx, &mut state, &read(at), &with_subtenants(), at);
    }
    assert!(!state.is_member(&kitchen, &mia) && !state.is_member(&stairs, &mia));
    assert_eq!(
        state
            .cleaning_groups
            .iter()
            .find(|g| g.id == kitchen)
            .unwrap()
            .member_ids
            .len(),
        3
    );
}

#[test]
fn a_missing_residents_group_or_nobody_living_here_removes_nobody() {
    let rules = with_subtenants();
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    // eve moved out, ana still lives here: eve would leave…
    let moved = || {
        vec![
            R::new("u-eve", "eve").groups(&["former-residents", "kitchen-floor-3"]),
            R::new("u-ana", "ana"),
        ]
    };
    // …but the subtenants group is missing from the directory (not made yet, a typo)
    let (mut state, k3, eve_id) = kitchen_with_eve(&rules);
    for i in 0..3 {
        let at = t0 + i * 15 * MIN;
        let mut read = snapshot(moved(), at);
        read.groups.remove("subtenants");
        let out = round(&ctx, &mut state, &read, &rules, at);
        assert!(out.notices.is_empty(), "{out:?}");
    }
    assert!(state.is_member(&k3, &eve_id));
    assert!(state.directory.missing.is_empty() && state.directory.held.is_empty());
    // with it, she goes
    for i in 3..5 {
        let at = t0 + i * 15 * MIN;
        round(&ctx, &mut state, &snapshot(moved(), at), &rules, at);
    }
    assert!(!state.is_member(&k3, &eve_id));

    // nobody in either group (a broken directory): nobody moves out
    let (mut state, k3, eve_id) = kitchen_with_eve(&rules);
    for i in 0..3 {
        let at = t0 + i * 15 * MIN;
        let read = snapshot(
            vec![R::new("u-eve", "eve").groups(&["kitchen-floor-3"])],
            at,
        );
        round(&ctx, &mut state, &read, &rules, at);
    }
    assert!(state.is_member(&k3, &eve_id));
    assert!(state.directory.missing.is_empty());
}

/// Residents (with subtenants) switched on while the subtenants group is not made yet: the quiet
/// reads then follow no residents rule (nobody could leave for it), so they are no review; once
/// the group exists, the first departures still wait for an admin.
#[test]
fn switched_on_while_a_residents_group_is_missing_the_first_departures_are_still_held() {
    let (mut state, mia, kitchen, stairs) = mia_and_neighbours();
    // followed before with residents off
    state.directory.reviewed = Some(String::new());
    let rules = with_subtenants();
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let read = |at, mia_groups: &[&str], with_subtenants: bool| {
        let mut dir = neighbours();
        dir.push(R::new("u-mia", "mia").groups(mia_groups));
        let mut read = snapshot(dir, at);
        if !with_subtenants {
            read.groups.remove("subtenants");
        }
        read
    };
    // Mia lives here, then moves out; no subtenants group yet: quiet reads, not a review
    round(
        &ctx,
        &mut state,
        &read(t0, &["residents"], false),
        &rules,
        t0,
    );
    assert_eq!(person(&state, &mia).directory_id.as_deref(), Some("u-mia"));
    for i in 1..4 {
        let at = t0 + i * 15 * MIN;
        let out = round(
            &ctx,
            &mut state,
            &read(at, &["former-residents"], false),
            &rules,
            at,
        );
        assert!(out.notices.is_empty(), "{out:?}");
    }
    assert_eq!(state.directory.reviewed.as_deref(), Some(""));
    assert!(state.directory.missing.is_empty());
    // The group made: her departures are the first ones under these rules, held.
    let mut notices = Vec::new();
    for i in 4..6 {
        let at = t0 + i * 15 * MIN;
        let out = round(
            &ctx,
            &mut state,
            &read(at, &["former-residents"], true),
            &rules,
            at,
        );
        notices.extend(out.notices);
    }
    assert!(state.is_member(&kitchen, &mia) && state.is_member(&stairs, &mia));
    assert_eq!(state.directory.held.len(), 2);
    assert!(
        notices.iter().any(|n| n.contains("first departures")),
        "{notices:?}"
    );
    assert_eq!(state.directory.reviewed.as_deref(), Some(""));
}

#[test]
fn adding_a_residents_group_needs_no_new_review_dropping_one_does() {
    let residents = rules_of(&["residents"]);
    let both = with_subtenants();
    assert!(both.reviewed_by(Some("residents")));
    assert!(both.reviewed_by(Some("residents,subtenants")));
    assert!(!residents.reviewed_by(Some("residents,subtenants")));
    assert!(!residents.reviewed_by(Some("")), "switched on");
    assert!(!residents.reviewed_by(None));
    assert!(!rules_of(&["subtenants"]).reviewed_by(Some("residents")));
    assert!(rules_of(&[]).reviewed_by(Some("")));
    assert_eq!(both.key(), "residents,subtenants");
    assert_eq!(residents.key(), "residents");

    // Reviewed under "residents" (a bot before the list), now with subtenants: a resident's
    // departures are not held, and the new key is remembered.
    let (mut state, mia, kitchen, _) = mia_and_neighbours();
    assert_eq!(state.directory.reviewed.as_deref(), Some("residents"));
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let read = |at, mia_groups: &[&str]| {
        let mut dir = neighbours();
        dir.push(R::new("u-mia", "mia").groups(mia_groups));
        snapshot(dir, at)
    };
    round(&ctx, &mut state, &read(t0, &["residents"]), &both, t0);
    assert_eq!(
        state.directory.reviewed.as_deref(),
        Some("residents,subtenants")
    );
    for i in 1..3 {
        let at = t0 + i * 15 * MIN;
        round(
            &ctx,
            &mut state,
            &read(at, &["former-residents"]),
            &both,
            at,
        );
    }
    assert!(!state.is_member(&kitchen, &mia));
    assert!(state.directory.held.is_empty());

    // Subtenants dropped again: who would leave for it is held for an admin first.
    let (mut state, _, kitchen, _) = mia_and_neighbours();
    state.directory.reviewed = Some(both.key());
    let sub_in_plan = add(&mut state, "@sub:house.org", Some("u-sub"), &[&kitchen]);
    let dir = |at| {
        let mut dir = neighbours();
        dir.push(R::new("u-mia", "mia"));
        dir.push(R::new("u-sub", "sub").groups(&["subtenants"]));
        snapshot(dir, at)
    };
    for i in 0..3 {
        let at = t0 + i * 15 * MIN;
        round(&ctx, &mut state, &dir(at), &residents, at);
    }
    assert!(state.is_member(&kitchen, &sub_in_plan));
    assert!(state.directory.held.iter().any(|(p, _)| p == &sub_in_plan));
}

// ── Conflicts and split people ──────────────────────────────────────────────

#[test]
fn an_account_two_residents_claim_counts_for_neither() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let kitchen = group(&mut state, "Kitchen", None);
    let shared = add(&mut state, "@shared:matrix.org", None, &[&kitchen]);
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let dir = || {
        vec![
            R::new("u-a", "a").link("@shared:matrix.org"),
            R::new("u-b", "b").link("@shared:matrix.org"),
        ]
    };
    let read = snapshot(dir(), t0);
    assert_eq!(
        person_account(Some(&read.people), &state, &id("@shared:matrix.org")),
        id("@shared:matrix.org")
    );
    assert!(recognised(Some(&read.people), &id("@shared:matrix.org")).is_empty());
    assert_eq!(
        logins(&read.people).of(person(&state, &shared)),
        None,
        "no calendar for it"
    );
    let mut told = Vec::new();
    for i in 0..3 {
        let at = t0 + i * 15 * MIN;
        told.extend(round(&ctx, &mut state, &snapshot(dir(), at), &rules(), at).notices);
    }
    assert!(person(&state, &shared).directory_id.is_none());
    assert!(state.is_member(&kitchen, &shared));
    // The admins hear of it once, with who claims it; again once it comes back.
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].contains("@shared:matrix.org") && told[0].contains("a and b"),
        "{}",
        told[0]
    );
    let at = t0 + 60 * MIN;
    let gone = vec![R::new("u-a", "a").link("@shared:matrix.org")];
    assert!(round(&ctx, &mut state, &snapshot(gone, at), &rules(), at)
        .notices
        .is_empty());
    let at = t0 + 75 * MIN;
    let out = round(&ctx, &mut state, &snapshot(dir(), at), &rules(), at);
    assert_eq!(out.notices.len(), 1, "{out:?}");
}

#[test]
fn two_people_in_use_for_one_resident_are_not_merged_and_told_once() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let kitchen = group(&mut state, "Kitchen", None);
    let one = add(&mut state, "@nick:house.org", None, &[&kitchen]);
    let two = add(&mut state, "@nick:matrix.org", None, &[&kitchen]);
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let dir = || vec![R::new("u-nick", "nick").link("@nick:matrix.org")];
    let out = round(&ctx, &mut state, &snapshot(dir(), t0), &rules(), t0);
    assert_eq!(out.notices.len(), 1, "{out:?}");
    assert!(out.notices[0].contains("2 people in the plan"));
    assert!(person(&state, &one).directory_id.is_none());
    assert!(person(&state, &two).directory_id.is_none());
    let read = snapshot(dir(), t0);
    for account in ["@nick:house.org", "@nick:matrix.org"] {
        assert_eq!(
            person_account(Some(&read.people), &state, &id(account)),
            id(account),
            "each stays itself"
        );
    }
    let t1 = t0 + 15 * MIN;
    let out = round(&ctx, &mut state, &snapshot(dir(), t1), &rules(), t1);
    assert!(out.notices.is_empty());
    assert_eq!(state.persons.len(), 2);
}

// ── G: rights never come from a link ────────────────────────────────────────

#[tokio::test]
async fn an_admins_linked_account_is_no_admin() {
    let mut state = State::default();
    state.cleaning_groups.push(CleaningGroup::new("Kitchen"));
    state.persons.push(Person::new_matrix("@admin:house.org"));
    let (ctx, _) = context(state);
    *ctx.directory.write().unwrap() = Some(Arc::new(snapshot(
        vec![R::new("u-admin", "admin").link("@admin:matrix.org")],
        SystemTime::now(),
    )));
    // The linked account is the admin's person in the plan …
    let linked = ctx.acting_as(&id("@admin:matrix.org"));
    let sender = linked.person_account(&id("@admin:matrix.org")).await;
    assert_eq!(sender, id("@admin:house.org"));
    // … but no admin.
    assert!(!linked.is_admin(&sender));
    let refused =
        crate::commands::add_matrix_participant(&linked, &sender, &["@new:house.org", "Kitchen"])
            .await
            .unwrap_err();
    assert!(refused.is::<mxbot_common::admin::NotAdmin>());
    // The listed account is.
    let admin = ctx.acting_as(&id("@admin:house.org"));
    assert!(admin.is_admin(&id("@admin:house.org")));
    // An admin adding another account of someone in the plan adds that person, not a second.
    let added = crate::commands::add_matrix_participant(
        &admin,
        &id("@admin:house.org"),
        &["@admin:matrix.org", "Kitchen"],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(added.contains("@admin:house.org"), "{added}");
    assert_eq!(ctx.state.lock().await.persons.len(), 1);
}

// ── Guards ──────────────────────────────────────────────────────────────────

/// Six neighbours in the kitchen, linked.
fn six() -> (State, GroupId, Vec<PersonId>) {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let kitchen = group(&mut state, "Kitchen", None);
    let ids = (0..6)
        .map(|i| {
            add(
                &mut state,
                &format!("@p{i}:house.org"),
                Some(&format!("u-{i}")),
                &[&kitchen],
            )
        })
        .collect();
    plan(&mut state);
    reviewed(&mut state);
    (state, kitchen, ids)
}

/// The six, the first `gone` of them moved out.
fn six_residents(gone: usize) -> Vec<R> {
    (0..6)
        .map(|i| {
            let r = R::new(&format!("u-{i}"), &format!("p{i}"));
            if i < gone {
                r.groups(&["former-residents"])
            } else {
                r
            }
        })
        .collect()
}

#[tokio::test]
async fn too_many_departures_are_held_until_an_admin_confirms() {
    let (mut state, kitchen, ids) = six();
    assert_eq!(mass_limit(&state, &kitchen), 2);
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    round(
        &ctx,
        &mut state,
        &snapshot(six_residents(3), t0),
        &rules(),
        t0,
    );
    let t1 = t0 + 15 * MIN;
    let out = round(
        &ctx,
        &mut state,
        &snapshot(six_residents(3), t1),
        &rules(),
        t1,
    );
    assert_eq!(out.notices.len(), 1, "{out:?}");
    let digest = held_digest(&state.directory.held);
    assert!(
        out.notices[0].contains(&format!("!directory confirm {digest}"))
            && out.notices[0].contains("Kitchen: 3, more than 2"),
        "{}",
        out.notices[0]
    );
    assert!(!out.plan_changed);
    assert!(ids.iter().all(|p| state.is_member(&kitchen, p)));
    assert_eq!(state.directory.held.len(), 3);
    // Still held, told once.
    let t2 = t1 + 15 * MIN;
    let out = round(
        &ctx,
        &mut state,
        &snapshot(six_residents(3), t2),
        &rules(),
        t2,
    );
    assert!(out.notices.is_empty());
    assert!(ids.iter().all(|p| state.is_member(&kitchen, p)));

    *ctx.state.lock().await = state;
    let read = snapshot(six_residents(3), t2);
    // Without the digest, or another one: nothing, the held ones and their digest shown.
    for wrong in ["", "00000000"] {
        let reply = confirm_held(&ctx, &read, &rules(), wrong, t2)
            .await
            .unwrap();
        assert!(
            reply.contains("nothing was done")
                && reply.contains(&format!("!directory confirm {digest}"))
                && reply.contains("@p0:house.org"),
            "{reply}"
        );
    }
    {
        let state = ctx.state.lock().await;
        assert!(ids.iter().all(|p| state.is_member(&kitchen, p)));
        assert!(!state.directory.held.is_empty());
    }
    // One of them is back in the directory: nothing confirmed.
    let changed = snapshot(six_residents(2), t2);
    let reply = confirm_held(&ctx, &changed, &rules(), &digest, t2)
        .await
        .unwrap();
    assert!(reply.contains("changed"), "{reply}");
    // A read too old: nothing.
    let old = snapshot(six_residents(3), t0);
    let reply = confirm_held(&ctx, &old, &rules(), &digest, t0 + 120 * MIN)
        .await
        .unwrap();
    assert!(reply.contains("too old"), "{reply}");
    // As held: they go, once, and the answer lists them.
    let reply = confirm_held(&ctx, &read, &rules(), &digest.to_uppercase(), t2)
        .await
        .unwrap();
    assert!(
        reply.starts_with(&format!("✅ Confirmed {digest}:")),
        "{reply}"
    );
    for i in 0..3 {
        assert!(reply.contains(&format!("@p{i}:house.org")), "{reply}");
    }
    assert!(!reply.contains("@p3:house.org"), "{reply}");
    {
        let state = ctx.state.lock().await;
        assert!(ids[..3].iter().all(|p| !state.is_member(&kitchen, p)));
        assert!(ids[3..].iter().all(|p| state.is_member(&kitchen, p)));
        assert!(state.directory.held.is_empty());
    }
    let reply = confirm_held(&ctx, &read, &rules(), &digest, t2)
        .await
        .unwrap();
    assert!(reply.contains("Nothing is held"));
}

#[test]
fn departures_within_the_limit_are_not_held() {
    let (mut state, kitchen, ids) = six();
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    round(
        &ctx,
        &mut state,
        &snapshot(six_residents(2), t0),
        &rules(),
        t0,
    );
    let t1 = t0 + 15 * MIN;
    let out = round(
        &ctx,
        &mut state,
        &snapshot(six_residents(2), t1),
        &rules(),
        t1,
    );
    assert_eq!(out.notices.len(), 2, "one per person: {out:?}");
    assert!(ids[..2].iter().all(|p| !state.is_member(&kitchen, p)));
    assert!(ids[2..].iter().all(|p| state.is_member(&kitchen, p)));
}

/// A kitchen-floor group emptied in the directory (a broken edit): the kitchen following it
/// is held as a whole; another group's departure within its limit still goes.
#[test]
fn a_kitchen_group_emptied_in_the_directory_is_held() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let k1 = group(&mut state, "Kitchen 1", Some("kitchen-floor-1"));
    let k3 = group(&mut state, "Kitchen 3", Some("kitchen-floor-3"));
    let ones: Vec<PersonId> = (0..6)
        .map(|i| {
            add(
                &mut state,
                &format!("@a{i}:house.org"),
                Some(&format!("u-a{i}")),
                &[&k1],
            )
        })
        .collect();
    let threes: Vec<PersonId> = (0..5)
        .map(|i| {
            add(
                &mut state,
                &format!("@b{i}:house.org"),
                Some(&format!("u-b{i}")),
                &[&k3],
            )
        })
        .collect();
    plan(&mut state);
    reviewed(&mut state);
    let (ctx, _) = context(State::default());
    // Everyone still lives here; kitchen-floor-1 lost all its members, b0 moved to kitchen 1.
    let dir = || {
        let mut d: Vec<R> = (0..6)
            .map(|i| R::new(&format!("u-a{i}"), &format!("a{i}")))
            .collect();
        d.extend((0..5).map(|i| {
            let r = R::new(&format!("u-b{i}"), &format!("b{i}"));
            if i == 0 {
                r
            } else {
                r.groups(&["residents", "kitchen-floor-3"])
            }
        }));
        d
    };
    let t0 = SystemTime::now();
    round(&ctx, &mut state, &snapshot(dir(), t0), &rules(), t0);
    let t1 = t0 + 15 * MIN;
    let out = round(&ctx, &mut state, &snapshot(dir(), t1), &rules(), t1);
    assert!(ones.iter().all(|p| state.is_member(&k1, p)));
    assert_eq!(state.directory.held.len(), 6);
    assert!(state.directory.held.iter().all(|(_, g)| g == &k1));
    assert!(!state.is_member(&k3, &threes[0]));
    assert!(threes[1..].iter().all(|p| state.is_member(&k3, p)));
    assert_eq!(out.notices.len(), 2, "{out:?}");
    assert!(
        out.notices
            .iter()
            .any(|n| n.contains("Kitchen 1: 6, more than 2")),
        "{out:?}"
    );
}

/// Moving out counts in every group someone leaves: three of a kitchen of five is too many
/// there (held), three of the fifteen on the staircase is not (they leave it).
#[test]
fn moving_out_counts_in_each_group_it_leaves() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let kitchen = group(&mut state, "Kitchen", None);
    let garden = group(&mut state, "Garden", None);
    let ids: Vec<PersonId> = (0..15)
        .map(|i| {
            let groups: Vec<&GroupId> = if i < 5 {
                vec![&kitchen, &garden]
            } else {
                vec![&garden]
            };
            add(
                &mut state,
                &format!("@p{i}:house.org"),
                Some(&format!("u-{i}")),
                &groups,
            )
        })
        .collect();
    plan(&mut state);
    reviewed(&mut state);
    assert_eq!(mass_limit(&state, &kitchen), 2);
    assert_eq!(mass_limit(&state, &garden), 3);
    let (ctx, _) = context(State::default());
    let dir = || {
        (0..15)
            .map(|i| {
                let r = R::new(&format!("u-{i}"), &format!("p{i}"));
                if i < 3 {
                    r.groups(&["former-residents"])
                } else {
                    r
                }
            })
            .collect::<Vec<_>>()
    };
    let t0 = SystemTime::now();
    round(&ctx, &mut state, &snapshot(dir(), t0), &rules(), t0);
    let t1 = t0 + 15 * MIN;
    round(&ctx, &mut state, &snapshot(dir(), t1), &rules(), t1);
    assert!(ids[..3].iter().all(|p| state.is_member(&kitchen, p)));
    assert!(ids[..3].iter().all(|p| !state.is_member(&garden, p)));
    assert_eq!(state.directory.held.len(), 3);
    assert!(state.directory.held.iter().all(|(_, g)| g == &kitchen));
}

/// The first departures after an upgrade (nothing remembered yet) wait for an admin however
/// few, kitchen moves too; after that the limits alone hold them. Switching residents on
/// later starts over.
#[tokio::test]
async fn the_first_departures_after_an_upgrade_are_held() {
    let (mut state, mia, kitchen, stairs) = mia_and_neighbours();
    state.directory = Default::default();
    let k1 = group(&mut state, "Kitchen 1", Some("kitchen-floor-1"));
    let bo = add(&mut state, "@bo:house.org", Some("u-bo"), &[&k1]);
    let (ctx, _) = context(State::default());
    let dir = |mia_moved: bool| {
        let mut d = neighbours();
        d.push(R::new("u-bo", "bo"));
        d.push(if mia_moved {
            R::new("u-mia", "mia").groups(&["former-residents"])
        } else {
            R::new("u-mia", "mia")
        });
        d
    };
    let t0 = SystemTime::now();
    round(&ctx, &mut state, &snapshot(dir(true), t0), &rules(), t0);
    let t1 = t0 + 15 * MIN;
    let out = round(&ctx, &mut state, &snapshot(dir(true), t1), &rules(), t1);
    // Mia (kitchen, staircase) and bo (no longer in kitchen-floor-1): all held.
    assert!(state.is_member(&kitchen, &mia) && state.is_member(&stairs, &mia));
    assert!(state.is_member(&k1, &bo));
    assert_eq!(state.directory.held.len(), 3);
    assert_eq!(out.notices.len(), 1, "{out:?}");
    assert!(
        out.notices[0].contains("first departures"),
        "{}",
        out.notices[0]
    );
    assert!(state.directory.reviewed.is_none());
    // Told once.
    let t2 = t1 + 15 * MIN;
    let out = round(&ctx, &mut state, &snapshot(dir(true), t2), &rules(), t2);
    assert!(out.notices.is_empty(), "{out:?}");
    // Confirmed: they go, and from now on the limits alone hold.
    let digest = held_digest(&state.directory.held);
    *ctx.state.lock().await = state;
    let reply = confirm_held(&ctx, &snapshot(dir(true), t2), &rules(), &digest, t2)
        .await
        .unwrap();
    assert!(reply.starts_with("✅ Confirmed"), "{reply}");
    let state = ctx.state.lock().await.clone();
    assert!(!state.is_member(&kitchen, &mia) && !state.is_member(&k1, &bo));
    assert_eq!(state.directory.reviewed.as_deref(), Some("residents"));

    // A fresh upgrade where nothing needs to leave: reviewed after two quiet reads.
    let (mut quiet, mia, kitchen, _) = mia_and_neighbours();
    quiet.directory = Default::default();
    let off = rules_of(&[]);
    for i in 0..2 {
        let at = t0 + i * 15 * MIN;
        round(&ctx, &mut quiet, &snapshot(dir(true), at), &off, at);
    }
    assert_eq!(quiet.directory.reviewed.as_deref(), Some(""));
    assert!(quiet.is_member(&kitchen, &mia), "residents not followed");
    // Residents switched on later: their first departures are held again.
    for i in 2..4 {
        let at = t0 + i * 15 * MIN;
        round(&ctx, &mut quiet, &snapshot(dir(true), at), &rules(), at);
    }
    assert!(quiet.is_member(&kitchen, &mia));
    assert_eq!(quiet.directory.held.len(), 2);
}

#[test]
fn a_stale_read_changes_nothing_and_reads_far_apart_are_not_in_a_row() {
    let (state, kitchen, ids) = six();
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let mut unlinked = state.clone();
    for p in &mut unlinked.persons {
        p.directory_id = None;
    }
    // Too old to act on: not even a link.
    let before = unlinked.event_log.len();
    let out = round(
        &ctx,
        &mut unlinked,
        &snapshot(six_residents(1), t0),
        &rules(),
        t0 + 61 * MIN,
    );
    assert_eq!(out, Round::default());
    assert_eq!(unlinked.event_log.len(), before);
    assert!(unlinked.persons.iter().all(|p| p.directory_id.is_none()));
    assert_eq!(unlinked.directory, state.directory);

    // Missing at t0; then no good read for three hours (failed reads never get here):
    // the read after that is a first one again.
    let mut state = state;
    round(
        &ctx,
        &mut state,
        &snapshot(six_residents(1), t0),
        &rules(),
        t0,
    );
    let t1 = t0 + 180 * MIN;
    round(
        &ctx,
        &mut state,
        &snapshot(six_residents(1), t1),
        &rules(),
        t1,
    );
    assert!(state.is_member(&kitchen, &ids[0]));
    let t2 = t1 + 15 * MIN;
    round(
        &ctx,
        &mut state,
        &snapshot(six_residents(1), t2),
        &rules(),
        t2,
    );
    assert!(!state.is_member(&kitchen, &ids[0]));

    // Back in between: the count starts again.
    let (mut state, kitchen, ids) = six();
    round(
        &ctx,
        &mut state,
        &snapshot(six_residents(1), t0),
        &rules(),
        t0,
    );
    let t1 = t0 + 15 * MIN;
    round(
        &ctx,
        &mut state,
        &snapshot(six_residents(0), t1),
        &rules(),
        t1,
    );
    let t2 = t1 + 15 * MIN;
    round(
        &ctx,
        &mut state,
        &snapshot(six_residents(1), t2),
        &rules(),
        t2,
    );
    assert!(state.is_member(&kitchen, &ids[0]));
}

#[test]
fn an_empty_or_missing_residents_group_removes_nobody() {
    let (mut state, kitchen, ids) = six();
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    let nobody_resident = || {
        (0..6)
            .map(|i| R::new(&format!("u-{i}"), &format!("p{i}")).groups(&[]))
            .collect::<Vec<_>>()
    };
    for i in 0..3 {
        let at = t0 + i * 15 * MIN;
        round(
            &ctx,
            &mut state,
            &snapshot(nobody_resident(), at),
            &rules(),
            at,
        );
    }
    assert!(ids.iter().all(|p| state.is_member(&kitchen, p)));
    // The group itself missing from the directory (a typo, a rename): the same.
    for i in 3..6 {
        let at = t0 + i * 15 * MIN;
        let mut read = snapshot(six_residents(2), at);
        read.groups.remove("residents");
        round(&ctx, &mut state, &read, &rules(), at);
    }
    assert!(ids.iter().all(|p| state.is_member(&kitchen, p)));
    // Not followed at all (`residents_group = ""`).
    let off = rules_of(&[]);
    for i in 6..9 {
        let at = t0 + i * 15 * MIN;
        round(&ctx, &mut state, &snapshot(six_residents(2), at), &off, at);
    }
    assert!(ids.iter().all(|p| state.is_member(&kitchen, p)));
}

#[test]
fn a_kitchen_missing_from_the_directory_is_left_alone() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let k4 = group(&mut state, "Kitchen 4", Some("kitchen-floor-4"));
    let ana = add(&mut state, "@ana:house.org", Some("u-ana"), &[&k4]);
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    for i in 0..3 {
        let at = t0 + i * 15 * MIN;
        let read = snapshot(vec![R::new("u-ana", "ana")], at);
        round(&ctx, &mut state, &read, &rules(), at);
    }
    assert!(state.is_member(&k4, &ana));
}

#[test]
fn a_restart_between_the_two_reads_remembers_the_first() {
    let (mut state, kitchen, ids) = six();
    let (ctx, _) = context(State::default());
    let t0 = SystemTime::now();
    round(
        &ctx,
        &mut state,
        &snapshot(six_residents(1), t0),
        &rules(),
        t0,
    );
    assert_eq!(state.directory.missing.len(), 1);
    // Saved and loaded again (a restart).
    let json = serde_json::to_string(&state).unwrap();
    let mut state: State = serde_json::from_str(&json).unwrap();
    assert_eq!(state.directory.missing.len(), 1);
    let t1 = t0 + 15 * MIN;
    round(
        &ctx,
        &mut state,
        &snapshot(six_residents(1), t1),
        &rules(),
        t1,
    );
    assert!(!state.is_member(&kitchen, &ids[0]));
}

#[test]
fn an_old_state_file_loads_unchanged() {
    let json = r#"{
        "persons": [{"id": "p1", "display_name": "Mia", "active": true,
                     "matrix_id": "@mia:house.org"}],
        "cleaning_groups": [{"id": "g1", "name": "Kitchen", "member_ids": ["p1"]}],
        "event_log": [{"id": "e1", "timestamp": "2026-01-05T10:00:00Z",
                       "event": {"kind": "person_matrix_linked", "person_id": "p1",
                                 "matrix_id": "@mia:house.org"}}],
        "created_at": null
    }"#;
    let state: State = serde_json::from_str(json).unwrap();
    assert!(state.persons[0].directory_id.is_none());
    assert!(state.cleaning_groups[0].directory_group.is_none());
    assert!(state.directory.is_empty());
    assert_eq!(state.event_log.len(), 1);
    // Written back without the new fields while they are unset.
    let again = serde_json::to_value(&state).unwrap();
    assert!(again.get("directory").is_none());
    assert!(again["persons"][0].get("directory_id").is_none());
    assert!(again["cleaning_groups"][0].get("directory_group").is_none());
}

#[test]
fn logins_follow_the_directory_id_and_deactivated_people_have_none() {
    let read = people(vec![
        R::new("u-ana", "ana").link("@ana:matrix.org"),
        R::new("u-bo", "bo").deactivated(),
    ]);
    let all = logins(&read);
    let mut ana = Person::new_matrix("@ana:matrix.org");
    assert_eq!(all.of(&ana).map(String::as_str), Some("ana"));
    ana.matrix_id = Some("@somewhere:else.org".into());
    ana.directory_id = Some("u-ana".into());
    assert_eq!(all.of(&ana).map(String::as_str), Some("ana"));
    let mut bo = Person::new_matrix("@bo:house.org");
    assert_eq!(all.of(&bo), None);
    bo.directory_id = Some("u-bo".into());
    assert_eq!(all.of(&bo), None);
}

/// Person G: an admin by `[security] admin_groups` (the house account only) while the read is
/// fresh; out of the group, or a read too old, and only `admin_users` remain.
#[test]
fn admins_from_the_directory_groups_follow_them_and_lapse_with_an_old_read() {
    let (ctx, path) = context(State::default());
    let reader = ctx.config.directory.as_ref().unwrap().reader.clone();
    let groups = ["it-admins".to_owned()];
    let now = SystemTime::now();
    let gina = || R::new("g", "gina").link("@gina:matrix.org");
    ctx.admin_users.follow(
        &snapshot(vec![gina().groups(&["residents", "it-admins"])], now),
        &reader,
        &groups,
    );
    assert!(ctx.is_admin(&id("@gina:house.org")));
    // a linked account is never an admin
    assert!(!ctx.is_admin(&id("@gina:matrix.org")));
    assert!(ctx.is_admin(&id("@admin:house.org")));
    // she leaves the group: no longer an admin at the next read
    ctx.admin_users.follow(
        &snapshot(vec![gina().groups(&["residents"])], now),
        &reader,
        &groups,
    );
    assert!(!ctx.is_admin(&id("@gina:house.org")));
    // a read older than stale_after gives nothing
    ctx.admin_users.follow(
        &snapshot(
            vec![gina().groups(&["it-admins"])],
            now - reader.stale_after() - std::time::Duration::from_secs(1),
        ),
        &reader,
        &groups,
    );
    assert!(!ctx.is_admin(&id("@gina:house.org")));
    assert!(ctx.is_admin(&id("@admin:house.org")));
    let _ = std::fs::remove_file(path);
}

/// A directory that is down is an outage (unavailable, never unhealthy); one that refuses the
/// token or answers what the bot cannot read is a failure.
#[tokio::test]
async fn only_a_directory_that_is_down_is_an_outage() {
    use mxbot_common::supervise::{report, snapshot, Outcome};
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
    let read = |status: u16| async move {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let e = client
            .get(server.uri())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .map_err(anyhow::Error::from)
            .unwrap_err();
        e.context("reading the directory")
    };
    let alive = report("cleaning directory under test");
    let last = || {
        snapshot()
            .into_iter()
            .find(|t| t.name == "cleaning directory under test")
            .unwrap()
            .last_outcome
    };
    report_failed_read(&alive, &read(503).await);
    assert_eq!(last(), Some(Outcome::Unavailable));
    report_failed_read(&alive, &read(403).await);
    assert_eq!(last(), Some(Outcome::Failed));
    report_failed_read(
        &alive,
        &anyhow::anyhow!("the directory's answer has no groups"),
    );
    assert_eq!(last(), Some(Outcome::Failed));
}
