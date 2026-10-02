use super::*;
use crate::{
    config::Config,
    domain::{CleaningSlot, SlotAssignment},
    state::State,
};
use chrono::Datelike;
use matrix_sdk::ruma::OwnedRoomId;
use std::{collections::HashSet, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

fn rotation_state() -> (State, GroupId, PersonId, PersonId) {
    let first = Person::new_matrix("@alice:example.org");
    let second = Person::new_matrix("@bob:example.org");
    let first_id = first.id.clone();
    let second_id = second.id.clone();
    let mut group = CleaningGroup::new("2nd Floor");
    let group_id = group.id.clone();
    group.member_ids = vec![first_id.clone(), second_id.clone()];

    let mut state = State::default();
    state.persons = vec![first, second];
    state.cleaning_groups.push(group);
    (state, group_id, first_id, second_id)
}

#[test]
fn matrix_user_id_validation_is_strict() {
    assert!(validate_matrix_user_id("@alice:example.org").is_ok());
    assert!(validate_matrix_user_id("alice").is_err());
    assert!(validate_matrix_user_id("@alice").is_err());
}

#[test]
fn removing_future_assignments_is_targeted_and_keeps_current_week() {
    let (mut state, group_id, first_id, second_id) = rotation_state();
    let (year, week) = current_iso_week();
    let (next_year, next_week) = add_weeks(year, week, 1);
    state.slot_assignments = vec![
        SlotAssignment {
            group_id: group_id.clone(),
            slot_index: 0,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(first_id.clone()),
            source: Default::default(),
        },
        SlotAssignment {
            group_id: group_id.clone(),
            slot_index: 0,
            iso_year: next_year,
            iso_week: next_week,
            shift: 0,
            person_id: Some(first_id.clone()),
            source: Default::default(),
        },
        SlotAssignment {
            group_id: group_id.clone(),
            slot_index: 1,
            iso_year: next_year,
            iso_week: next_week,
            shift: 0,
            person_id: Some(second_id.clone()),
            source: Default::default(),
        },
    ];

    let removed = remove_future_assignments_for_person(&mut state, &first_id, &group_id);

    assert_eq!(removed.len(), 1);
    assert!(state.slot_assignments.iter().any(|assignment| {
        assignment.iso_year == year
            && assignment.iso_week == week
            && assignment.person_id.as_deref() == Some(first_id.as_str())
    }));
    assert!(state.slot_assignments.iter().any(|assignment| {
        assignment.iso_year == next_year
            && assignment.iso_week == next_week
            && assignment.person_id.as_deref() == Some(second_id.as_str())
    }));
}

#[test]
fn current_open_assignment_blocks_removal_until_completed() {
    let (mut state, group_id, first_id, _) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id.clone()),
        source: Default::default(),
    });

    assert_eq!(
        current_open_assignments(&state, &group_id, &first_id),
        vec!["2nd Floor"]
    );

    state
        .apply_event(DomainEvent::CleaningCompleted {
            group_id: group_id.clone(),
            slot_id: None,
            person_id: first_id.clone(),
            responsible_person_ids: vec![first_id.clone()],
            iso_year: year,
            iso_week: week,
            shift: 0,
        })
        .unwrap();

    assert!(current_open_assignments(&state, &group_id, &first_id).is_empty());
    assert_eq!(
        state.completions.len(),
        1,
        "completed history must remain stored"
    );
}

#[test]
fn empty_rotation_has_an_explicit_next_status() {
    let (mut state, group_id, _, _) = rotation_state();
    state
        .group_by_name_mut("2nd Floor")
        .unwrap()
        .member_ids
        .clear();
    assert_eq!(
        next_assignment_summary(&state, &group_id),
        "Next: rotation is empty."
    );
}

fn test_context(state: State) -> (BotContext, PathBuf, OwnedUserId) {
    let config: Config = toml::from_str(
        r#"
        [matrix]
        homeserver = "https://matrix.example.org"
        user_id = "@cleaningbot:example.org"
        access_token = "test"
        device_id = "TEST"

        [security]
        admin_users = ["@admin:example.org"]

        [schedule]
        room_id = "!room:example.org"
        interval_weeks = 1
        materialize_weeks = 4
    "#,
    )
    .unwrap();
    let admin = OwnedUserId::try_from("@admin:example.org").unwrap();
    let path = std::env::temp_dir().join(format!("cleaning-bot-test-{}.json", Uuid::new_v4()));
    let ctx = BotContext {
        state: Arc::new(Mutex::new(state)),
        state_path: path.clone(),
        config: Arc::new(config),
        admin_users: HashSet::from([admin.clone()]),
        room_id: OwnedRoomId::try_from("!room:example.org").unwrap(),
    };
    (ctx, path, admin)
}

/// Same as `test_context` but with a configurable materialize horizon,
/// for tests that need to pre-seed several already-frozen weeks and then
/// still have room for a join/leave to reach a genuinely new week.
fn test_context_with_horizon(
    state: State,
    materialize_weeks: u32,
) -> (BotContext, PathBuf, OwnedUserId) {
    let config: Config = toml::from_str(&format!(
        r#"
        [matrix]
        homeserver = "https://matrix.example.org"
        user_id = "@cleaningbot:example.org"
        access_token = "test"
        device_id = "TEST"

        [security]
        admin_users = ["@admin:example.org"]

        [schedule]
        room_id = "!room:example.org"
        interval_weeks = 1
        materialize_weeks = {materialize_weeks}
    "#
    ))
    .unwrap();
    let admin = OwnedUserId::try_from("@admin:example.org").unwrap();
    let path = std::env::temp_dir().join(format!("cleaning-bot-test-{}.json", Uuid::new_v4()));
    let ctx = BotContext {
        state: Arc::new(Mutex::new(state)),
        state_path: path.clone(),
        config: Arc::new(config),
        admin_users: HashSet::from([admin.clone()]),
        room_id: OwnedRoomId::try_from("!room:example.org").unwrap(),
    };
    (ctx, path, admin)
}

/// Three-member single-slot group, ready for `resolver::materialize`.
fn three_person_state() -> (State, GroupId, PersonId, PersonId, PersonId) {
    let anna = Person::new_named("Anna");
    let bob = Person::new_named("Bob");
    let carla = Person::new_named("Carla");
    let (aid, bid, cid) = (anna.id.clone(), bob.id.clone(), carla.id.clone());
    let mut group = CleaningGroup::new("Floor");
    let gid = group.id.clone();
    group.member_ids = vec![aid.clone(), bid.clone(), cid.clone()];
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    state.persons = vec![anna, bob, carla];
    state.cleaning_groups.push(group);
    (state, gid, aid, bid, cid)
}

/// Materialize and apply `weeks_ahead` weeks directly against `state` —
/// a test-only shortcut around `resolver::materialize` for pre-seeding
/// "already planned" weeks before exercising a join/leave command.
fn seed_materialized_weeks(state: &mut State, weeks_ahead: usize) {
    for ev in resolver::materialize(state, weeks_ahead) {
        state.apply_event(ev).unwrap();
    }
}

fn assignee_for(state: &State, group_id: &GroupId, year: i32, week: u32) -> Option<PersonId> {
    state
        .slot_assignments
        .iter()
        .find(|a| a.group_id == *group_id && a.iso_year == year && a.iso_week == week)
        .and_then(|a| a.person_id.clone())
}

/// Like `assignee_for`, but also answers for weeks beyond the frozen
/// horizon via the preview fallback — used to check that the *eventual*
/// rotation still cycles correctly even where join/leave deliberately
/// didn't extend the frozen horizon that far.
fn preview_assignee_for(
    state: &State,
    group_id: &GroupId,
    year: i32,
    week: u32,
) -> Option<PersonId> {
    let group = state.group_by_id(group_id).unwrap();
    state
        .slot_assignee(group, 0, Turn::new(year, week, 0))
        .map(|p| p.id.clone())
}

#[tokio::test]
async fn matrix_participant_normal_flow_persists_and_rejects_duplicates() {
    let mut state = State::default();
    state.cleaning_groups.push(CleaningGroup::new("2nd Floor"));
    let (ctx, path, admin) = test_context(state);

    let added = add_matrix_participant(&ctx, &admin, &["@new:example.org", "2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(added.contains("Added @new:example.org"));

    {
        let state = ctx.state.lock().await;
        let group = state.group_by_name("2nd Floor").unwrap();
        assert_eq!(group.member_ids.len(), 1);
        let new_id = state
            .person_by_matrix_id("@new:example.org")
            .unwrap()
            .id
            .as_str();
        let (year, week) = current_iso_week();
        assert!(
            !state.slot_assignments.iter().any(|assignment| {
                assignment.group_id == group.id
                    && assignment.iso_year == year
                    && assignment.iso_week == week
                    && assignment.person_id.as_deref() == Some(new_id)
            }),
            "the first member must not inherit the active week"
        );
    }
    let persisted = State::load(&path).await.unwrap();
    assert_eq!(
        persisted
            .group_by_name("2nd Floor")
            .unwrap()
            .member_ids
            .len(),
        1
    );

    let duplicate = add_matrix_participant(&ctx, &admin, &["@new:example.org", "2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(duplicate.contains("already in"));

    let removed = remove_matrix_participant(&ctx, &admin, &["@new:example.org", "2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(removed.contains("Removed @new:example.org"));
    assert!(ctx
        .state
        .lock()
        .await
        .group_by_name("2nd Floor")
        .unwrap()
        .member_ids
        .is_empty());

    let missing = remove_matrix_participant(&ctx, &admin, &["@new:example.org", "2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(missing.contains("is not in"));

    let persisted = State::load(&path).await.unwrap();
    assert!(persisted
        .group_by_name("2nd Floor")
        .unwrap()
        .member_ids
        .is_empty());
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn adding_requires_admin_and_preserves_the_active_assignment() {
    let (mut state, group_id, first_id, _) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id.clone()),
        source: Default::default(),
    });
    let (ctx, path, admin) = test_context(state);
    let outsider = OwnedUserId::try_from("@outsider:example.org").unwrap();

    let error = add_matrix_participant(&ctx, &outsider, &["@charlie:example.org", "2nd Floor"])
        .await
        .unwrap_err();
    assert!(error.is::<mxbot_common::admin::NotAdmin>());

    let malformed = add_matrix_participant(&ctx, &admin, &["charlie", "2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(malformed.contains("not a valid Matrix user ID"));

    add_matrix_participant(&ctx, &admin, &["@charlie:example.org", "2nd Floor"])
        .await
        .unwrap();

    let state = ctx.state.lock().await;
    let group = state.group_by_id(&group_id).unwrap();
    let charlie_id = &state
        .person_by_matrix_id("@charlie:example.org")
        .unwrap()
        .id;
    assert_eq!(
        group.member_ids.last(),
        Some(charlie_id),
        "new members append to the rotation"
    );
    assert!(
        state.slot_assignments.iter().any(|assignment| {
            assignment.group_id == group_id
                && assignment.iso_year == year
                && assignment.iso_week == week
                && assignment.person_id.as_deref() == Some(first_id.as_str())
        }),
        "the active assignment must remain frozen"
    );
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn assign_overrides_current_week_and_persists() {
    let (state, group_id, _first_id, second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);

    let reply = cmd_assign(&ctx, &admin, &["2nd Floor", "@bob:example.org"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("Assigned"), "{reply}");
    assert!(reply.contains("2nd Floor"), "{reply}");

    let (year, week) = current_iso_week();
    {
        let state = ctx.state.lock().await;
        let assignment = state
            .slot_assignments
            .iter()
            .find(|a| a.group_id == group_id && a.iso_year == year && a.iso_week == week)
            .expect("manual assignment must be stored");
        assert_eq!(assignment.person_id.as_deref(), Some(second_id.as_str()));
        assert_eq!(assignment.source, AssignmentSource::Assign);
    }

    let persisted = State::load(&path).await.unwrap();
    assert!(
        persisted.slot_assignments.iter().any(|a| {
            a.group_id == group_id
                && a.iso_year == year
                && a.iso_week == week
                && a.person_id.as_deref() == Some(second_id.as_str())
        }),
        "manual assignment must survive a reload"
    );

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn assign_requires_admin() {
    let (state, ..) = rotation_state();
    let (ctx, path, _admin) = test_context(state);
    let outsider = OwnedUserId::try_from("@outsider:example.org").unwrap();

    let error = cmd_assign(&ctx, &outsider, &["2nd Floor", "@bob:example.org"])
        .await
        .unwrap_err();
    assert!(error.is::<mxbot_common::admin::NotAdmin>());

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn assign_reports_unknown_group_or_person() {
    let (state, ..) = rotation_state();
    let (ctx, path, admin) = test_context(state);

    let no_group = cmd_assign(&ctx, &admin, &["Basement", "@bob:example.org"])
        .await
        .unwrap()
        .unwrap();
    assert!(no_group.contains("not found"), "{no_group}");

    let no_person = cmd_assign(&ctx, &admin, &["2nd Floor", "@charlie:example.org"])
        .await
        .unwrap()
        .unwrap();
    assert!(no_person.contains("not registered"), "{no_person}");

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn assign_flags_a_person_outside_the_rotation() {
    let (mut state, ..) = rotation_state();
    state.persons.push(Person::new_named("Guest"));
    let (ctx, path, admin) = test_context(state);

    let reply = cmd_assign(&ctx, &admin, &["2nd Floor", "Guest"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("one-off assignment"), "{reply}");

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn assign_multi_slot_requires_a_valid_slot_name() {
    let mut state = State::default();
    let alice = Person::new_named("Alice");
    let bob = Person::new_named("Bob");
    let (alice_id, bob_id) = (alice.id.clone(), bob.id.clone());
    state.persons = vec![alice, bob];
    let mut group = CleaningGroup::new("Floor");
    let group_id = group.id.clone();
    group.member_ids = vec![alice_id, bob_id.clone()];
    let mut scharni = CleaningSlot::new("Scharni");
    scharni.id = "s0".into();
    let mut colbe = CleaningSlot::new("Colbe");
    colbe.id = "s1".into();
    group.slots = vec![scharni, colbe];
    state.cleaning_groups.push(group);
    let (ctx, path, admin) = test_context(state);

    let missing_slot = cmd_assign(&ctx, &admin, &["Floor", "Bob"])
        .await
        .unwrap()
        .unwrap();
    assert!(missing_slot.contains("has slots"), "{missing_slot}");

    let ok = cmd_assign(&ctx, &admin, &["Floor", "Colbe", "Bob"])
        .await
        .unwrap()
        .unwrap();
    assert!(ok.contains("Colbe"), "{ok}");

    let (year, week) = current_iso_week();
    let state = ctx.state.lock().await;
    let assignment = state
        .slot_assignments
        .iter()
        .find(|a| a.group_id == group_id && a.iso_year == year && a.iso_week == week)
        .expect("manual assignment must be stored");
    assert_eq!(assignment.slot_index, 1, "Colbe is the second slot");
    assert_eq!(assignment.person_id.as_deref(), Some(bob_id.as_str()));
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn unassign_clears_a_manual_assignment() {
    let (state, group_id, _first_id, _second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);

    cmd_assign(&ctx, &admin, &["2nd Floor", "@bob:example.org"])
        .await
        .unwrap();
    let reply = cmd_unassign(&ctx, &admin, &["2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("Cleared"), "{reply}");

    let (year, week) = current_iso_week();
    let state = ctx.state.lock().await;
    let assignment = state
        .slot_assignments
        .iter()
        .find(|a| a.group_id == group_id && a.iso_year == year && a.iso_week == week)
        .expect("assignment record must still exist");
    assert!(assignment.person_id.is_none());
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

// ── !importplan ───────────────────────────────────────────────────────────

fn iso_week_token(year: i32, week: u32) -> String {
    format!("{year}-W{week:02}")
}

#[tokio::test]
async fn import_freezes_a_future_week_with_import_source() {
    let (state, group_id, _first_id, second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();
    let (ny, nw) = add_weeks(cy, cw, 1);

    let reply = cmd_importplan(
        &ctx,
        &admin,
        &[&iso_week_token(ny, nw), "2nd Floor", "@bob:example.org"],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.contains("1 added"), "{reply}");

    let state = ctx.state.lock().await;
    let a = state
        .slot_assignments
        .iter()
        .find(|a| a.group_id == group_id && a.iso_year == ny && a.iso_week == nw)
        .expect("imported assignment must be stored");
    assert_eq!(a.person_id.as_deref(), Some(second_id.as_str()));
    assert_eq!(a.source, AssignmentSource::Import);
    drop(state);

    let persisted = State::load(&path).await.unwrap();
    assert!(
        persisted.slot_assignments.iter().any(|a| {
            a.group_id == group_id
                && a.iso_year == ny
                && a.iso_week == nw
                && a.person_id.as_deref() == Some(second_id.as_str())
        }),
        "imported assignment must survive a reload"
    );

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_requires_admin() {
    let (state, ..) = rotation_state();
    let (ctx, path, _admin) = test_context(state);
    let outsider = OwnedUserId::try_from("@outsider:example.org").unwrap();
    let (cy, cw) = current_iso_week();
    let (ny, nw) = add_weeks(cy, cw, 1);

    let error = cmd_importplan(
        &ctx,
        &outsider,
        &[&iso_week_token(ny, nw), "2nd Floor", "@bob:example.org"],
    )
    .await
    .unwrap_err();
    assert!(error.is::<mxbot_common::admin::NotAdmin>());

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_rejects_a_past_week_and_changes_nothing() {
    let (state, ..) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();
    let (py, pw) = add_weeks(cy, cw, -1);

    let reply = cmd_importplan(
        &ctx,
        &admin,
        &[&iso_week_token(py, pw), "2nd Floor", "@bob:example.org"],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.contains("aborted"), "{reply}");
    assert!(reply.contains("past"), "{reply}");

    let state = ctx.state.lock().await;
    assert!(
        state.slot_assignments.is_empty(),
        "a rejected import must not write anything"
    );
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_rejects_unknown_group_or_person_and_changes_nothing() {
    let (state, ..) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();
    let (ny, nw) = add_weeks(cy, cw, 1);

    let no_group = cmd_importplan(
        &ctx,
        &admin,
        &[&iso_week_token(ny, nw), "Basement", "@bob:example.org"],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(no_group.contains("not found"), "{no_group}");

    let no_person = cmd_importplan(
        &ctx,
        &admin,
        &[&iso_week_token(ny, nw), "2nd Floor", "@charlie:example.org"],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(no_person.contains("not registered"), "{no_person}");

    let state = ctx.state.lock().await;
    assert!(state.slot_assignments.is_empty());
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_rejects_conflicting_entries_within_the_same_batch() {
    let (state, group_id, first_id, second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();
    let (ny, nw) = add_weeks(cy, cw, 1);
    let week_tok = iso_week_token(ny, nw);

    let reply = cmd_importplan(
        &ctx,
        &admin,
        &[
            &week_tok,
            "2nd Floor",
            "@alice:example.org",
            ";",
            &week_tok,
            "2nd Floor",
            "@bob:example.org",
        ],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.contains("aborted"), "{reply}");
    assert!(reply.contains("conflicting entries"), "{reply}");

    let state = ctx.state.lock().await;
    assert!(
        state
            .slot_assignments
            .iter()
            .all(|a| !(a.group_id == group_id && a.iso_year == ny && a.iso_week == nw)),
        "neither conflicting entry may be written"
    );
    drop(state);
    let _ = (first_id, second_id);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_rejects_conflict_with_an_already_frozen_different_assignee() {
    let (state, group_id, first_id, _second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();
    let (ny, nw) = add_weeks(cy, cw, 1);

    // Alice is already frozen in via a normal admin !assign.
    cmd_assign(
        &ctx,
        &admin,
        &["2nd Floor", "@alice:example.org", "week", &nw.to_string()],
    )
    .await
    .unwrap();

    let reply = cmd_importplan(
        &ctx,
        &admin,
        &[&iso_week_token(ny, nw), "2nd Floor", "@bob:example.org"],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.contains("aborted"), "{reply}");
    assert!(reply.contains("already assigned"), "{reply}");

    let state = ctx.state.lock().await;
    let a = state
        .slot_assignments
        .iter()
        .find(|a| a.group_id == group_id && a.iso_year == ny && a.iso_week == nw)
        .unwrap();
    assert_eq!(
        a.person_id.as_deref(),
        Some(first_id.as_str()),
        "existing assignment must survive untouched"
    );
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_is_idempotent_across_repeated_identical_runs() {
    let (state, group_id, _first_id, second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();
    let (ny, nw) = add_weeks(cy, cw, 1);
    let args = [
        iso_week_token(ny, nw),
        "2nd Floor".to_owned(),
        "@bob:example.org".to_owned(),
    ];
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let first = cmd_importplan(&ctx, &admin, &arg_refs)
        .await
        .unwrap()
        .unwrap();
    assert!(first.contains("1 added"), "{first}");

    let second = cmd_importplan(&ctx, &admin, &arg_refs)
        .await
        .unwrap()
        .unwrap();
    assert!(second.contains("Nothing to do"), "{second}");
    assert!(second.contains("already imported"), "{second}");

    let state = ctx.state.lock().await;
    let matches: Vec<_> = state
        .slot_assignments
        .iter()
        .filter(|a| a.group_id == group_id && a.iso_year == ny && a.iso_week == nw)
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "repeated import must not duplicate the assignment"
    );
    assert_eq!(matches[0].person_id.as_deref(), Some(second_id.as_str()));
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_multiple_entries_in_one_command() {
    let (state, group_id, first_id, second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();
    let (w1y, w1w) = add_weeks(cy, cw, 1);
    let (w2y, w2w) = add_weeks(cy, cw, 2);

    let reply = cmd_importplan(
        &ctx,
        &admin,
        &[
            &iso_week_token(w1y, w1w),
            "2nd Floor",
            "@alice:example.org",
            ";",
            &iso_week_token(w2y, w2w),
            "2nd Floor",
            "@bob:example.org",
        ],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.contains("2 added"), "{reply}");

    let state = ctx.state.lock().await;
    assert_eq!(
        state
            .slot_assignments
            .iter()
            .find(|a| a.group_id == group_id && a.iso_year == w1y && a.iso_week == w1w)
            .and_then(|a| a.person_id.as_deref()),
        Some(first_id.as_str()),
    );
    assert_eq!(
        state
            .slot_assignments
            .iter()
            .find(|a| a.group_id == group_id && a.iso_year == w2y && a.iso_week == w2w)
            .and_then(|a| a.person_id.as_deref()),
        Some(second_id.as_str()),
    );
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_does_not_disturb_the_rotation_queue_and_round_robin_continues_after_it() {
    // An already-running group has a real, non-empty `rotation_queue` —
    // unlike a brand-new group, `reconcile_queue` leaves an existing
    // queue untouched rather than reseeding/shifting it, so this is the
    // realistic case the "don't disturb rotation_queue" requirement is
    // actually about.
    let (mut raw_state, group_id, anna_id, bob_id, carla_id) = three_person_state();
    {
        let group = raw_state
            .cleaning_groups
            .iter_mut()
            .find(|g| g.id == group_id)
            .unwrap();
        group.rotation_queue = vec![anna_id.clone(), bob_id.clone(), carla_id.clone()];
    }
    let (ctx, path, admin) = test_context(raw_state);

    let (fy, fw) = current_iso_week();

    // Paper plan says Carla covers the very next due week — out of the
    // natural Anna → Bob → Carla order the queue would produce.
    let reply = cmd_importplan(&ctx, &admin, &[&iso_week_token(fy, fw), "Floor", "Carla"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("1 added"), "{reply}");

    {
        let state = ctx.state.lock().await;
        let queue = &state.group_by_id(&group_id).unwrap().rotation_queue;
        assert_eq!(
            queue,
            &vec![anna_id.clone(), bob_id.clone(), carla_id.clone()],
            "importing must not pop or reorder the rotation queue"
        );
    }

    // Fill this and the next two due weeks the normal way.
    let events = {
        let state = ctx.state.lock().await;
        resolver::materialize(&state, 3)
    };
    {
        let mut state = ctx.state.lock().await;
        for e in events {
            state.apply_event(e).unwrap();
        }
    }

    let state = ctx.state.lock().await;
    let at = |y: i32, w: u32| {
        state
            .slot_assignments
            .iter()
            .find(|a| a.group_id == group_id && a.iso_year == y && a.iso_week == w)
            .cloned()
    };

    let imported = at(fy, fw).expect("imported week must still be recorded");
    assert_eq!(
        imported.person_id.as_deref(),
        Some(carla_id.as_str()),
        "materialize must not re-decide an already-imported week"
    );
    assert_eq!(
        imported.source,
        AssignmentSource::Import,
        "materialize must not overwrite the import's audit source"
    );

    let (y1, w1) = add_weeks(fy, fw, 1);
    let (y2, w2) = add_weeks(fy, fw, 2);
    let next = at(y1, w1).expect("the week right after the import must be auto-filled");
    let next2 = at(y2, w2).expect("the week after that must be auto-filled too");

    // The queue never advanced during import, so round-robin resumes
    // exactly where it would have started — Anna first, then Bob —
    // deterministically, not from wherever Carla's import "left off".
    assert_eq!(
        next.person_id.as_deref(),
        Some(anna_id.as_str()),
        "rotation must resume at the front of the untouched queue"
    );
    assert_eq!(next2.person_id.as_deref(), Some(bob_id.as_str()));
    assert_eq!(next.source, AssignmentSource::RoundRobin);
    assert_eq!(next2.source, AssignmentSource::RoundRobin);
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

// ── Integration: import → render → mentions ─────────────────────────────────
//
// `!importplan` writes state the same way `!assign` does, but nothing above
// checks that the *displayed* plan (what `refresh_pinned_plan` would render
// and send) actually reflects an import immediately, or that the render
// still produces real Matrix mentions afterward. These tie the two
// features together end-to-end through the same pure building blocks
// `refresh_pinned_plan` uses (`build_weekly_plan` + `mentionify_with_names`),
// since there's no Room-mocking infrastructure in this codebase to drive
// `refresh_pinned_plan` itself.

#[test]
fn command_may_change_current_plan_includes_importplan() {
    for (cmd, sub) in [
        ("!done", None),
        ("!undo", None),
        ("!takeover", None),
        ("!acceptswap", Some("3")),
        ("!swap", Some("accept")),
        ("!plan", Some("skip")),
        ("!plan", Some("assign")),
        ("!plan", Some("unassign")),
        ("!plan", Some("reset")),
        ("!plan", Some("import")),
        ("!groups", Some("disable")),
        ("!groups", Some("enable")),
        ("!member", Some("away")),
    ] {
        assert!(
            command_may_change_current_plan(cmd, sub),
            "{cmd} {sub:?} must trigger a pinned-plan refresh"
        );
    }
    for (cmd, sub) in [
        ("!status", None),
        ("!help", None),
        ("!stats", None),
        ("!plan", None),
        ("!plan", Some("6")),
        ("!swap", Some("@bob:example.org")),
        ("!groups", None),
        ("!groups", Some("2nd floor")),
        ("!member", Some("add")),
        ("", None),
    ] {
        assert!(
            !command_may_change_current_plan(cmd, sub),
            "{cmd} {sub:?} must not trigger a pinned-plan refresh"
        );
    }
}

#[tokio::test]
async fn import_for_the_current_week_changes_the_render_and_is_idempotent_after() {
    let (state, group_id, _first_id, second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();

    let before = {
        let state = ctx.state.lock().await;
        scheduler::build_weekly_plan(&state, cy, cw, &state.cleaning_groups).0
    };

    let args = [
        iso_week_token(cy, cw),
        "2nd Floor".to_owned(),
        "@bob:example.org".to_owned(),
    ];
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let reply = cmd_importplan(&ctx, &admin, &arg_refs)
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("1 added"), "{reply}");

    let after = {
        let state = ctx.state.lock().await;
        scheduler::build_weekly_plan(&state, cy, cw, &state.cleaning_groups).0
    };
    assert_ne!(
        before, after,
        "the render for the current week must change immediately after import"
    );
    assert!(
        after.contains(second_id.as_str()) || after.contains("@bob:example.org"),
        "the newly imported assignee must show up in the current week's render: {after}"
    );

    // Re-running the identical import is a no-op against state (already
    // covered by `import_is_idempotent_across_repeated_identical_runs`);
    // this checks the consequence `refresh_pinned_plan` actually cares
    // about — the re-render must come out byte-identical, which is
    // exactly what makes its `weekly_plan_rendered` cache check skip
    // sending a redundant edit instead of duplicating it.
    let second = cmd_importplan(&ctx, &admin, &arg_refs)
        .await
        .unwrap()
        .unwrap();
    assert!(second.contains("Nothing to do"), "{second}");
    let after_repeat = {
        let state = ctx.state.lock().await;
        scheduler::build_weekly_plan(&state, cy, cw, &state.cleaning_groups).0
    };
    assert_eq!(
        after, after_repeat,
        "repeating the same import must not change the render again"
    );
    let _ = group_id;

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_makes_a_matrix_participant_mentionable_and_a_non_matrix_participant_plain() {
    let (mut state, _group_id, _first_id, _second_id) = rotation_state();
    state.persons.push(Person::new_named("Flo3"));
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();
    let (ny, nw) = add_weeks(cy, cw, 1);

    // Bob (Matrix) covers this week; Flo3 (no Matrix account) covers next week.
    let reply = cmd_importplan(
        &ctx,
        &admin,
        &[
            &iso_week_token(cy, cw),
            "2nd Floor",
            "@bob:example.org",
            ";",
            &iso_week_token(ny, nw),
            "2nd Floor",
            "Flo3",
        ],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.contains("2 added"), "{reply}");

    let state = ctx.state.lock().await;
    let (raw, _mxids) = scheduler::build_weekly_plan(&state, cy, cw, &state.cleaning_groups);
    drop(state);
    assert!(raw.contains("@bob:example.org"), "{raw}");

    let mut names = std::collections::HashMap::new();
    names.insert("@bob:example.org".to_string(), "Bob".to_string());
    let content = format::mentionify_with_names(&raw, &names);
    let mentions = content
        .mentions
        .expect("Bob has a Matrix ID and must produce a real mention");
    assert!(
        mentions
            .user_ids
            .iter()
            .any(|u| u.as_str() == "@bob:example.org"),
        "{mentions:?}"
    );

    let state = ctx.state.lock().await;
    let (raw_next, _mxids) = scheduler::build_weekly_plan(&state, ny, nw, &state.cleaning_groups);
    drop(state);
    assert!(raw_next.contains("Flo3"), "{raw_next}");
    assert!(
        !raw_next.contains('@'),
        "a person without a Matrix ID must never leave an @mxid token in the text: {raw_next}"
    );
    // Must not panic and must not fabricate a mention for a plain name.
    let plain_content = format::mentionify_with_names(&raw_next, &std::collections::HashMap::new());
    assert!(
        plain_content.mentions.is_none() || plain_content.mentions.unwrap().user_ids.is_empty()
    );

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn a_completion_recorded_after_import_still_mentions_the_imported_assignee() {
    let (state, group_id, _first_id, second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();

    // Import freezes Bob in for this week (as `refresh_pinned_plan` would
    // send right after the command, per `command_may_change_current_plan`).
    cmd_importplan(
        &ctx,
        &admin,
        &[&iso_week_token(cy, cw), "2nd Floor", "@bob:example.org"],
    )
    .await
    .unwrap()
    .unwrap();

    // Bob then marks his imported task done — a second, independent state
    // change that also triggers a pinned-plan refresh (`!done` is in
    // `command_may_change_current_plan` too). The re-render for that edit
    // must still carry a real mention for Bob, not silently regress to
    // plain text.
    {
        let mut state = ctx.state.lock().await;
        state
            .apply_event(DomainEvent::CleaningCompleted {
                group_id: group_id.clone(),
                slot_id: None,
                person_id: second_id.clone(),
                responsible_person_ids: vec![second_id.clone()],
                iso_year: cy,
                iso_week: cw,
                shift: 0,
            })
            .unwrap();
    }

    let state = ctx.state.lock().await;
    let (raw, _mxids) = scheduler::build_weekly_plan(&state, cy, cw, &state.cleaning_groups);
    drop(state);
    assert!(raw.contains("✅"), "{raw}");
    assert!(
        raw.contains("@bob:example.org"),
        "the done line must still carry Bob's mxid, not just his display name: {raw}"
    );

    let mut names = std::collections::HashMap::new();
    names.insert("@bob:example.org".to_string(), "Bob".to_string());
    let content = format::mentionify_with_names(&raw, &names);
    let mentions = content
        .mentions
        .expect("the edited/done render must still produce a real mention");
    assert!(
        mentions
            .user_ids
            .iter()
            .any(|u| u.as_str() == "@bob:example.org"),
        "{mentions:?}"
    );

    let _ = tokio::fs::remove_file(path).await;
}

// ── !importplan --replace ────────────────────────────────────────────────

#[tokio::test]
async fn import_replace_overrides_an_already_materialized_round_robin_pick() {
    let (state, group_id, first_id, second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();

    // The normal scheduler already froze Alice in for the current week
    // (a fresh 2-member queue's first-ever draw goes to member_ids[0]).
    let events = {
        let state = ctx.state.lock().await;
        resolver::materialize(&state, 1)
    };
    {
        let mut state = ctx.state.lock().await;
        for e in events {
            state.apply_event(e).unwrap();
        }
    }
    {
        let state = ctx.state.lock().await;
        let a = state
            .slot_assignments
            .iter()
            .find(|a| a.group_id == group_id && a.iso_year == cy && a.iso_week == cw)
            .unwrap();
        assert_eq!(
            a.person_id.as_deref(),
            Some(first_id.as_str()),
            "sanity: round-robin picked Alice"
        );
        assert_eq!(a.source, AssignmentSource::RoundRobin);
    }

    // Without --replace, the paper plan (Bob) conflicts and is rejected.
    let blocked = cmd_importplan(
        &ctx,
        &admin,
        &[&iso_week_token(cy, cw), "2nd Floor", "@bob:example.org"],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(blocked.contains("aborted"), "{blocked}");
    assert!(blocked.contains("--replace"), "{blocked}");

    // With --replace, it overrides the round-robin pick.
    let reply = cmd_importplan(
        &ctx,
        &admin,
        &[
            "--replace",
            &iso_week_token(cy, cw),
            "2nd Floor",
            "@bob:example.org",
        ],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.contains("1 replaced"), "{reply}");
    assert!(reply.contains("0 added"), "{reply}");

    let state = ctx.state.lock().await;
    let a = state
        .slot_assignments
        .iter()
        .find(|a| a.group_id == group_id && a.iso_year == cy && a.iso_week == cw)
        .unwrap();
    assert_eq!(
        a.person_id.as_deref(),
        Some(second_id.as_str()),
        "Bob must now hold the slot"
    );
    assert_eq!(
        a.source,
        AssignmentSource::Import,
        "the replacement must be tagged as an import, not left as round-robin"
    );
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_replace_does_not_touch_the_rotation_queue() {
    let (mut raw_state, group_id, anna_id, bob_id, carla_id) = three_person_state();
    {
        let group = raw_state
            .cleaning_groups
            .iter_mut()
            .find(|g| g.id == group_id)
            .unwrap();
        group.rotation_queue = vec![anna_id.clone(), bob_id.clone(), carla_id.clone()];
    }
    let (ctx, path, admin) = test_context(raw_state);
    let (fy, fw) = current_iso_week();

    // Materialize freezes Anna in via round-robin for the first due week.
    let events = {
        let state = ctx.state.lock().await;
        resolver::materialize(&state, 1)
    };
    {
        let mut state = ctx.state.lock().await;
        for e in events {
            state.apply_event(e).unwrap();
        }
    }

    let queue_before = {
        let state = ctx.state.lock().await;
        state.group_by_id(&group_id).unwrap().rotation_queue.clone()
    };

    // Paper plan actually says Carla covers that week — replace Anna's pick.
    let reply = cmd_importplan(
        &ctx,
        &admin,
        &["--replace", &iso_week_token(fy, fw), "Floor", "Carla"],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.contains("1 replaced"), "{reply}");

    let state = ctx.state.lock().await;
    let queue_after = &state.group_by_id(&group_id).unwrap().rotation_queue;
    assert_eq!(
        &queue_before, queue_after,
        "replacing a materialized pick must not touch rotation_queue"
    );
    let a = state
        .slot_assignments
        .iter()
        .find(|a| a.group_id == group_id && a.iso_year == fy && a.iso_week == fw)
        .unwrap();
    assert_eq!(a.person_id.as_deref(), Some(carla_id.as_str()));
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_replace_is_atomic_one_bad_entry_blocks_the_whole_batch() {
    let (state, group_id, first_id, _second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();

    let events = {
        let state = ctx.state.lock().await;
        resolver::materialize(&state, 1)
    };
    {
        let mut state = ctx.state.lock().await;
        for e in events {
            state.apply_event(e).unwrap();
        }
    }

    // One valid replacement plus one entry naming an unregistered person —
    // the whole batch (including the otherwise-valid replacement) must be
    // rejected, and nothing may be written.
    let reply = cmd_importplan(
        &ctx,
        &admin,
        &[
            "--replace",
            &iso_week_token(cy, cw),
            "2nd Floor",
            "@bob:example.org",
            ";",
            &iso_week_token(cy, cw),
            "2nd Floor",
            "@nobody:example.org",
        ],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.contains("aborted"), "{reply}");

    let state = ctx.state.lock().await;
    let a = state
        .slot_assignments
        .iter()
        .find(|a| a.group_id == group_id && a.iso_year == cy && a.iso_week == cw)
        .unwrap();
    assert_eq!(
        a.person_id.as_deref(),
        Some(first_id.as_str()),
        "the pre-existing pick must survive an aborted --replace batch"
    );
    assert_eq!(a.source, AssignmentSource::RoundRobin);
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn import_replace_is_idempotent_and_refuses_to_touch_a_completed_week() {
    let (state, group_id, first_id, second_id) = rotation_state();
    let (ctx, path, admin) = test_context(state);
    let (cy, cw) = current_iso_week();

    // Alice is already responsible for (and completes) the current week.
    {
        let mut state = ctx.state.lock().await;
        state
            .apply_event(DomainEvent::SlotAssigned {
                group_id: group_id.clone(),
                slot_index: 0,
                iso_year: cy,
                iso_week: cw,
                shift: 0,
                person_id: Some(first_id.clone()),
                source: AssignmentSource::RoundRobin,
                actor_id: None,
                previous_person_id: None,
            })
            .unwrap();
        state
            .apply_event(DomainEvent::CleaningCompleted {
                group_id: group_id.clone(),
                slot_id: None,
                person_id: first_id.clone(),
                responsible_person_ids: vec![first_id.clone()],
                iso_year: cy,
                iso_week: cw,
                shift: 0,
            })
            .unwrap();
    }

    // Even with --replace, an already-completed week cannot be overridden.
    let blocked = cmd_importplan(
        &ctx,
        &admin,
        &[
            "--replace",
            &iso_week_token(cy, cw),
            "2nd Floor",
            "@bob:example.org",
        ],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(blocked.contains("aborted"), "{blocked}");
    assert!(blocked.contains("already completed"), "{blocked}");

    // Re-importing the identical (already-completed) assignment is still
    // a harmless no-op, in either mode.
    let noop = cmd_importplan(
        &ctx,
        &admin,
        &[
            "--replace",
            &iso_week_token(cy, cw),
            "2nd Floor",
            "@alice:example.org",
        ],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(noop.contains("Nothing to do"), "{noop}");

    let state = ctx.state.lock().await;
    let a = state
        .slot_assignments
        .iter()
        .find(|a| a.group_id == group_id && a.iso_year == cy && a.iso_week == cw)
        .unwrap();
    assert_eq!(
        a.person_id.as_deref(),
        Some(first_id.as_str()),
        "a completed week's assignment must be untouched"
    );
    drop(state);
    let _ = second_id;

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn unassign_requires_admin() {
    let (state, ..) = rotation_state();
    let (ctx, path, _admin) = test_context(state);
    let outsider = OwnedUserId::try_from("@outsider:example.org").unwrap();

    let error = cmd_unassign(&ctx, &outsider, &["2nd Floor"])
        .await
        .unwrap_err();
    assert!(error.is::<mxbot_common::admin::NotAdmin>());

    let _ = tokio::fs::remove_file(path).await;
}

// ── Rotation-queue regression tests ──────────────────────────────────────
//
// These exercise the core promise of the queue-based rotation: a leave
// never changes an already-frozen future week that isn't the leaver's, and
// a join never changes the running cycle — it only re-plans from the next
// cycle on, to seat the newcomer there.

#[tokio::test]
async fn join_mid_cycle_keeps_the_running_cycle_and_seats_the_newcomer_in_the_next() {
    let (mut state, group_id, aid, bid, cid) = three_person_state();
    seed_materialized_weeks(&mut state, 5);
    let (y, w) = current_iso_week();
    let week = |i: i64| add_weeks(y, w, i);
    let before: Vec<Option<PersonId>> = (0..5)
        .map(|i| assignee_for(&state, &group_id, week(i).0, week(i).1))
        .collect();
    assert_eq!(
        before,
        vec![
            Some(aid.clone()),
            Some(bid.clone()),
            Some(cid.clone()),
            Some(aid.clone()),
            Some(bid.clone()),
        ],
        "sanity check on the pre-seeded plan"
    );

    let (ctx, path, admin) = test_context_with_horizon(state, 8);
    let reply = cmd_addperson(&ctx, &admin, &["David", "Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("Added David"), "{reply}");

    let state = ctx.state.lock().await;
    let david_id = state.find_person("David").unwrap().id.clone();

    // The running cycle (Anna, Bob, Carla) is untouched.
    for i in 0..3 {
        let (y, w) = week(i);
        assert_eq!(
            assignee_for(&state, &group_id, y, w),
            before[i as usize],
            "week index {i} is in the running cycle and must not change"
        );
    }
    // The next cycle still starts with Anna, and David is in it right
    // away — not at the end of the frozen horizon.
    let at = |i: i64| assignee_for(&state, &group_id, week(i).0, week(i).1);
    assert_eq!(at(3), Some(aid.clone()), "the cycle start stays put");
    assert_eq!(at(4), Some(david_id.clone()), "David's first turn");
    // The horizon isn't extended past what was already frozen…
    assert_eq!(at(5), None, "not frozen yet");
    // …but the rotation continues with the rest of the cycle.
    let preview = |i: i64| preview_assignee_for(&state, &group_id, week(i).0, week(i).1);
    assert_eq!(preview(5), Some(bid));
    assert_eq!(preview(6), Some(cid));
    assert_eq!(preview(7), Some(aid));

    // The reply names the actual first turn and the re-planned range.
    assert!(reply.contains("First turn: "), "{reply}");
    assert!(reply.contains(&format!("(week {})", week(4).1)), "{reply}");
    assert!(
        reply.contains(&format!("weeks from week {} on were re-planned", week(4).1)),
        "{reply}"
    );

    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn multiple_joins_in_sequence_respect_arrival_order() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let group = CleaningGroup::new("Floor");
    let gid = group.id.clone();
    state.cleaning_groups.push(group);
    let (ctx, path, admin) = test_context_with_horizon(state, 8);

    cmd_addperson(&ctx, &admin, &["Anna", "Floor"])
        .await
        .unwrap();
    cmd_addperson(&ctx, &admin, &["Bob", "Floor"])
        .await
        .unwrap();

    let state = ctx.state.lock().await;
    let anna_id = state.find_person("Anna").unwrap().id.clone();
    let bob_id = state.find_person("Bob").unwrap().id.clone();
    let (y, w) = current_iso_week();

    // Anna joined an empty group: the current week stays unassigned
    // rather than handing her a task mid-week.
    assert_eq!(assignee_for(&state, &gid, y, w), None);
    let (y1, w1) = add_weeks(y, w, 1);
    assert_eq!(
        assignee_for(&state, &gid, y1, w1),
        Some(anna_id),
        "Anna joined first, goes first"
    );
    let (y2, w2) = add_weeks(y, w, 2);
    assert_eq!(
        assignee_for(&state, &gid, y2, w2),
        Some(bob_id),
        "Bob joined second, follows Anna"
    );

    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn join_never_skips_an_existing_member_who_has_not_had_a_turn_yet() {
    // Only the current week has ever been materialized (Anna). Bob and
    // Carla are already queued but have zero turns — they still belong to
    // the running cycle, so David must come after both of them.
    let (mut state, group_id, aid, bid, cid) = three_person_state();
    seed_materialized_weeks(&mut state, 1);
    let (ctx, path, admin) = test_context_with_horizon(state, 8);

    let reply = cmd_addperson(&ctx, &admin, &["David", "Floor"])
        .await
        .unwrap()
        .unwrap();

    let state = ctx.state.lock().await;
    let david_id = state.find_person("David").unwrap().id.clone();
    let (y, w) = current_iso_week();
    let at = |i: i64| {
        let (y, w) = add_weeks(y, w, i);
        assignee_for(&state, &group_id, y, w)
    };
    assert_eq!(at(0), Some(aid.clone()), "the active week is untouched");
    assert_eq!(at(1), Some(bid), "must not skip Bob's first turn");
    assert_eq!(at(2), Some(cid), "must not skip Carla's first turn");
    assert_eq!(at(3), Some(aid), "the next cycle starts with Anna again");
    assert_eq!(at(4), Some(david_id), "David is seated in that next cycle");
    // Nothing was frozen past week 0, so no frozen week had to change.
    assert!(
        reply.contains("Already-planned weeks are unchanged."),
        "{reply}"
    );
    assert!(
        reply.contains(&format!("(week {})", add_weeks(y, w, 4).1)),
        "{reply}"
    );

    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

/// Five-member group two weeks into a cycle: Anna and Bob had their turns
/// in the past two weeks, Carla's is the active week, and `weeks_ahead`
/// due weeks from the current one are frozen by plain round robin.
fn mid_cycle_state(weeks_ahead: usize) -> (State, GroupId, Vec<PersonId>) {
    let names = ["Anna", "Bob", "Carla", "Dora", "Emil"];
    let persons: Vec<Person> = names.iter().map(|n| Person::new_named(n)).collect();
    let ids: Vec<PersonId> = persons.iter().map(|p| p.id.clone()).collect();
    let mut group = CleaningGroup::new("Floor");
    let gid = group.id.clone();
    group.member_ids = ids.clone();
    // Anna and Bob just had theirs, so Carla is up next.
    group.rotation_queue = [2, 3, 4, 0, 1].iter().map(|&i| ids[i].clone()).collect();
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    state.persons = persons;
    state.cleaning_groups.push(group);
    let (y, w) = current_iso_week();
    for (back, who) in [(2, 0), (1, 1)] {
        let (py, pw) = add_weeks(y, w, -back);
        state.slot_assignments.push(SlotAssignment {
            group_id: gid.clone(),
            slot_index: 0,
            iso_year: py,
            iso_week: pw,
            shift: 0,
            person_id: Some(ids[who].clone()),
            source: Default::default(),
        });
    }
    seed_materialized_weeks(&mut state, weeks_ahead);
    (state, gid, ids)
}

/// Assignee per week from `from` to `to` (inclusive, offsets from the
/// current week), as display names — readable in assertion messages and
/// comparable across states whose newcomers got different random ids.
fn plan_names(state: &State, group_id: &GroupId, from: i64, to: i64) -> Vec<String> {
    let (y, w) = current_iso_week();
    (from..=to)
        .map(|i| {
            let (y, w) = add_weeks(y, w, i);
            assignee_for(state, group_id, y, w)
                .and_then(|pid| state.person_by_id(&pid))
                .map(|p| p.display_name.clone())
                .unwrap_or_else(|| "-".into())
        })
        .collect()
}

#[tokio::test]
async fn several_joins_in_one_cycle_are_spread_through_the_next_cycle() {
    let (state, group_id, _) = mid_cycle_state(12);
    assert_eq!(
        plan_names(&state, &group_id, -2, 11),
        [
            "Anna", "Bob", "Carla", "Dora", "Emil", // running cycle
            "Anna", "Bob", "Carla", "Dora", "Emil", "Anna", "Bob", "Carla", "Dora",
        ],
        "sanity check on the pre-seeded plan"
    );
    let (ctx, path, admin) = test_context_with_horizon(state, 12);

    cmd_addperson(&ctx, &admin, &["Finn", "Floor"])
        .await
        .unwrap();
    let reply = cmd_addperson(&ctx, &admin, &["Gina", "Floor"])
        .await
        .unwrap()
        .unwrap();

    let state = ctx.state.lock().await;
    let plan = plan_names(&state, &group_id, -2, 11);
    assert_eq!(
        plan,
        [
            "Anna", "Bob", "Carla", "Dora", "Emil", // running cycle, unchanged
            "Anna", "Finn", "Bob", "Carla", "Gina", "Dora", "Emil", // next cycle
            "Anna", "Finn", // and on
        ],
    );
    // Every member — old and new — has exactly one turn in the next cycle.
    let next_cycle: HashSet<&String> = plan[5..12].iter().collect();
    assert_eq!(next_cycle.len(), 7, "{plan:?}");
    // The two newcomers are not seated side by side.
    let finn = plan[5..12].iter().position(|n| n == "Finn").unwrap();
    let gina = plan[5..12].iter().position(|n| n == "Gina").unwrap();
    assert!(gina.abs_diff(finn) > 1, "{plan:?}");
    // The frozen horizon is exactly as deep as before.
    let (y, w) = current_iso_week();
    let (ly, lw) = add_weeks(y, w, 12);
    assert_eq!(assignee_for(&state, &group_id, ly, lw), None);

    assert!(
        reply.contains(&format!("(week {})", add_weeks(y, w, 7).1)),
        "Gina's first turn is reported: {reply}"
    );

    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn joining_one_by_one_plans_the_same_as_spreading_both_at_once() {
    // Finn joins, and Gina only later in the same cycle — the second join
    // re-seats Finn too, so the outcome doesn't depend on that timing.
    let (state, group_id, ids) = mid_cycle_state(12);
    let (ctx, path, admin) = test_context_with_horizon(state, 12);
    cmd_addperson(&ctx, &admin, &["Finn", "Floor"])
        .await
        .unwrap();
    let after_finn = plan_names(&*ctx.state.lock().await, &group_id, -2, 11);
    assert_eq!(
        after_finn[5..12],
        ["Anna", "Finn", "Bob", "Carla", "Dora", "Emil", "Anna"],
        "one newcomer follows the cycle's first turn"
    );
    cmd_addperson(&ctx, &admin, &["Gina", "Floor"])
        .await
        .unwrap();

    let state = ctx.state.lock().await;
    let finn = state.find_person("Finn").unwrap().id.clone();
    let gina = state.find_person("Gina").unwrap().id.clone();
    let group = state.group_by_id(&group_id).unwrap();
    let mut expected = resolver::spread_newcomers(&ids, &[finn, gina]);
    // Two full cycles are frozen through week 11; the queue continues the
    // second one after its first two turns.
    expected.rotate_left(2);
    assert_eq!(group.rotation_queue, expected);

    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn join_replanning_is_deterministic_and_a_restart_changes_nothing() {
    let run = || async {
        let (state, group_id, _) = mid_cycle_state(12);
        let (ctx, path, admin) = test_context_with_horizon(state, 12);
        cmd_addperson(&ctx, &admin, &["Finn", "Floor"])
            .await
            .unwrap();
        cmd_addperson(&ctx, &admin, &["Gina", "Floor"])
            .await
            .unwrap();
        let state = ctx.state.lock().await.clone();
        let _ = tokio::fs::remove_file(path).await;
        (state, group_id)
    };
    let (first, gid1) = run().await;
    let (second, gid2) = run().await;
    assert_eq!(
        plan_names(&first, &gid1, -2, 11),
        plan_names(&second, &gid2, -2, 11),
        "the same situation must always produce the same plan"
    );

    // A restart only runs the additive materialize: same horizon, no change.
    let mut restarted = first.clone();
    let events = resolver::materialize(&restarted, 12);
    assert!(events.is_empty(), "restart must not re-plan: {events:?}");
    for ev in events {
        restarted.apply_event(ev).unwrap();
    }
    assert_eq!(
        serde_json::to_string(&restarted.slot_assignments).unwrap(),
        serde_json::to_string(&first.slot_assignments).unwrap()
    );
}

#[tokio::test]
async fn join_replanning_leaves_history_completions_and_manual_weeks_alone() {
    let (mut state, group_id, ids) = mid_cycle_state(12);
    let (y, w) = current_iso_week();
    // Bob's past week is done, and an admin pinned Emil to week 8.
    let (by, bw) = add_weeks(y, w, -1);
    state
        .apply_event(DomainEvent::CleaningCompleted {
            group_id: group_id.clone(),
            slot_id: None,
            person_id: ids[1].clone(),
            responsible_person_ids: vec![ids[1].clone()],
            iso_year: by,
            iso_week: bw,
            shift: 0,
        })
        .unwrap();
    let (my, mw) = add_weeks(y, w, 8);
    state
        .apply_event(DomainEvent::SlotAssigned {
            group_id: group_id.clone(),
            slot_index: 0,
            iso_year: my,
            iso_week: mw,
            shift: 0,
            person_id: Some(ids[4].clone()),
            source: AssignmentSource::Assign,
            actor_id: Some("@admin:example.org".into()),
            previous_person_id: Some(ids[3].clone()),
        })
        .unwrap();
    let history_before: Vec<SlotAssignment> = state
        .slot_assignments
        .iter()
        .filter(|a| (a.iso_year, a.iso_week) <= (y, w))
        .cloned()
        .collect();
    let completions_before = serde_json::to_string(&state.completions).unwrap();
    let (ctx, path, admin) = test_context_with_horizon(state, 12);

    cmd_addperson(&ctx, &admin, &["Finn", "Floor"])
        .await
        .unwrap();

    let state = ctx.state.lock().await;
    for a in &history_before {
        assert!(
            state.slot_assignments.contains(a),
            "past/current assignment changed: {a:?}"
        );
    }
    assert_eq!(
        serde_json::to_string(&state.completions).unwrap(),
        completions_before
    );
    let pinned = state
        .slot_assignments
        .iter()
        .find(|a| a.group_id == group_id && (a.iso_year, a.iso_week) == (my, mw))
        .unwrap();
    assert_eq!(pinned.person_id.as_ref(), Some(&ids[4]), "admin pin kept");
    assert_eq!(pinned.source, AssignmentSource::Assign);
    assert_eq!(
        plan_names(&state, &group_id, 3, 4),
        ["Anna", "Finn"],
        "Finn still joins the next cycle"
    );

    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn full_lifecycle_survives_a_complete_event_replay() {
    // Build the group and its members through real events (not the
    // struct-literal test helpers) so the event log is actually
    // complete, then plan several weeks, join, leave, and replay the
    // whole log from an empty State — exactly what a restart-from-log
    // would do.
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let group_id = Uuid::new_v4().to_string();
    state
        .apply_event(DomainEvent::GroupCreated {
            group_id: group_id.clone(),
            name: "Floor".into(),
        })
        .unwrap();
    for name in ["Anna", "Bob", "Carla"] {
        let pid = Uuid::new_v4().to_string();
        state
            .apply_event(DomainEvent::PersonCreated {
                person_id: pid.clone(),
                display_name: name.into(),
                matrix_id: None,
            })
            .unwrap();
        state
            .apply_event(DomainEvent::PersonJoinedGroup {
                person_id: pid,
                group_id: group_id.clone(),
            })
            .unwrap();
    }
    seed_materialized_weeks(&mut state, 3);

    let (ctx, path, admin) = test_context_with_horizon(state, 6);
    cmd_addperson(&ctx, &admin, &["David", "Floor"])
        .await
        .unwrap();
    cmd_removeperson(&ctx, &admin, &["Bob", "Floor"])
        .await
        .unwrap();

    let before = ctx.state.lock().await.clone();
    assert!(!before.event_log.is_empty());

    let mut replayed = State::default();
    for logged in &before.event_log {
        replayed.apply_event(logged.event.clone()).unwrap();
    }

    assert_eq!(
        serde_json::to_string(&before.cleaning_groups).unwrap(),
        serde_json::to_string(&replayed.cleaning_groups).unwrap(),
        "member_ids and rotation_queue must be identical after full replay"
    );
    assert_eq!(
        serde_json::to_string(&before.slot_assignments).unwrap(),
        serde_json::to_string(&replayed.slot_assignments).unwrap(),
        "slot_assignments must be identical after full replay"
    );

    // And a plain reload-from-disk (the ordinary restart path) must
    // agree with both.
    let reloaded = State::load(&path).await.unwrap();
    assert_eq!(
        serde_json::to_string(&before.cleaning_groups).unwrap(),
        serde_json::to_string(&reloaded.cleaning_groups).unwrap()
    );

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn removeperson_without_any_future_assignment_just_leaves() {
    let (mut state, group_id, aid, _bid, cid) = three_person_state();
    seed_materialized_weeks(&mut state, 1);
    let (ctx, path, admin) = test_context(state);

    let reply = cmd_removeperson(&ctx, &admin, &["Carla", "Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("Removed Carla"), "{reply}");

    let state = ctx.state.lock().await;
    assert!(!state
        .group_by_id(&group_id)
        .unwrap()
        .member_ids
        .contains(&cid));
    let (y, w) = current_iso_week();
    assert_eq!(
        assignee_for(&state, &group_id, y, w),
        Some(aid),
        "untouched — not Carla's week"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn leavefloor_is_blocked_while_the_current_assignment_is_open() {
    let (mut state, group_id, first_id, _second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id.clone()),
        source: Default::default(),
    });
    let (ctx, path, _admin) = test_context(state);
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();

    let reply = cmd_leavefloor(&ctx, &alice, &["2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("cannot leave"), "{reply}");

    let state = ctx.state.lock().await;
    assert!(
        state
            .group_by_id(&group_id)
            .unwrap()
            .member_ids
            .contains(&first_id),
        "must not have left"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn removeperson_with_future_assignments_moves_everyone_after_them_up() {
    let (mut state, group_id, ..) = three_person_state();
    seed_materialized_weeks(&mut state, 5);
    let (ctx, path, admin) = test_context_with_horizon(state, 8);

    let reply = cmd_removeperson(&ctx, &admin, &["Carla", "Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("Removed Carla"), "{reply}");

    let state = ctx.state.lock().await;
    // Weeks before Carla's first one stay; from there the two remaining
    // members alternate instead of Anna inheriting week 2 right before
    // her own week 3.
    assert_eq!(
        plan_names(&state, &group_id, 0, 4),
        ["Anna", "Bob", "Anna", "Bob", "Anna"]
    );
    assert!(
        reply.contains(&format!(
            "from week {} on",
            add_weeks(current_iso_week().0, current_iso_week().1, 2).1
        )),
        "{reply}"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

/// Four-member group (Anna, Bob, Carla, Dora) with `weeks` weeks frozen
/// from the current one: Anna, Bob, Carla, Dora, Anna, …
fn four_person_state(weeks: usize) -> (State, GroupId) {
    let (mut state, gid, ..) = three_person_state();
    let dora = Person::new_named("Dora");
    state.cleaning_groups[0].member_ids.push(dora.id.clone());
    state.persons.push(dora);
    seed_materialized_weeks(&mut state, weeks);
    (state, gid)
}

/// Invariants every plan must keep: one assignment per (group, slot, turn),
/// and no future plain-rotation turn for someone who isn't a member.
fn assert_plan_invariants(state: &State) {
    let mut keys = HashSet::new();
    for a in &state.slot_assignments {
        assert!(
            keys.insert((
                a.group_id.clone(),
                a.slot_index,
                a.iso_year,
                a.iso_week,
                a.shift
            )),
            "duplicate assignment: {a:?}"
        );
    }
    let current = current_iso_week();
    for a in &state.slot_assignments {
        if (a.iso_year, a.iso_week) > current && a.source == AssignmentSource::RoundRobin {
            if let Some(pid) = &a.person_id {
                let group = state.group_by_id(&a.group_id).unwrap();
                assert!(
                    group.member_ids.contains(pid),
                    "non-member {pid} still in future rotation: {a:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn leaving_before_their_turn_moves_the_rest_up_without_back_to_back_turns() {
    let (state, gid) = four_person_state(8);
    assert_eq!(
        plan_names(&state, &gid, 0, 7),
        ["Anna", "Bob", "Carla", "Dora", "Anna", "Bob", "Carla", "Dora"]
    );
    let (ctx, path, admin) = test_context_with_horizon(state, 8);
    let reply = cmd_removeperson(&ctx, &admin, &["Bob", "Floor"])
        .await
        .unwrap()
        .unwrap();

    let state = ctx.state.lock().await;
    let plan = plan_names(&state, &gid, 0, 7);
    assert_eq!(
        plan,
        ["Anna", "Carla", "Dora", "Anna", "Carla", "Dora", "Anna", "Carla"],
        "the current week stays; everyone after Bob moves up one turn"
    );
    for pair in plan.windows(2) {
        assert_ne!(pair[0], pair[1], "back-to-back turns: {plan:?}");
    }
    assert!(
        reply.contains("Their 2 upcoming turn(s) were handed on"),
        "{reply}"
    );
    assert_plan_invariants(&state);
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn leaving_after_their_turn_keeps_it_and_the_rest_of_the_round() {
    let (mut state, gid) = four_person_state(8);
    // Anna's turn is this week, and she's done it.
    let anna = state.find_person("Anna").unwrap().id.clone();
    let (y, w) = current_iso_week();
    state
        .apply_event(DomainEvent::CleaningCompleted {
            group_id: gid.clone(),
            slot_id: None,
            person_id: anna.clone(),
            responsible_person_ids: vec![anna.clone()],
            iso_year: y,
            iso_week: w,
            shift: 0,
        })
        .unwrap();
    let completions = serde_json::to_string(&state.completions).unwrap();
    let (ctx, path, admin) = test_context_with_horizon(state, 8);
    cmd_removeperson(&ctx, &admin, &["Anna", "Floor"])
        .await
        .unwrap();

    let state = ctx.state.lock().await;
    assert_eq!(
        plan_names(&state, &gid, 0, 7),
        ["Anna", "Bob", "Carla", "Dora", "Bob", "Carla", "Dora", "Bob"],
        "her done week and everything before her next turn stay"
    );
    assert_eq!(
        serde_json::to_string(&state.completions).unwrap(),
        completions
    );
    assert_plan_invariants(&state);
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn leaving_reports_their_pinned_weeks_and_keeps_everyone_elses() {
    let (mut state, gid) = four_person_state(8);
    let id = |state: &State, n: &str| state.find_person(n).unwrap().id.clone();
    let (bob, dora) = (id(&state, "Bob"), id(&state, "Dora"));
    let (y, w) = current_iso_week();
    let pin = |state: &mut State, offset: i64, who: &PersonId, source: AssignmentSource| {
        let (py, pw) = add_weeks(y, w, offset);
        state
            .apply_event(DomainEvent::SlotAssigned {
                group_id: gid.clone(),
                slot_index: 0,
                iso_year: py,
                iso_week: pw,
                shift: 0,
                person_id: Some(who.clone()),
                source,
                actor_id: None,
                previous_person_id: None,
            })
            .unwrap();
    };
    // An admin put Bob on week 6; Dora's week 3 came from the paper plan.
    pin(&mut state, 6, &bob, AssignmentSource::Assign);
    pin(&mut state, 3, &dora, AssignmentSource::Import);
    let (ctx, path, admin) = test_context_with_horizon(state, 8);
    let reply = cmd_removeperson(&ctx, &admin, &["Bob", "Floor"])
        .await
        .unwrap()
        .unwrap();

    let state = ctx.state.lock().await;
    let (py, pw) = add_weeks(y, w, 3);
    let dora_pin = state
        .slot_assignments
        .iter()
        .find(|a| a.group_id == gid && (a.iso_year, a.iso_week) == (py, pw))
        .unwrap();
    assert_eq!(
        dora_pin.person_id.as_ref(),
        Some(&dora),
        "others' pins stay"
    );
    assert_eq!(dora_pin.source, AssignmentSource::Import);
    assert!(
        !state
            .slot_assignments
            .iter()
            .any(|a| a.person_id.as_ref() == Some(&bob) && (a.iso_year, a.iso_week) > (y, w)),
        "Bob holds nothing in the future"
    );
    assert!(
        reply.contains(&format!(
            "⚠️ Their assigned week {} (Floor) is now",
            add_weeks(y, w, 6).1
        )),
        "his pinned week must be reported, not silently dropped: {reply}"
    );
    assert_plan_invariants(&state);
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn joining_and_leaving_again_restores_the_original_plan() {
    let (state, group_id, _) = mid_cycle_state(12);
    let original = plan_names(&state, &group_id, -2, 11);
    let (ctx, path, admin) = test_context_with_horizon(state, 12);
    cmd_addperson(&ctx, &admin, &["Finn", "Floor"])
        .await
        .unwrap();
    cmd_removeperson(&ctx, &admin, &["Finn", "Floor"])
        .await
        .unwrap();
    let state = ctx.state.lock().await;
    assert_eq!(plan_names(&state, &group_id, -2, 11), original);
    assert_plan_invariants(&state);
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn a_join_and_a_leave_plan_the_same_in_either_order() {
    let run = |join_first: bool| async move {
        let (state, group_id, _) = mid_cycle_state(12);
        let (ctx, path, admin) = test_context_with_horizon(state, 12);
        for step in if join_first {
            [true, false]
        } else {
            [false, true]
        } {
            if step {
                cmd_addperson(&ctx, &admin, &["Finn", "Floor"])
                    .await
                    .unwrap();
            } else {
                cmd_removeperson(&ctx, &admin, &["Dora", "Floor"])
                    .await
                    .unwrap();
            }
        }
        let state = ctx.state.lock().await.clone();
        assert_plan_invariants(&state);
        let _ = tokio::fs::remove_file(path).await;
        plan_names(&state, &group_id, -2, 11)
    };
    assert_eq!(run(true).await, run(false).await);
}

#[tokio::test]
async fn plan_reset_names_the_pinned_weeks_it_discards() {
    let (mut state, gid) = four_person_state(8);
    let dora = state.find_person("Dora").unwrap().id.clone();
    let (y, w) = current_iso_week();
    let (py, pw) = add_weeks(y, w, 2);
    state
        .apply_event(DomainEvent::SlotAssigned {
            group_id: gid.clone(),
            slot_index: 0,
            iso_year: py,
            iso_week: pw,
            shift: 0,
            person_id: Some(dora),
            source: AssignmentSource::Import,
            actor_id: None,
            previous_person_id: None,
        })
        .unwrap();
    let (ctx, path, admin) = test_context_with_horizon(state, 8);
    let reply = cmd_resetplan(&ctx, &admin, &["Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(
        reply.contains(&format!(
            "Discarded 1 pinned week(s), now plain rotation: {pw} (imported)"
        )),
        "{reply}"
    );
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn the_scheduler_keeps_the_frozen_plan_rolling_without_touching_it() {
    let (state, gid) = four_person_state(3);
    let before = state.slot_assignments.clone();
    let (ctx, path, _) = test_context_with_horizon(state, 6);
    crate::scheduler::roll_planning_horizon(&ctx).await.unwrap();
    let state = ctx.state.lock().await;
    for a in &before {
        assert!(
            state.slot_assignments.contains(a),
            "frozen turn changed: {a:?}"
        );
    }
    assert_eq!(
        plan_names(&state, &gid, 0, 6),
        ["Anna", "Bob", "Carla", "Dora", "Anna", "Bob", "-"],
        "filled up to the configured six weeks, no further"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn myplan_lists_the_senders_next_turns_with_their_kind() {
    let anna = Person::new_matrix("@anna:example.org");
    let bob = Person::new_named("Bob");
    let mut group = CleaningGroup::new("Floor");
    let gid = group.id.clone();
    group.member_ids = vec![anna.id.clone(), bob.id.clone()];
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let anna_id = anna.id.clone();
    state.persons = vec![anna, bob];
    state.cleaning_groups.push(group);
    seed_materialized_weeks(&mut state, 4); // Anna, Bob, Anna, Bob
    let (y, w) = current_iso_week();
    let (py, pw) = add_weeks(y, w, 3);
    state
        .apply_event(DomainEvent::SlotAssigned {
            group_id: gid.clone(),
            slot_index: 0,
            iso_year: py,
            iso_week: pw,
            shift: 0,
            person_id: Some(anna_id),
            source: AssignmentSource::Import,
            actor_id: None,
            previous_person_id: None,
        })
        .unwrap();
    let (ctx, path, _) = test_context_with_horizon(state, 4);
    let anna_mxid = OwnedUserId::try_from("@anna:example.org").unwrap();

    let reply = cmd_myplan(&ctx, &anna_mxid, &["4"]).await.unwrap().unwrap();
    let lines: Vec<&str> = reply.lines().collect();
    assert!(lines[0].contains("Upcoming turns for anna"), "{reply}");
    let week = |i: i64| add_weeks(y, w, i).1;
    assert!(
        lines[1].starts_with(&format!("• week {} ", week(0))),
        "{reply}"
    );
    assert!(
        lines[1].contains("now"),
        "the running turn is flagged: {reply}"
    );
    assert!(
        lines[2].starts_with(&format!("• week {} ", week(2))),
        "{reply}"
    );
    assert!(
        lines[3].starts_with(&format!("• week {} ", week(3))) && lines[3].contains("(imported)"),
        "{reply}"
    );
    assert!(
        lines[4].starts_with(&format!("• week {} ", week(4))) && lines[4].contains("(tentative)"),
        "past the frozen plan: {reply}"
    );
    assert_eq!(
        lines.len(),
        6,
        "four turns plus the tentative note: {reply}"
    );

    // Anyone can look someone else up, just like !next.
    let bob_plan = cmd_myplan(&ctx, &anna_mxid, &["Bob", "1"])
        .await
        .unwrap()
        .unwrap();
    assert!(bob_plan.contains("Upcoming turns for Bob"), "{bob_plan}");
    assert!(
        bob_plan.contains(&format!("• week {} ", week(1))),
        "{bob_plan}"
    );

    let stranger = OwnedUserId::try_from("@new:example.org").unwrap();
    let none = cmd_myplan(&ctx, &stranger, &[]).await.unwrap().unwrap();
    assert!(none.contains("not registered"), "{none}");
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn myplan_without_upcoming_turns_says_so() {
    let (mut state, ..) = rotation_state();
    state.created_at = Some(Utc::now());
    state.persons.push(Person::new_named("Zoe")); // in no group
    let (ctx, path, admin) = test_context(state);
    let reply = cmd_myplan(&ctx, &admin, &["Zoe"]).await.unwrap().unwrap();
    assert_eq!(reply, "📅 No upcoming turns for Zoe in the next two years.");
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn join_then_leave_leaves_no_trace_and_preserves_original_weeks() {
    let (mut state, group_id, aid, bid, _cid) = three_person_state();
    seed_materialized_weeks(&mut state, 2);
    let (ctx, path, admin) = test_context_with_horizon(state, 8);

    cmd_addperson(&ctx, &admin, &["David", "Floor"])
        .await
        .unwrap();
    let david_id = ctx
        .state
        .lock()
        .await
        .find_person("David")
        .unwrap()
        .id
        .clone();
    cmd_removeperson(&ctx, &admin, &["David", "Floor"])
        .await
        .unwrap();

    let state = ctx.state.lock().await;
    let group = state.group_by_id(&group_id).unwrap();
    assert!(!group.member_ids.contains(&david_id));
    assert!(!group.rotation_queue.contains(&david_id));
    assert!(
        !state
            .slot_assignments
            .iter()
            .any(|a| a.person_id.as_deref() == Some(david_id.as_str())),
        "no assignment should still reference David"
    );

    let (y, w) = current_iso_week();
    assert_eq!(assignee_for(&state, &group_id, y, w), Some(aid));
    let (y1, w1) = add_weeks(y, w, 1);
    assert_eq!(assignee_for(&state, &group_id, y1, w1), Some(bid));
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

// ── !linkmatrix ───────────────────────────────────────────────────────────
//
// `apply_linkmatrix` is the Room-independent core of `cmd_linkmatrix` (see
// its doc comment) — exercised directly here since this codebase has no
// Matrix `Room` test double to drive `cmd_linkmatrix` itself through.

#[tokio::test]
async fn linkmatrix_repairs_a_corrupted_matrix_id_and_merges_the_unused_stub() {
    // Reproduces the real "papageientaucher" incident *exactly*, storage
    // order included: the unused, valid-mxid stub is stored BEFORE the
    // real 3+4 Floor participant (whose matrix_id is malformed) in
    // `state.persons` — this is what let `find_person`'s
    // first-match-by-name lookup return the stub and refuse with
    // "already has a Matrix account linked" before ever reaching the
    // real participant's invalid matrix_id.
    let stub = Person::new_matrix("@papageientaucher:matrix.org"); // valid, unused
    let stub_id = stub.id.clone();
    let real = Person::new_matrix("papageientaucher"); // matrix_id = "papageientaucher" (invalid)
    let real_id = real.id.clone();

    let mut group = CleaningGroup::new("3+4 Floor");
    let group_id = group.id.clone();
    group.member_ids = vec![real_id.clone()];
    group.rotation_queue = vec![real_id.clone()];

    let mut state = State::default();
    state.persons = vec![stub, real]; // stub first, exactly as in production
    state.cleaning_groups.push(group);
    let (year, week) = current_iso_week();
    let (fy, fw) = add_weeks(year, week, 1);
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: fy,
        iso_week: fw,
        shift: 0,
        person_id: Some(real_id.clone()),
        source: Default::default(),
    });
    state.completions.push(crate::state::Completion {
        group_id: group_id.clone(),
        slot_id: None,
        completed_by_id: real_id.clone(),
        responsible_person_ids: vec![real_id.clone()],
        iso_year: year,
        iso_week: week,
        shift: 0,
        completed_at: chrono::Utc::now(),
        skipped: false,
    });
    let (ctx, path, _admin) = test_context(state);

    let reply = apply_linkmatrix(
        &ctx,
        "papageientaucher",
        "@papageientaucher:matrix.org",
        Some("Bela"),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.contains("repaired"), "{reply}");

    let state = ctx.state.lock().await;

    // The real participant's identity, membership, history and queue
    // position are all untouched — only matrix_id (and, since we passed
    // one, display_name) changed.
    let real_now = state
        .person_by_id(&real_id)
        .expect("real participant's PersonId must survive");
    assert_eq!(
        real_now.matrix_id.as_deref(),
        Some("@papageientaucher:matrix.org")
    );
    assert_eq!(real_now.display_name, "Bela");

    let group_now = state.group_by_id(&group_id).unwrap();
    assert_eq!(
        group_now.member_ids,
        vec![real_id.clone()],
        "group membership must be unchanged"
    );
    assert_eq!(
        group_now.rotation_queue,
        vec![real_id.clone()],
        "rotation position must be unchanged"
    );

    assert!(
        state.slot_assignments.iter().any(|a| a.group_id == group_id
            && a.iso_year == fy
            && a.iso_week == fw
            && a.person_id.as_deref() == Some(real_id.as_str())),
        "the existing assignment must still reference the same PersonId"
    );
    assert!(
        state
            .completions
            .iter()
            .any(|c| c.completed_by_id == real_id),
        "completion history must still reference the same PersonId"
    );

    // The unused stub was merged away — no duplicate participant remains.
    assert!(
        state.person_by_id(&stub_id).is_none(),
        "the unused stub must be removed, not kept alongside"
    );
    assert_eq!(
        state.persons.len(),
        1,
        "exactly one papageientaucher record must remain"
    );
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn linkmatrix_refuses_to_overwrite_an_already_valid_matrix_id() {
    let mut state = State::default();
    state
        .persons
        .push(Person::new_matrix("@already:example.org"));

    let (ctx, path, _admin) = test_context(state);
    let reply = apply_linkmatrix(&ctx, "already", "@new:example.org", None)
        .await
        .unwrap()
        .unwrap();
    assert!(
        reply.contains("already has a Matrix account linked"),
        "{reply}"
    );

    let state = ctx.state.lock().await;
    assert_eq!(
        state.find_person("already").unwrap().matrix_id.as_deref(),
        Some("@already:example.org"),
        "a valid existing matrix_id must never be overwritten by another"
    );
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn linkmatrix_refuses_to_guess_between_two_real_people_with_the_same_name() {
    // Two distinct, unrelated participants happen to share a display
    // name; both currently have no Matrix ID and both have real
    // activity. Auto-picking either would silently attach the wrong
    // person's history to this mxid — must refuse instead.
    let dup_a = Person::new_named("Dup");
    let dup_a_id = dup_a.id.clone();
    let dup_b = Person::new_named("Dup");
    let dup_b_id = dup_b.id.clone();

    let mut group = CleaningGroup::new("Floor");
    let group_id = group.id.clone();
    group.member_ids = vec![dup_a_id.clone(), dup_b_id.clone()];

    let mut state = State::default();
    state.persons = vec![dup_a, dup_b];
    state.cleaning_groups.push(group);
    let (year, week) = current_iso_week();
    state.completions.push(crate::state::Completion {
        group_id: group_id.clone(),
        slot_id: None,
        completed_by_id: dup_b_id.clone(),
        responsible_person_ids: vec![dup_b_id.clone()],
        iso_year: year,
        iso_week: week,
        shift: 0,
        completed_at: chrono::Utc::now(),
        skipped: false,
    });
    let (ctx, path, _admin) = test_context(state);

    let reply = apply_linkmatrix(&ctx, "Dup", "@new:example.org", None)
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("refusing to guess"), "{reply}");
    assert!(
        reply.contains(&dup_a_id) && reply.contains(&dup_b_id),
        "{reply}"
    );

    let state = ctx.state.lock().await;
    assert!(state.person_by_id(&dup_a_id).unwrap().matrix_id.is_none());
    assert!(state.person_by_id(&dup_b_id).unwrap().matrix_id.is_none());
    drop(state);

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn skip_does_not_touch_rotation_queue_or_future_assignments() {
    let (mut state, group_id, ..) = three_person_state();
    seed_materialized_weeks(&mut state, 3);
    let queue_before = state.group_by_id(&group_id).unwrap().rotation_queue.clone();
    let assignments_before = state.slot_assignments.clone();

    let (ctx, path, admin) = test_context(state);
    let reply = cmd_skip(&ctx, &admin, &["Floor"]).await.unwrap().unwrap();
    assert!(reply.contains("Skipped"), "{reply}");

    let state = ctx.state.lock().await;
    assert_eq!(
        state.group_by_id(&group_id).unwrap().rotation_queue,
        queue_before
    );
    assert_eq!(state.slot_assignments.len(), assignments_before.len());
    for a in &assignments_before {
        assert!(
            state.slot_assignments.iter().any(|b| {
                b.group_id == a.group_id
                    && b.slot_index == a.slot_index
                    && b.iso_year == a.iso_year
                    && b.iso_week == a.iso_week
                    && b.person_id == a.person_id
            }),
            "assignment for week {} preserved",
            a.iso_week
        );
    }
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn done_does_not_touch_rotation_queue_or_future_assignments() {
    let (mut state, group_id, ..) = rotation_state();
    seed_materialized_weeks(&mut state, 3);
    let queue_before = state.group_by_id(&group_id).unwrap().rotation_queue.clone();
    let (cur_y, cur_w) = current_iso_week();
    let future_before: Vec<_> = state
        .slot_assignments
        .iter()
        .filter(|a| a.iso_year > cur_y || (a.iso_year == cur_y && a.iso_week > cur_w))
        .cloned()
        .collect();

    let (ctx, path, _admin) = test_context(state);
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();
    let reply = cmd_done(&ctx, &alice, &["2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("Cleaned"), "{reply}");

    let state = ctx.state.lock().await;
    assert_eq!(
        state.group_by_id(&group_id).unwrap().rotation_queue,
        queue_before
    );
    let future_after: Vec<_> = state
        .slot_assignments
        .iter()
        .filter(|a| a.iso_year > cur_y || (a.iso_year == cur_y && a.iso_week > cur_w))
        .cloned()
        .collect();
    assert_eq!(future_after.len(), future_before.len());
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn manual_assign_and_unassign_do_not_touch_rotation_queue() {
    let (mut state, group_id, _aid, _bid, cid) = three_person_state();
    seed_materialized_weeks(&mut state, 2);
    let queue_before = state.group_by_id(&group_id).unwrap().rotation_queue.clone();
    let (ctx, path, admin) = test_context(state);

    cmd_assign(&ctx, &admin, &["Floor", "Carla"]).await.unwrap();
    {
        let state = ctx.state.lock().await;
        assert_eq!(
            state.group_by_id(&group_id).unwrap().rotation_queue,
            queue_before
        );
        let (y, w) = current_iso_week();
        assert_eq!(assignee_for(&state, &group_id, y, w), Some(cid.clone()));
    }

    cmd_unassign(&ctx, &admin, &["Floor"]).await.unwrap();
    let state = ctx.state.lock().await;
    assert_eq!(
        state.group_by_id(&group_id).unwrap().rotation_queue,
        queue_before
    );
    let (y, w) = current_iso_week();
    assert_eq!(assignee_for(&state, &group_id, y, w), None);
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn restart_replay_reproduces_identical_rotation() {
    let (mut state, group_id, ..) = three_person_state();
    seed_materialized_weeks(&mut state, 2);
    let (ctx, path, admin) = test_context_with_horizon(state, 8);
    cmd_addperson(&ctx, &admin, &["David", "Floor"])
        .await
        .unwrap();

    let before = ctx.state.lock().await.clone();
    // Simulate a restart: reload straight from the saved JSON.
    let after = State::load(&path).await.unwrap();

    assert_eq!(
        serde_json::to_string(&before.cleaning_groups).unwrap(),
        serde_json::to_string(&after.cleaning_groups).unwrap(),
        "member_ids and rotation_queue must survive a reload byte-for-byte"
    );
    assert_eq!(
        serde_json::to_string(&before.slot_assignments).unwrap(),
        serde_json::to_string(&after.slot_assignments).unwrap()
    );

    // Re-running materialize against the reloaded state, up to exactly
    // how far it's already frozen, must be a no-op — no logic may depend
    // on events only ever having existed in RAM.
    let horizon = group_horizon_weeks_ahead(&after, &group_id);
    assert!(horizon > 0, "the join must have frozen at least one week");
    let replay_events = resolver::materialize(&after, horizon);
    assert!(
        replay_events.is_empty(),
        "materialize must be a no-op on an already-materialized, reloaded state"
    );

    let _ = tokio::fs::remove_file(path).await;
}

// ── !takeover: current-week handoff ──────────────────────────────────────

#[tokio::test]
async fn takeover_reassigns_the_running_week_and_the_original_loses_it() {
    let (mut state, group_id, first_id, second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id.clone()),
        source: Default::default(),
    });
    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    let reply = cmd_takeover(&ctx, &bob, &["2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("took over"), "{reply}");
    assert!(
        reply.contains("Alice") || reply.contains("alice"),
        "should note who it came from: {reply}"
    );

    let state = ctx.state.lock().await;
    assert_eq!(
        assignee_for(&state, &group_id, year, week),
        Some(second_id.clone()),
        "Bob is now the sole responsible person"
    );
    assert_ne!(
        assignee_for(&state, &group_id, year, week),
        Some(first_id),
        "Alice must no longer be responsible"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn takeover_does_not_touch_rotation_queue_or_future_assignments() {
    let (mut state, group_id, first_id, _second_id) = three_person_state_matrix();
    seed_materialized_weeks(&mut state, 4);
    let queue_before = state.group_by_id(&group_id).unwrap().rotation_queue.clone();
    let (y, w) = current_iso_week();
    let future_before: Vec<_> = (1..4)
        .map(|i| {
            let (fy, fw) = add_weeks(y, w, i);
            (fy, fw, assignee_for(&state, &group_id, fy, fw))
        })
        .collect();

    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();
    cmd_takeover(&ctx, &bob, &["Floor"]).await.unwrap().unwrap();

    let state = ctx.state.lock().await;
    assert_eq!(
        state.group_by_id(&group_id).unwrap().rotation_queue,
        queue_before,
        "a takeover must never pop or reorder the rotation queue"
    );
    for (fy, fw, before) in future_before {
        assert_eq!(
            assignee_for(&state, &group_id, fy, fw),
            before,
            "future week {fw} must be untouched"
        );
    }
    // Only the current week actually changed.
    assert_eq!(
        assignee_for(&state, &group_id, y, w),
        Some(state.find_person("Bob").unwrap().id.clone())
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
    let _ = first_id;
}

#[tokio::test]
async fn new_assignee_can_mark_done_and_it_persists_across_restart() {
    let (mut state, group_id, first_id, second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id),
        source: Default::default(),
    });
    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    cmd_takeover(&ctx, &bob, &["2nd Floor"]).await.unwrap();
    let done_reply = cmd_done(&ctx, &bob, &["2nd Floor"]).await.unwrap().unwrap();
    assert!(done_reply.contains("Cleaned"), "{done_reply}");

    {
        let state = ctx.state.lock().await;
        assert!(state.is_completed(&group_id, year, week));
        let completion = state
            .completions
            .iter()
            .find(|c| c.group_id == group_id && c.iso_year == year && c.iso_week == week)
            .unwrap();
        assert_eq!(completion.completed_by_id, second_id);
        assert_eq!(
            completion.responsible_person_ids,
            vec![second_id.clone()],
            "credit must go to Bob, the current assignee, not the original round-robin pick"
        );
    }

    // Restart: reload from disk, state must agree exactly.
    let reloaded = State::load(&path).await.unwrap();
    assert!(reloaded.is_completed(&group_id, year, week));
    assert_eq!(
        assignee_for(&reloaded, &group_id, year, week),
        Some(second_id),
        "Bob remains the assignee after a restart"
    );

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn a_member_can_still_mark_done_but_credit_goes_to_the_takeover_assignee() {
    // Alice is still a group member after Bob's takeover, so she can
    // still press done as a convenience — but the recorded responsible
    // person must be Bob, the current assignee, not Alice.
    let (mut state, group_id, first_id, second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id.clone()),
        source: Default::default(),
    });
    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();

    cmd_takeover(&ctx, &bob, &["2nd Floor"]).await.unwrap();
    let reply = cmd_done(&ctx, &alice, &["2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("Cleaned"), "{reply}");

    let state = ctx.state.lock().await;
    let completion = state
        .completions
        .iter()
        .find(|c| c.group_id == group_id && c.iso_year == year && c.iso_week == week)
        .unwrap();
    assert_eq!(
        completion.responsible_person_ids,
        vec![second_id],
        "credit belongs to Bob, not Alice"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
    let _ = first_id;
}

#[tokio::test]
async fn double_takeover_leaves_only_the_last_person_responsible() {
    let (mut state, group_id, aid, ..) = three_person_state();
    seed_materialized_weeks(&mut state, 1);
    let (ctx, path, admin) = test_context(state);

    cmd_assign(&ctx, &admin, &["Floor", "Bob"]).await.unwrap();
    cmd_assign(&ctx, &admin, &["Floor", "Carla"]).await.unwrap();

    let state = ctx.state.lock().await;
    let (y, w) = current_iso_week();
    let carla_id = state.find_person("Carla").unwrap().id.clone();
    let bob_id = state.find_person("Bob").unwrap().id.clone();
    assert_eq!(
        assignee_for(&state, &group_id, y, w),
        Some(carla_id),
        "only Carla is responsible"
    );
    assert_ne!(assignee_for(&state, &group_id, y, w), Some(bob_id));
    assert_ne!(assignee_for(&state, &group_id, y, w), Some(aid));
    // Exactly one SlotAssignment record exists for this (group, week) —
    // never two "responsible" people at once.
    assert_eq!(
        state
            .slot_assignments
            .iter()
            .filter(|a| a.group_id == group_id && a.iso_year == y && a.iso_week == w)
            .count(),
        1
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn takeover_of_an_already_completed_week_is_rejected() {
    let (mut state, group_id, first_id, _second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id.clone()),
        source: Default::default(),
    });
    let (ctx, path, _admin) = test_context(state);
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    cmd_done(&ctx, &alice, &["2nd Floor"]).await.unwrap();
    let reply = cmd_takeover(&ctx, &bob, &["2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("already completed"), "{reply}");

    let state = ctx.state.lock().await;
    assert_eq!(
        assignee_for(&state, &group_id, year, week),
        Some(first_id),
        "a completed assignment must not be silently reassigned"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn takeover_of_a_skipped_week_is_rejected() {
    let (mut state, group_id, first_id, _second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id),
        source: Default::default(),
    });
    let (ctx, path, admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    cmd_skip(&ctx, &admin, &["2nd Floor"]).await.unwrap();
    let reply = cmd_takeover(&ctx, &bob, &["2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("already completed"), "{reply}");

    let state = ctx.state.lock().await;
    assert!(
        state.is_completed(&group_id, year, week),
        "must remain skipped, not reopened"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn double_done_after_takeover_stays_consistent() {
    let (mut state, group_id, first_id, _second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id),
        source: Default::default(),
    });
    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    cmd_takeover(&ctx, &bob, &["2nd Floor"]).await.unwrap();
    cmd_done(&ctx, &bob, &["2nd Floor"]).await.unwrap();
    let second_reply = cmd_done(&ctx, &bob, &["2nd Floor"]).await.unwrap().unwrap();
    assert!(second_reply.contains("Already done"), "{second_reply}");

    let state = ctx.state.lock().await;
    let count = state
        .completions
        .iter()
        .filter(|c| c.group_id == group_id && c.iso_year == year && c.iso_week == week)
        .count();
    assert_eq!(count, 1, "no duplicate completion record");
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn takeover_then_done_survives_a_full_event_replay() {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let group_id = Uuid::new_v4().to_string();
    state
        .apply_event(DomainEvent::GroupCreated {
            group_id: group_id.clone(),
            name: "2nd Floor".into(),
        })
        .unwrap();
    for mxid in ["@alice:example.org", "@bob:example.org"] {
        let pid = Uuid::new_v4().to_string();
        state
            .apply_event(DomainEvent::PersonCreated {
                person_id: pid.clone(),
                display_name: mxid.into(),
                matrix_id: Some(mxid.into()),
            })
            .unwrap();
        state
            .apply_event(DomainEvent::PersonJoinedGroup {
                person_id: pid,
                group_id: group_id.clone(),
            })
            .unwrap();
    }
    seed_materialized_weeks(&mut state, 1);

    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();
    cmd_takeover(&ctx, &bob, &["2nd Floor"]).await.unwrap();
    cmd_done(&ctx, &bob, &["2nd Floor"]).await.unwrap();

    let before = ctx.state.lock().await.clone();
    let mut replayed = State::default();
    for logged in &before.event_log {
        replayed.apply_event(logged.event.clone()).unwrap();
    }

    assert_eq!(
        serde_json::to_string(&before.slot_assignments).unwrap(),
        serde_json::to_string(&replayed.slot_assignments).unwrap(),
        "current assignee must be identical after full replay"
    );
    // Compare status/ownership, not `completed_at` — that one field is
    // intentionally re-stamped to the apply-time `Utc::now()` (a
    // pre-existing, unrelated property of `CleaningCompleted`'s handler,
    // not something a full replay-from-log ever does in production: the
    // real restart path is `State::load`, a direct deserialize of the
    // already-persisted timestamp, exercised by the reload assertions above).
    let strip_ts = |completions: &[crate::state::Completion]| {
        completions
            .iter()
            .map(|c| {
                (
                    c.group_id.clone(),
                    c.completed_by_id.clone(),
                    c.responsible_person_ids.clone(),
                    c.iso_year,
                    c.iso_week,
                    c.skipped,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        strip_ts(&before.completions),
        strip_ts(&replayed.completions),
        "done status and responsible person must be identical after full replay"
    );

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn accepted_swap_actually_reassigns_an_already_materialized_current_week() {
    // Regression test for the bug this feature request was built around:
    // !acceptswap only recorded swap_requests status before, which
    // `responsible_person` ignores whenever the week is already frozen
    // (materialized in advance, which the current week always is) — so
    // an accepted swap silently had no visible effect.
    let (mut state, group_id, first_id, second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id),
        source: Default::default(),
    });
    let (ctx, path, _admin) = test_context(state);
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    let swap_reply = cmd_swap(&ctx, &alice, &["@bob:example.org", "2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    let id: u64 = swap_reply
        .split('#')
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    cmd_acceptswap(&ctx, &bob, &[&id.to_string()])
        .await
        .unwrap();

    let state = ctx.state.lock().await;
    assert_eq!(
        assignee_for(&state, &group_id, year, week),
        Some(second_id),
        "the frozen current week must reflect the accepted swap, not just swap_requests status"
    );
    // And !done now works for Bob without any special-cased swap lookup.
    drop(state);
    let done_reply = cmd_done(&ctx, &bob, &["2nd Floor"]).await.unwrap().unwrap();
    assert!(done_reply.contains("Cleaned"), "{done_reply}");
    let _ = tokio::fs::remove_file(path).await;
}

/// Three-member group with matrix-linked persons (needed for commands
/// that resolve the sender via their Matrix ID, like !takeover).
fn three_person_state_matrix() -> (State, GroupId, PersonId, PersonId) {
    let alice = Person::new_matrix("@alice:example.org");
    let bob = Person::new_matrix("@bob:example.org");
    let carla = Person::new_matrix("@carla:example.org");
    let (aid, bid) = (alice.id.clone(), bob.id.clone());
    let mut group = CleaningGroup::new("Floor");
    let gid = group.id.clone();
    group.member_ids = vec![alice.id.clone(), bob.id.clone(), carla.id.clone()];
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    state.persons = vec![alice, bob, carla];
    state.cleaning_groups.push(group);
    (state, gid, aid, bid)
}

/// Two matrix-linked members ("Alice", "Bob") in a single group ("Floor")
/// with two named slots ("Kitchen", "Bath"), for !takeover ergonomics tests.
fn multi_slot_group_matrix() -> (State, GroupId, PersonId, PersonId, String, String) {
    let alice = Person::new_matrix("@alice:example.org");
    let bob = Person::new_matrix("@bob:example.org");
    let (aid, bid) = (alice.id.clone(), bob.id.clone());
    let mut group = CleaningGroup::new("Floor");
    let gid = group.id.clone();
    group.member_ids = vec![aid.clone(), bid.clone()];
    let mut kitchen = CleaningSlot::new("Kitchen");
    kitchen.id = "kitchen".into();
    let mut bath = CleaningSlot::new("Bath");
    bath.id = "bath".into();
    let (kitchen_id, bath_id) = (kitchen.id.clone(), bath.id.clone());
    group.slots = vec![kitchen, bath];
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    state.persons = vec![alice, bob];
    state.cleaning_groups.push(group);
    (state, gid, aid, bid, kitchen_id, bath_id)
}

// ── !takeover ergonomics ──────────────────────────────────────────────────

#[tokio::test]
async fn bare_takeover_with_one_group_and_one_slot_just_works() {
    let (mut state, group_id, first_id, second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id),
        source: Default::default(),
    });
    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    let reply = cmd_takeover(&ctx, &bob, &[]).await.unwrap().unwrap();
    assert!(reply.contains("took over"), "{reply}");

    let state = ctx.state.lock().await;
    assert_eq!(assignee_for(&state, &group_id, year, week), Some(second_id));
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn takeover_slot_only_uses_the_senders_own_group() {
    let (mut state, group_id, aid, bid, kitchen_id, _bath_id) = multi_slot_group_matrix();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: current_iso_week().0,
        iso_week: current_iso_week().1,
        shift: 0,
        person_id: Some(aid),
        source: Default::default(),
    });
    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    let reply = cmd_takeover(&ctx, &bob, &["Kitchen"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("took over"), "{reply}");
    assert!(reply.contains("Kitchen"), "{reply}");

    let state = ctx.state.lock().await;
    let (year, week) = current_iso_week();
    let assignment = state
        .slot_assignments
        .iter()
        .find(|a| {
            a.group_id == group_id && a.iso_year == year && a.iso_week == week && a.slot_index == 0
        })
        .unwrap();
    assert_eq!(assignment.person_id.as_deref(), Some(bid.as_str()));
    let _ = kitchen_id;
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn multiple_open_slots_are_listed_instead_of_guessed() {
    // Neither slot is done, skipped, or already Bob's — both are valid
    // candidates, so the bot must not silently pick one.
    let (mut state, group_id, aid, _bid, ..) = multi_slot_group_matrix();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(aid.clone()),
        source: Default::default(),
    });
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 1,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(aid.clone()),
        source: Default::default(),
    });
    let before = state.slot_assignments.clone();
    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    let reply = cmd_takeover(&ctx, &bob, &["Floor"]).await.unwrap().unwrap();
    assert!(reply.contains("Kitchen"), "{reply}");
    assert!(reply.contains("Bath"), "{reply}");
    assert!(reply.contains("!takeover Floor"), "{reply}");

    let state = ctx.state.lock().await;
    assert_eq!(
        state.slot_assignments, before,
        "nothing may change while the choice is ambiguous"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn member_of_several_groups_must_specify_which_one() {
    let (mut state, floor_id, _aid, bid) = three_person_state_matrix();
    let mut kitchen_group = CleaningGroup::new("Kitchen Crew");
    let kitchen_group_id = kitchen_group.id.clone();
    kitchen_group.member_ids = vec![bid.clone()];
    state.cleaning_groups.push(kitchen_group);
    let before = state.slot_assignments.clone();
    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    let reply = cmd_takeover(&ctx, &bob, &[]).await.unwrap().unwrap();
    assert!(reply.contains("multiple groups"), "{reply}");
    assert!(reply.contains("Floor"), "{reply}");
    assert!(reply.contains("Kitchen Crew"), "{reply}");

    let state = ctx.state.lock().await;
    assert_eq!(
        state.slot_assignments, before,
        "nothing may change while the group is ambiguous"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
    let _ = (floor_id, kitchen_group_id);
}

#[tokio::test]
async fn nothing_to_take_over_is_reported_clearly() {
    // Both slots are already Bob's — no candidate left to take over.
    let (mut state, group_id, aid, bid, ..) = multi_slot_group_matrix();
    let (year, week) = current_iso_week();
    for slot_index in [0, 1] {
        state.slot_assignments.push(SlotAssignment {
            group_id: group_id.clone(),
            slot_index,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(bid.clone()),
            source: Default::default(),
        });
    }
    let before = state.slot_assignments.clone();
    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    let reply = cmd_takeover(&ctx, &bob, &["Floor"]).await.unwrap().unwrap();
    assert!(reply.contains("Nothing to take over"), "{reply}");

    let state = ctx.state.lock().await;
    assert_eq!(
        state.slot_assignments, before,
        "both slots were already Bob's — nothing should change"
    );
    let _ = aid;
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn explicit_group_and_slot_syntax_still_works_for_multi_slot_groups() {
    let (mut state, group_id, aid, bid, ..) = multi_slot_group_matrix();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 1,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(aid),
        source: Default::default(),
    });
    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    let reply = cmd_takeover(&ctx, &bob, &["Floor", "Bath"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("took over"), "{reply}");
    assert!(reply.contains("Bath"), "{reply}");

    let state = ctx.state.lock().await;
    let assignment = state
        .slot_assignments
        .iter()
        .find(|a| {
            a.group_id == group_id && a.iso_year == year && a.iso_week == week && a.slot_index == 1
        })
        .unwrap();
    assert_eq!(assignment.person_id.as_deref(), Some(bid.as_str()));
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

// ── Production-audit regression tests ────────────────────────────────

#[tokio::test]
async fn swap_in_a_group_with_slots_moves_only_the_requesters_slot() {
    // Alice holds Kitchen, Bob holds Bath this week. Alice swaps with Carla:
    // Carla must get Kitchen — never Bath, and Bob's slot stays his.
    let (mut state, group_id, _aid, bid, ..) = multi_slot_group_matrix();
    let carla = Person::new_matrix("@carla:example.org");
    let carla_id = carla.id.clone();
    state.persons.push(carla);
    let (ctx, path, _admin) = test_context(state);
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();
    let carla = OwnedUserId::try_from("@carla:example.org").unwrap();
    let (year, week) = current_iso_week();

    let reply = cmd_swap(&ctx, &alice, &["@carla:example.org", "Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("Floor / Kitchen"), "{reply}");
    // Naming a slot Alice doesn't hold is refused.
    let wrong = cmd_swap(&ctx, &alice, &["@carla:example.org", "Floor", "Bath"])
        .await
        .unwrap()
        .unwrap();
    assert!(wrong.contains("isn't yours"), "{wrong}");

    let id = ctx.state.lock().await.swap_requests[0].id.to_string();
    let accepted = cmd_acceptswap(&ctx, &carla, &[id.as_str()])
        .await
        .unwrap()
        .unwrap();
    assert!(accepted.contains("Kitchen"), "{accepted}");

    let state = ctx.state.lock().await;
    let group = state.group_by_id(&group_id).unwrap().clone();
    assert_eq!(
        state
            .slot_assignee(&group, 0, Turn::new(year, week, 0))
            .map(|p| p.id.clone()),
        Some(carla_id)
    );
    assert_eq!(
        state
            .slot_assignee(&group, 1, Turn::new(year, week, 0))
            .map(|p| p.id.clone()),
        Some(bid)
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn accepting_a_swap_after_the_week_was_reassigned_elsewhere_is_refused_not_overwritten() {
    // Alice requests a swap with Bob. Before Bob gets around to
    // accepting, the week is reassigned to Carla (e.g. an admin
    // !assign, or Alice left the group and it was refilled). Accepting
    // the now-stale swap must not silently hand Carla's week to Bob
    // without her consent.
    let (mut state, group_id, first_id, _second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id.clone()),
        source: Default::default(),
    });
    let carla = Person::new_matrix("@carla:example.org");
    let carla_id = carla.id.clone();
    state.persons.push(carla);

    let (ctx, path, _admin) = test_context(state);
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    let swap_reply = cmd_swap(&ctx, &alice, &["@bob:example.org", "2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    let id: u64 = swap_reply
        .split('#')
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();

    {
        let mut state = ctx.state.lock().await;
        state
            .apply_event(DomainEvent::SlotAssigned {
                group_id: group_id.clone(),
                slot_index: 0,
                iso_year: year,
                iso_week: week,
                shift: 0,
                person_id: Some(carla_id.clone()),
                source: AssignmentSource::Assign,
                actor_id: Some("@admin:example.org".to_owned()),
                previous_person_id: Some(first_id.clone()),
            })
            .unwrap();
        state.save(&path).await.unwrap();
    }

    let accept_reply = cmd_acceptswap(&ctx, &bob, &[&id.to_string()])
        .await
        .unwrap()
        .unwrap();
    assert!(accept_reply.contains("no longer valid"), "{accept_reply}");

    let state = ctx.state.lock().await;
    assert_eq!(
        assignee_for(&state, &group_id, year, week),
        Some(carla_id),
        "Carla's reassignment must survive an unrelated stale swap acceptance"
    );
    assert_eq!(
        state
            .swap_requests
            .iter()
            .find(|s| s.id == id)
            .unwrap()
            .status,
        SwapStatus::Rejected,
        "the stale request should be auto-cancelled, not left pending forever"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn removing_a_slot_repoints_future_assignments_at_the_same_physical_slot() {
    // SlotAssignment.slot_index is a raw position into group.slots, not
    // a stable id. Removing a middle slot used to leave already-frozen
    // assignments for later slots pointing at the wrong (shifted-down)
    // slot index — silently reattributing someone's future assignment
    // to a different room.
    let mut group = CleaningGroup::new("Floor");
    let gid = group.id.clone();
    let mut kitchen = CleaningSlot::new("Kitchen");
    kitchen.id = "kitchen".into();
    let mut bath = CleaningSlot::new("Bath");
    bath.id = "bath".into();
    let mut hallway = CleaningSlot::new("Hallway");
    hallway.id = "hallway".into();
    group.slots = vec![kitchen, bath, hallway];
    let alice = Person::new_named("Alice");
    let bob = Person::new_named("Bob");
    let (aid, bid) = (alice.id.clone(), bob.id.clone());
    group.member_ids = vec![aid.clone(), bid.clone()];

    let mut state = State::default();
    state.persons = vec![alice, bob];
    state.cleaning_groups.push(group);
    let (year, week) = current_iso_week();
    state.slot_assignments = vec![
        SlotAssignment {
            group_id: gid.clone(),
            slot_index: 0,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(aid.clone()),
            source: Default::default(),
        },
        SlotAssignment {
            group_id: gid.clone(),
            slot_index: 2,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(bid.clone()),
            source: Default::default(),
        },
    ];

    let (ctx, path, admin) = test_context(state);
    let reply = cmd_removeslot(&ctx, &admin, &["Floor", "Bath"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("Removed slot"), "{reply}");

    let state = ctx.state.lock().await;
    let group = state.group_by_id(&gid).unwrap();
    assert_eq!(
        group
            .slots
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        vec!["kitchen", "hallway"]
    );

    // Alice's Kitchen assignment (index 0, before the removed slot) is untouched.
    let alice_assignment = state
        .slot_assignments
        .iter()
        .find(|a| a.person_id.as_deref() == Some(aid.as_str()))
        .unwrap();
    assert_eq!(alice_assignment.slot_index, 0);

    // Bob's Hallway assignment (index 2, after the removed slot) must be
    // re-pointed to Hallway's new index, not silently become Bath's old slot.
    let bob_assignment = state
        .slot_assignments
        .iter()
        .find(|a| a.person_id.as_deref() == Some(bid.as_str()))
        .unwrap();
    assert_eq!(
        bob_assignment.slot_index, 1,
        "must be re-pointed at Hallway's new index"
    );
    assert_eq!(
        group
            .slots
            .get(bob_assignment.slot_index)
            .map(|s| s.id.as_str()),
        Some("hallway"),
        "the re-pointed index must resolve back to the same physical slot"
    );
    assert!(state
        .slot_assignments
        .iter()
        .all(|a| a.slot_index < group.slots.len()));
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

// ── !absent affects eligibility, not already-frozen weeks ────────────

#[tokio::test]
async fn absence_declared_after_the_week_is_frozen_does_not_retroactively_change_it() {
    // Anna is already frozen for the running week. Marking her absent
    // afterward must not touch that assignment — the "automatic
    // rotation never rewrites an already-frozen week" rule applies to
    // absence exactly like it does to joins and leaves. Standing in for
    // her is a manual act (!takeover/!swap/!assign), not automatic.
    let (mut state, group_id, first_id, _second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id.clone()),
        source: Default::default(),
    });
    let (ctx, path, admin) = test_context(state);

    let reply = cmd_absent(&ctx, &admin, &["@alice:example.org"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("away"), "{reply}");

    let state = ctx.state.lock().await;
    assert_eq!(
        assignee_for(&state, &group_id, year, week),
        Some(first_id),
        "an already-frozen assignment must survive a later !absent for the same person"
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

// ── Audit metadata ─────────────────────────────────────────────────────

#[tokio::test]
async fn assign_takeover_and_swap_produce_distinguishable_audit_metadata() {
    // All three manual-override paths used to write the same
    // indistinguishable `AssignmentSource::Manual` — the event log
    // couldn't tell an admin !assign, a self-service !takeover, and an
    // accepted swap apart. Each must now carry its own source, plus who
    // triggered it and who they replaced.
    let (mut state, group_id, first_id, second_id) = rotation_state();
    let (year, week) = current_iso_week();
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(first_id.clone()),
        source: Default::default(),
    });
    let carla = Person::new_matrix("@carla:example.org");
    let carla_id = carla.id.clone();
    state.persons.push(carla);
    let (next_year, next_week) = add_weeks(year, week, 1);
    // Freeze next week's pick as Bob up front, so the swap-acceptance
    // check below ("is the requester still the current holder") has a
    // deterministic answer instead of depending on queue-preview internals.
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year: next_year,
        iso_week: next_week,
        shift: 0,
        person_id: Some(second_id.clone()),
        source: Default::default(),
    });

    let (ctx, path, admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    // 1. Admin !assign — actor is the admin, previous is Alice.
    cmd_assign(&ctx, &admin, &["2nd Floor", "@carla:example.org"])
        .await
        .unwrap();
    {
        let state = ctx.state.lock().await;
        let a = state
            .slot_assignments
            .iter()
            .find(|a| a.group_id == group_id && a.iso_year == year && a.iso_week == week)
            .unwrap();
        assert_eq!(a.source, AssignmentSource::Assign);
    }
    let assign_event = last_slot_assigned_event(&ctx).await;
    assert_eq!(assign_event.0, AssignmentSource::Assign);
    assert_eq!(assign_event.1.as_deref(), Some("@admin:example.org"));
    assert_eq!(assign_event.2.as_deref(), Some(first_id.as_str()));

    // 2. Self-service !takeover — actor is the claimant, previous is Carla.
    let takeover_reply = cmd_takeover(&ctx, &bob, &["2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(takeover_reply.contains("took over"), "{takeover_reply}");
    let bob_id = {
        ctx.state
            .lock()
            .await
            .person_by_matrix_id("@bob:example.org")
            .unwrap()
            .id
            .clone()
    };
    let takeover_event = last_slot_assigned_event(&ctx).await;
    assert_eq!(takeover_event.0, AssignmentSource::Takeover);
    assert_eq!(takeover_event.1.as_deref(), Some("@bob:example.org"));
    assert_eq!(takeover_event.2.as_deref(), Some(carla_id.as_str()));

    // 3. Accepted swap (next week, so it doesn't collide with the
    // already-completed-this-week checks above) — actor is the
    // accepter, previous is the original requester.
    let swap_reply = cmd_swap(
        &ctx,
        &bob,
        &[
            "@alice:example.org",
            "2nd Floor",
            "week",
            &next_week.to_string(),
        ],
    )
    .await
    .unwrap()
    .unwrap();
    let id: u64 = swap_reply
        .split('#')
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();
    cmd_acceptswap(&ctx, &alice, &[&id.to_string()])
        .await
        .unwrap();
    let swap_event = last_slot_assigned_event(&ctx).await;
    assert_eq!(swap_event.0, AssignmentSource::Swap);
    assert_eq!(swap_event.1.as_deref(), Some("@alice:example.org"));
    assert_eq!(swap_event.2.as_deref(), Some(bob_id.as_str()));

    let _ = tokio::fs::remove_file(path).await;
}

/// The `(source, actor_id, previous_person_id)` of the most recent
/// `SlotAssigned` in the event log — lets a test check the audit trail
/// a command actually left behind, not just the resulting live state.
async fn last_slot_assigned_event(
    ctx: &BotContext,
) -> (AssignmentSource, Option<String>, Option<PersonId>) {
    let state = ctx.state.lock().await;
    state
        .event_log
        .iter()
        .rev()
        .find_map(|logged| match &logged.event {
            DomainEvent::SlotAssigned {
                source,
                actor_id,
                previous_person_id,
                ..
            } => Some((source.clone(), actor_id.clone(), previous_person_id.clone())),
            _ => None,
        })
        .expect("expected a SlotAssigned event in the log")
}

// ── Command overhaul: status, groups, undo, done, renamed commands ───────────

/// "Floor" with two slots cleaned by two people in the same week: Alice has
/// Scharni, Bob has Colbe. Carol (no Matrix) is a third member.
fn two_slot_week() -> (State, GroupId, PersonId, PersonId) {
    let mut state = State::default();
    let alice = Person::new_matrix("@alice:example.org");
    let bob = Person::new_matrix("@bob:example.org");
    let carol = Person::new_named("Carol");
    let (alice_id, bob_id) = (alice.id.clone(), bob.id.clone());
    let mut group = CleaningGroup::new("Floor");
    let group_id = group.id.clone();
    group.member_ids = vec![alice_id.clone(), bob_id.clone(), carol.id.clone()];
    let mut scharni = CleaningSlot::new("Scharni");
    scharni.id = "s0".into();
    let mut colbe = CleaningSlot::new("Colbe");
    colbe.id = "s1".into();
    group.slots = vec![scharni, colbe];
    state.persons = vec![alice, bob, carol];
    state.cleaning_groups.push(group);
    let (year, week) = current_iso_week();
    for (slot_index, person_id) in [(0, &alice_id), (1, &bob_id)] {
        state
            .apply_event(DomainEvent::SlotAssigned {
                group_id: group_id.clone(),
                slot_index,
                iso_year: year,
                iso_week: week,
                shift: 0,
                person_id: Some(person_id.clone()),
                source: AssignmentSource::Assign,
                actor_id: None,
                previous_person_id: None,
            })
            .unwrap();
    }
    (state, group_id, alice_id, bob_id)
}

#[tokio::test]
async fn status_lists_every_person_of_a_shared_week_with_their_own_state() {
    let (state, _group_id, _alice_id, _bob_id) = two_slot_week();
    let (ctx, path, _admin) = test_context(state);
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();

    let before = cmd_status(&ctx).await.unwrap().unwrap();
    assert!(before.contains("0 of 2 done"), "{before}");
    assert!(before.contains("⬜ Scharni · alice"), "{before}");
    assert!(before.contains("⬜ Colbe · bob"), "{before}");

    cmd_done(&ctx, &alice, &[]).await.unwrap().unwrap();
    let after = cmd_status(&ctx).await.unwrap().unwrap();
    assert!(after.contains("1 of 2 done"), "{after}");
    assert!(after.contains("✅ Scharni · alice"), "{after}");
    assert!(after.contains("⬜ Colbe · bob"), "{after}");
    // Status never pings: names, not Matrix IDs.
    assert!(!after.contains("@alice:example.org"), "{after}");

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn status_shows_who_actually_cleaned_and_skips() {
    let (state, group_id, _alice_id, bob_id) = two_slot_week();
    let (ctx, path, admin) = test_context(state);
    let (year, week) = current_iso_week();
    {
        let mut state = ctx.state.lock().await;
        // Bob cleans Alice's slot.
        state
            .apply_event(DomainEvent::CleaningCompleted {
                group_id: group_id.clone(),
                slot_id: Some("s0".into()),
                person_id: bob_id.clone(),
                responsible_person_ids: vec![],
                iso_year: year,
                iso_week: week,
                shift: 0,
            })
            .unwrap();
    }
    cmd_skip(&ctx, &admin, &["Floor"]).await.unwrap();

    let text = cmd_status(&ctx).await.unwrap().unwrap();
    assert!(text.contains("✅ Scharni · alice (done by bob)"), "{text}");
    assert!(text.contains("⏭️ Colbe · bob · skipped"), "{text}");
    assert!(text.contains("2 of 2 done"), "{text}");

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn undo_in_a_shared_week_only_takes_back_the_senders_own_slot() {
    let (state, group_id, _alice_id, _bob_id) = two_slot_week();
    let (ctx, path, admin) = test_context(state);
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();
    let (year, week) = current_iso_week();

    cmd_done(&ctx, &alice, &[]).await.unwrap();
    cmd_done(&ctx, &bob, &[]).await.unwrap();
    assert!(ctx.state.lock().await.is_completed(&group_id, year, week));

    let reply = cmd_undo(&ctx, &alice, &[]).await.unwrap().unwrap();
    assert!(reply.contains("Floor / Scharni"), "{reply}");
    {
        let state = ctx.state.lock().await;
        assert!(!state.is_slot_completed(&group_id, &"s0".to_owned(), year, week));
        assert!(
            state.is_slot_completed(&group_id, &"s1".to_owned(), year, week),
            "Bob's mark must survive Alice's undo"
        );
    }

    // An admin naming the group clears the whole week.
    cmd_undo(&ctx, &admin, &["Floor"]).await.unwrap();
    assert!(!ctx
        .state
        .lock()
        .await
        .is_slot_completed(&group_id, &"s1".to_owned(), year, week));

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn bare_done_only_marks_what_is_open_for_the_sender() {
    let (mut state, group_id, alice_id, bob_id) = two_slot_week();
    // Bob is also in a plain group where it's Alice's turn.
    let mut kitchen = CleaningGroup::new("Kitchen");
    let kitchen_id = kitchen.id.clone();
    kitchen.member_ids = vec![alice_id.clone(), bob_id.clone()];
    state.cleaning_groups.push(kitchen);
    let (year, week) = current_iso_week();
    state
        .apply_event(DomainEvent::SlotAssigned {
            group_id: kitchen_id.clone(),
            slot_index: 0,
            iso_year: year,
            iso_week: week,
            shift: 0,
            person_id: Some(alice_id),
            source: AssignmentSource::Assign,
            actor_id: None,
            previous_person_id: None,
        })
        .unwrap();
    let (ctx, path, _admin) = test_context(state);
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();
    let stranger = OwnedUserId::try_from("@nobody:example.org").unwrap();

    let reply = cmd_done(&ctx, &bob, &[]).await.unwrap().unwrap();
    assert!(reply.contains("Floor / Colbe"), "{reply}");
    {
        let state = ctx.state.lock().await;
        assert!(state.is_slot_completed(&group_id, &"s1".to_owned(), year, week));
        assert!(!state.is_slot_completed(&group_id, &"s0".to_owned(), year, week));
        assert!(
            !state.is_completed(&kitchen_id, year, week),
            "a group where nothing is open for Bob must not be marked"
        );
    }

    let reply = cmd_done(&ctx, &stranger, &[]).await.unwrap().unwrap();
    assert!(reply.contains("not registered"), "{reply}");

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn groups_overview_shows_every_group_with_its_members() {
    let (mut state, _group_id, _alice_id, bob_id) = two_slot_week();
    let mut storage = CleaningGroup::new("Storage");
    storage.member_ids = vec![bob_id];
    storage.is_active = false;
    state.cleaning_groups.push(storage);
    let (ctx, path, _admin) = test_context(state);

    let text = cmd_groups(&ctx, None).await.unwrap().unwrap();
    assert!(
        text.contains("**Floor** (3) · alice, bob, Carol (no Matrix)"),
        "{text}"
    );
    assert!(text.contains("Slots: Scharni · Colbe"), "{text}");
    assert!(
        text.contains("🚫 **Storage** (1) · bob · disabled"),
        "{text}"
    );

    let detail = cmd_groups(&ctx, Some("floor")).await.unwrap().unwrap();
    assert!(detail.contains("🏢 **Floor**"), "{detail}");
    assert!(detail.contains("⬜ Scharni · alice"), "{detail}");
    assert!(detail.contains("@bob:example.org"), "{detail}");

    let missing = cmd_groups(&ctx, Some("Attic")).await.unwrap().unwrap();
    assert!(missing.contains("not found"), "{missing}");

    let _ = tokio::fs::remove_file(path).await;
}

#[test]
fn old_command_names_point_to_their_replacement() {
    assert!(renamed_command_hint("!cleanplan")
        .unwrap()
        .contains("!plan [N]"));
    assert!(renamed_command_hint("!adduser")
        .unwrap()
        .contains("!member add"));
    assert!(renamed_command_hint("!setroomweight")
        .unwrap()
        .contains("!groups weight"));
    assert!(renamed_command_hint("!unknown").is_none());
}

#[test]
fn help_is_short_and_admin_help_is_separate() {
    let help = help_text();
    assert!(help.lines().count() <= 16, "{help}");
    assert!(help.contains("!groups"));
    assert!(!help.contains("!plan assign"));
    assert!(admin_help_text().contains("!plan assign"));
}

// ── Follow-up fixes: !next, names, deletion, stats, slots, away ──────────────

fn freeze(
    state: &mut State,
    group_id: &GroupId,
    slot_index: usize,
    (year, week): (i32, u32),
    person: &PersonId,
) {
    state.slot_assignments.push(SlotAssignment {
        group_id: group_id.clone(),
        slot_index,
        iso_year: year,
        iso_week: week,
        shift: 0,
        person_id: Some(person.clone()),
        source: Default::default(),
    });
}

#[tokio::test]
async fn next_names_the_persons_own_turn_not_just_the_next_due_week() {
    let (mut state, group_id, alice_id, bob_id) = rotation_state();
    let (y, w) = current_iso_week();
    freeze(&mut state, &group_id, 0, (y, w), &alice_id);
    freeze(&mut state, &group_id, 0, add_weeks(y, w, 1), &bob_id);
    let (ctx, path, _admin) = test_context(state);
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();
    let bob = OwnedUserId::try_from("@bob:example.org").unwrap();

    let reply = cmd_next(&ctx, &bob, &[]).await.unwrap().unwrap();
    assert!(reply.contains("next week"), "{reply}");
    let reply = cmd_next(&ctx, &alice, &[]).await.unwrap().unwrap();
    assert!(reply.contains("this week"), "{reply}");

    cmd_done(&ctx, &alice, &[]).await.unwrap();
    let reply = cmd_next(&ctx, &alice, &[]).await.unwrap().unwrap();
    assert!(!reply.contains("this week"), "{reply}");
    assert!(reply.contains("Already done: 2nd Floor"), "{reply}");

    let _ = tokio::fs::remove_file(path).await;
}

#[test]
fn apostrophes_are_letters_and_typographic_quotes_group_words() {
    assert_eq!(
        tokenize("!member add Nick's \u{201c}2nd Floor\u{201d}"),
        ["!member", "add", "Nick's", "2nd Floor"]
    );
    assert_eq!(
        tokenize("!groups add \"Dach Boden\""),
        ["!groups", "add", "Dach Boden"]
    );
}

#[test]
fn multi_word_names_need_no_quotes() {
    let (mut state, _group_id, _alice_id, _bob_id) = rotation_state();
    let mut floor = CleaningGroup::new("Floor");
    floor.slots = vec![CleaningSlot::new("Scharni Toilette")];
    state.cleaning_groups.push(floor);
    let norm = |cmd: &str, args: &[&str]| normalize_args(&state, cmd, args);

    assert_eq!(
        norm("!member", &["add", "Max", "Mustermann", "2nd", "Floor"]),
        ["add", "Max Mustermann", "2nd Floor"]
    );
    assert_eq!(
        norm(
            "!plan",
            &["assign", "2nd", "floor", "Max", "Mustermann", "week", "40"]
        ),
        ["assign", "2nd Floor", "Max", "Mustermann", "week", "40"]
    );
    assert_eq!(
        norm(
            "!groups",
            &["weight", "2nd", "Floor", "Big", "Kitchen", "1.5"]
        ),
        ["weight", "2nd Floor", "Big Kitchen", "1.5"]
    );
    assert_eq!(
        norm(
            "!groups",
            &["slot", "add", "2nd", "Floor", "Scharni", "Toilette"]
        ),
        ["slot", "add", "2nd Floor", "Scharni Toilette"]
    );
    assert_eq!(
        norm("!groups", &["remove", "2nd", "Floor", "confirm"]),
        ["remove", "2nd Floor", "confirm"]
    );
    assert_eq!(
        norm("!member", &["away", "Max", "Mustermann", "3"]),
        ["away", "Max Mustermann", "3"]
    );
    assert_eq!(norm("!done", &["2nd", "Floor"]), ["2nd Floor"]);
    assert_eq!(
        norm("!swap", &["@bob:example.org", "2nd", "Floor", "week", "40"]),
        ["@bob:example.org", "2nd Floor", "week", "40"]
    );
}

#[tokio::test]
async fn deleting_a_group_needs_confirmation_and_leaves_nothing_behind() {
    let (mut state, group_id, alice_id, _bob_id) = rotation_state();
    freeze(&mut state, &group_id, 0, current_iso_week(), &alice_id);
    let (ctx, path, admin) = test_context(state);
    let alice = OwnedUserId::try_from("@alice:example.org").unwrap();
    cmd_done(&ctx, &alice, &[]).await.unwrap();

    let warning = cmd_removefloor(&ctx, &admin, &["2nd Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(
        warning.contains("!groups remove 2nd Floor confirm"),
        "{warning}"
    );
    assert!(ctx.state.lock().await.group_by_id(&group_id).is_some());

    cmd_removefloor(&ctx, &admin, &["2nd Floor", "confirm"])
        .await
        .unwrap();
    let state = ctx.state.lock().await;
    assert!(state.group_by_id(&group_id).is_none());
    assert!(state
        .slot_assignments
        .iter()
        .all(|a| a.group_id != group_id));
    assert!(state.completions.iter().all(|c| c.group_id != group_id));
    let report = crate::validate::validate_state(&state);
    assert!(
        !report.summary().contains(&group_id),
        "no dangling references: {}",
        report.summary()
    );
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[test]
fn personal_stats_count_own_turns_not_every_week_of_the_group() {
    let (mut state, group_id, alice_id, bob_id) = rotation_state();
    let (y, w) = current_iso_week();
    // Four past weeks alternating Alice/Bob: Alice cleaned both of hers,
    // Bob missed his first and cleaned his second.
    for (back, person) in [(4, &alice_id), (3, &bob_id), (2, &alice_id), (1, &bob_id)] {
        freeze(&mut state, &group_id, 0, add_weeks(y, w, -back), person);
    }
    for (back, by) in [(4, &alice_id), (2, &alice_id), (1, &bob_id)] {
        let (cy, cw) = add_weeks(y, w, -back);
        state.completions.push(crate::state::Completion {
            group_id: group_id.clone(),
            slot_id: None,
            completed_by_id: by.clone(),
            responsible_person_ids: vec![],
            iso_year: cy,
            iso_week: cw,
            shift: 0,
            completed_at: Utc::now(),
            skipped: false,
        });
    }

    let alice = analytics::person_stats(&state, &alice_id).unwrap();
    assert_eq!((alice.completed, alice.due_weeks, alice.missed), (2, 2, 0));
    assert_eq!(alice.completion_rate, 1.0);
    assert_eq!(alice.streak, 2);
    let bob = analytics::person_stats(&state, &bob_id).unwrap();
    assert_eq!((bob.completed, bob.due_weeks, bob.missed), (1, 2, 1));
    assert_eq!(bob.streak, 1);
}

#[tokio::test]
async fn skipping_one_slot_leaves_the_other_open() {
    let (state, group_id, ..) = two_slot_week();
    let (ctx, path, admin) = test_context(state);
    let (year, week) = current_iso_week();

    let reply = cmd_skip(&ctx, &admin, &["Floor", "Colbe"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("Floor / Colbe"), "{reply}");
    let state = ctx.state.lock().await;
    assert!(state.is_slot_completed(&group_id, &"s1".to_owned(), year, week));
    assert!(!state.is_slot_completed(&group_id, &"s0".to_owned(), year, week));
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn going_away_points_out_weeks_already_planned_for_the_person() {
    let (mut state, group_id, alice_id, _bob_id) = rotation_state();
    let next = add_weeks(current_iso_week().0, current_iso_week().1, 1);
    freeze(&mut state, &group_id, 0, next, &alice_id);
    let (ctx, path, admin) = test_context(state);

    let reply = cmd_absent(&ctx, &admin, &["@alice:example.org", "3"])
        .await
        .unwrap()
        .unwrap();
    assert!(
        reply.contains(&format!(
            "Still planned for them: week {} 2nd Floor",
            next.1
        )),
        "{reply}"
    );
    let _ = tokio::fs::remove_file(path).await;
}

// ── Rhythms: Kitchen weekly, Bathroom twice a week ───────────────────────────

/// Kitchen (weekly) and Bathroom (2× per week: Mon–Wed, Thu–Sun) sharing the
/// same four people, planned four weeks ahead.
fn kitchen_and_bathroom() -> (State, CleaningGroup, CleaningGroup, Vec<PersonId>) {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let people: Vec<Person> = [
        "@anna:example.org",
        "@ben:example.org",
        "@cleo:example.org",
        "@dan:example.org",
    ]
    .iter()
    .map(|m| Person::new_matrix(m))
    .collect();
    let ids: Vec<PersonId> = people.iter().map(|p| p.id.clone()).collect();
    state.persons = people;
    let mut kitchen = CleaningGroup::new("Kitchen");
    kitchen.member_ids = ids.clone();
    let mut bathroom = CleaningGroup::new("Bathroom");
    bathroom.member_ids = ids.clone();
    bathroom.rhythm = crate::rhythm::Rhythm {
        every_weeks: Some(1),
        shift_starts: crate::rhythm::Rhythm::times_per_week(2),
    };
    state.cleaning_groups = vec![kitchen.clone(), bathroom.clone()];
    for ev in resolver::materialize(&state, 4) {
        state.apply_event(ev).unwrap();
    }
    (state, kitchen, bathroom, ids)
}

fn holder(state: &State, group: &CleaningGroup, turn: Turn) -> PersonId {
    state.slot_assignee(group, 0, turn).unwrap().id.clone()
}

#[test]
fn a_weekly_and_a_twice_weekly_group_are_planned_side_by_side() {
    let (state, kitchen, bathroom, ids) = kitchen_and_bathroom();
    let (y, w) = current_iso_week();

    // Kitchen: one turn per week, one person after the other.
    let kitchen_people: Vec<PersonId> = (0..4)
        .map(|i| {
            let (y, w) = add_weeks(y, w, i);
            assert_eq!(state.turns_in_week(&kitchen, y, w).len(), 1);
            holder(&state, &kitchen, Turn::new(y, w, 0))
        })
        .collect();
    assert_eq!(kitchen_people, ids);

    // Bathroom: two turns per week, two different people, continuing
    // through the same rotation: Anna/Ben, Cleo/Dan, Anna/Ben, …
    let mut bathroom_people = Vec::new();
    for i in 0..4 {
        let (y, w) = add_weeks(y, w, i);
        let turns = state.turns_in_week(&bathroom, y, w);
        assert_eq!(turns.len(), 2);
        let first = holder(&state, &bathroom, turns[0]);
        let second = holder(&state, &bathroom, turns[1]);
        assert_ne!(
            first, second,
            "the two halves of a week go to different people"
        );
        bathroom_people.extend([first, second]);
    }
    // Fair: over 4 weeks (8 turns) everyone had exactly two.
    for id in &ids {
        assert_eq!(bathroom_people.iter().filter(|p| *p == id).count(), 2);
    }
    // Shifts have their own dates.
    let (start, end) = Turn::new(y, w, 1).dates(&bathroom.rhythm);
    assert_eq!(start.weekday(), chrono::Weekday::Thu);
    assert_eq!(end.weekday(), chrono::Weekday::Sun);
}

#[tokio::test]
async fn status_shows_each_shift_with_its_own_person_state_and_whats_next() {
    let (state, _kitchen, bathroom, ids) = kitchen_and_bathroom();
    let (y, w) = current_iso_week();
    let first_shift_person = state
        .person_by_id(&holder(&state, &bathroom, Turn::new(y, w, 0)))
        .unwrap()
        .matrix_id
        .clone()
        .unwrap();
    let (ctx, path, _admin) = test_context(state);
    let _ = ids;

    // Mark the first shift done (a past week would be simpler, but the
    // status is about the current one).
    {
        let mut state = ctx.state.lock().await;
        let pid = state
            .person_by_matrix_id(&first_shift_person)
            .unwrap()
            .id
            .clone();
        let duty = Duty {
            group: bathroom.clone(),
            slot_index: 0,
            turn: Turn::new(y, w, 0),
        };
        mark_duties_done(&mut state, &pid, &[duty]).unwrap();
    }
    let text = cmd_status(&ctx).await.unwrap().unwrap();
    assert!(
        text.contains("**Bathroom** · 2× per week (Mon–Wed, Thu–Sun)"),
        "{text}"
    );
    assert!(text.contains("✅ Mon–Wed · "), "{text}");
    assert!(text.contains("⬜ Thu–Sun · "), "{text}");
    assert!(text.contains("**Kitchen**\n⬜ anna"), "{text}");
    assert!(text.contains("1 of 3 done"), "{text}");
    assert!(
        text.contains(&format!("Next (week {}): Mon–Wed ", add_weeks(y, w, 1).1)),
        "{text}"
    );
    // The running shift is marked.
    let now_marked = text.lines().filter(|l| l.ends_with("← now")).count();
    assert!(now_marked <= 1, "{text}");

    let _ = tokio::fs::remove_file(path).await;
}

#[test]
fn done_marks_started_turns_and_otherwise_only_the_next_one() {
    let (mut state, _kitchen, bathroom, _ids) = kitchen_and_bathroom();
    let (y, w) = current_iso_week();
    let next_week = add_weeks(y, w, 1);
    // Next week nothing has started: only the earliest own turn counts.
    let who = holder(&state, &bathroom, Turn::new(next_week.0, next_week.1, 1));
    let duties = markable_duties(&state, &who, next_week, None);
    assert!(!duties.is_empty());
    let first_start = duties[0].turn.dates(&duties[0].group.rhythm).0;
    assert!(duties
        .iter()
        .all(|d| d.turn.dates(&d.group.rhythm).0 == first_start));

    // A past week: every own turn has started, all are markable at once.
    let last_week = add_weeks(y, w, -1);
    freeze(&mut state, &bathroom.id, 0, last_week, &who);
    state.slot_assignments.push(SlotAssignment {
        group_id: bathroom.id.clone(),
        slot_index: 0,
        iso_year: last_week.0,
        iso_week: last_week.1,
        shift: 1,
        person_id: Some(who.clone()),
        source: Default::default(),
    });
    let duties = markable_duties(&state, &who, last_week, Some(&bathroom.id));
    assert_eq!(duties.len(), 2);
    mark_duties_done(&mut state, &who, &duties).unwrap();
    assert!(state.is_completed(&bathroom.id, last_week.0, last_week.1));
}

#[test]
fn reminders_follow_each_turn() {
    let (state, _kitchen, bathroom, _ids) = kitchen_and_bathroom();
    let (y, w) = current_iso_week();
    let day =
        |weekday: u32| crate::rhythm::week_monday(y, w) + chrono::Duration::days(weekday as i64);
    let names = |turns: Vec<(CleaningGroup, Turn)>| -> Vec<(String, u8)> {
        turns.into_iter().map(|(g, t)| (g.name, t.shift)).collect()
    };
    let initial = crate::state::ReminderKind::Initial;
    let final_ = crate::state::ReminderKind::Final;

    // Monday: the weekly plan covers everything starting then — no extra notice.
    assert!(scheduler::turns_to_remind(&state, day(0), &initial, 6).is_empty());
    // Wednesday: Bathroom's first shift ends.
    assert_eq!(
        names(scheduler::turns_to_remind(&state, day(2), &final_, 6)),
        [("Bathroom".to_owned(), 0)]
    );
    // Thursday: Bathroom's second shift starts.
    assert_eq!(
        names(scheduler::turns_to_remind(&state, day(3), &initial, 6)),
        [("Bathroom".to_owned(), 1)]
    );
    // Sunday: Kitchen's week and Bathroom's second shift end.
    assert_eq!(
        names(scheduler::turns_to_remind(&state, day(6), &final_, 6)),
        [("Kitchen".to_owned(), 0), ("Bathroom".to_owned(), 1)]
    );

    // A turn that's done, or already reminded, isn't reminded again.
    let mut state = state;
    let who = holder(&state, &bathroom, Turn::new(y, w, 1));
    mark_duties_done(
        &mut state,
        &who,
        &[Duty {
            group: bathroom.clone(),
            slot_index: 0,
            turn: Turn::new(y, w, 1),
        }],
    )
    .unwrap();
    state.mark_reminder_sent(
        &state.cleaning_groups[0].id.clone(),
        y,
        w,
        0,
        final_.clone(),
    );
    assert!(scheduler::turns_to_remind(&state, day(6), &final_, 6).is_empty());
}

#[tokio::test]
async fn changing_the_rhythm_replans_later_weeks_without_costing_anyone_a_turn() {
    let (mut state, kitchen, _bathroom, ids) = kitchen_and_bathroom();
    state.cleaning_groups.retain(|g| g.id == kitchen.id);
    let (y, w) = current_iso_week();
    let this_week = holder(&state, &kitchen, Turn::new(y, w, 0));
    let (ctx, path, admin) = test_context(state);

    let reply = cmd_groups_rhythm(&ctx, &admin, &["Kitchen", "2x"])
        .await
        .unwrap()
        .unwrap();
    assert!(
        reply.contains("now cleaned 2× per week (Mon–Wed, Thu–Sun)"),
        "{reply}"
    );

    let state = ctx.state.lock().await;
    let kitchen = state.group_by_id(&kitchen.id).unwrap().clone();
    // This week's existing turn keeps its person; the new second shift and
    // later weeks continue the rotation where it stood.
    assert_eq!(holder(&state, &kitchen, Turn::new(y, w, 0)), this_week);
    let mut order = vec![this_week];
    for i in 0..3 {
        let (y, w) = add_weeks(y, w, i);
        for turn in state.turns_in_week(&kitchen, y, w) {
            if (turn.year, turn.week, turn.shift) != (y, w, 0) || i > 0 {
                order.push(holder(&state, &kitchen, turn));
            }
        }
    }
    assert_eq!(
        &order[..4],
        &ids[..],
        "Anna, Ben, Cleo, Dan — nobody skipped"
    );
    drop(state);

    // Every second week, from the tracking start.
    cmd_groups_rhythm(&ctx, &admin, &["Kitchen", "weekly", "every", "2"])
        .await
        .unwrap();
    let state = ctx.state.lock().await;
    let kitchen = state.group_by_id(&kitchen.id).unwrap().clone();
    assert_eq!(kitchen.rhythm.describe(), "every 2 weeks");
    let (ny, nw) = add_weeks(y, w, 1);
    assert!(state.turns_in_week(&kitchen, ny, nw).is_empty());
    assert!(state
        .slot_assignments
        .iter()
        .all(|a| a.group_id != kitchen.id
            || (a.iso_year, a.iso_week) <= (y, w)
            || state.is_due_week(&kitchen, a.iso_year, a.iso_week)));
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

#[test]
fn rhythm_specs_parse() {
    let weekly = crate::rhythm::Rhythm::weekly();
    let r = parse_rhythm(&weekly, &["2x"]).unwrap();
    assert_eq!(r.describe(), "2× per week (Mon–Wed, Thu–Sun)");
    let r = parse_rhythm(&weekly, &["thu"]).unwrap();
    assert_eq!(r.describe(), "2× per week (Mon–Wed, Thu–Sun)");
    let r = parse_rhythm(&weekly, &["every", "2", "weeks"]).unwrap();
    assert_eq!(r.describe(), "every 2 weeks");
    let r = parse_rhythm(&r, &["weekly"]).unwrap();
    assert_eq!(r.describe(), "weekly");
    let r = parse_rhythm(&weekly, &["1x"]).unwrap();
    assert!(r.shift_starts.is_empty());
    assert!(parse_rhythm(&weekly, &["9x"]).is_err());
    assert!(parse_rhythm(&weekly, &["often"]).is_err());
}

#[test]
fn a_calendar_feed_has_one_event_per_shift_with_its_own_dates() {
    let (state, _kitchen, bathroom, _ids) = kitchen_and_bathroom();
    let (y, w) = current_iso_week();
    let who = holder(&state, &bathroom, Turn::new(y, w, 1));
    let snapshot = crate::schedule::build_schedule(&state, 1);
    let mine: Vec<_> = snapshot
        .for_person(&who)
        .into_iter()
        .filter(|a| a.group_id == bathroom.id)
        .collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].shift_label.as_deref(), Some("Thu–Sun"));
    let ics = crate::ical::render_ics(&snapshot, &who);
    let thursday = Turn::new(y, w, 1).dates(&bathroom.rhythm).0;
    assert!(
        ics.contains(&format!("DTSTART;VALUE=DATE:{}", thursday.format("%Y%m%d"))),
        "{ics}"
    );
    assert!(ics.contains("Bathroom (Thu–Sun)"), "{ics}");
}

#[test]
fn stats_count_turns_so_a_twice_weekly_group_owes_twice_the_duties() {
    let (mut state, kitchen, bathroom, _ids) = kitchen_and_bathroom();
    // Pretend tracking began three weeks ago; those turns are over.
    state.created_at = Some(Utc::now() - chrono::Duration::weeks(3));
    // Shifts of this week that already ended count too (Mon–Wed from Thursday on).
    let (y, w) = current_iso_week();
    let over_now = state
        .turns_in_week(&bathroom, y, w)
        .into_iter()
        .filter(|t| state.turn_over(&bathroom, *t))
        .count();
    assert_eq!(state.closed_turns(&kitchen).len(), 3);
    assert_eq!(state.closed_turns(&bathroom).len(), 6 + over_now);
    let fairness = analytics::fairness_report(&state, &bathroom.id).unwrap();
    assert_eq!(fairness.due_weeks as usize, 6 + over_now);
    assert!((fairness.entries[0].expected - (6 + over_now) as f64 / 4.0).abs() < 1e-9);
    let model = analytics::group_load_model(&bathroom);
    assert!(
        (model.assignments_per_year - 26.0).abs() < 1e-9,
        "{}",
        model.assignments_per_year
    );
}

// ── !cleaning person ──────────────────────────────────────────────────────

/// Two groups: Floor (mia, Dan, alex) and Kitchen (mia, Alex), four weeks
/// frozen from the current one, plus Dan holding an imported Kitchen week
/// although he isn't a Kitchen member. `alex` (Matrix) and `Alex` (no
/// Matrix) share a display name; Zoe is in no group at all.
fn cleaning_person_ctx() -> (BotContext, PathBuf, OwnedUserId, Vec<(i32, u32)>) {
    let mia = Person::new_matrix("@mia:example.org");
    let dan = Person::new_named("Dan");
    let alex_m = Person::new_matrix("@alex:example.org");
    let alex_n = Person::new_named("Alex");
    let zoe = Person::new_named("Zoe");
    let mut floor = CleaningGroup::new("Floor");
    floor.member_ids = vec![mia.id.clone(), dan.id.clone(), alex_m.id.clone()];
    let mut kitchen = CleaningGroup::new("Kitchen");
    let kitchen_id = kitchen.id.clone();
    kitchen.member_ids = vec![mia.id.clone(), alex_n.id.clone()];
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    let dan_id = dan.id.clone();
    state.persons = vec![mia, dan, alex_m, alex_n, zoe];
    state.cleaning_groups = vec![floor, kitchen];
    // Floor: mia, Dan, alex, mia · Kitchen: mia, Alex, mia, Alex
    seed_materialized_weeks(&mut state, 4);
    let (y, w) = current_iso_week();
    let weeks: Vec<(i32, u32)> = (0..6).map(|i| add_weeks(y, w, i)).collect();
    state
        .apply_event(DomainEvent::SlotAssigned {
            group_id: kitchen_id,
            slot_index: 0,
            iso_year: weeks[3].0,
            iso_week: weeks[3].1,
            shift: 0,
            person_id: Some(dan_id),
            source: AssignmentSource::Import,
            actor_id: None,
            previous_person_id: None,
        })
        .unwrap();
    let (ctx, path, _) = test_context_with_horizon(state, 4);
    let viewer = OwnedUserId::try_from("@someone:example.org").unwrap();
    (ctx, path, viewer, weeks)
}

#[tokio::test]
async fn cleaning_person_shows_a_matrix_participant_across_groups() {
    let (ctx, path, viewer, weeks) = cleaning_person_ctx();
    // No admin needed; full Matrix ID and display name find the same person.
    let by_mxid = cmd_cleaning_person(&ctx, &viewer, &["@mia:example.org", "3"])
        .await
        .unwrap()
        .unwrap();
    let by_name = cmd_cleaning_person(&ctx, &viewer, &["MIA", "3"])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_mxid, by_name);
    let lines: Vec<&str> = by_mxid.lines().collect();
    assert!(lines[0].contains("Upcoming turns for mia"), "{by_mxid}");
    assert!(
        lines[1].starts_with(&format!("• week {} ", weeks[0].1))
            && lines[1].ends_with("Floor · now ⚠️"),
        "{by_mxid}"
    );
    assert!(
        lines[2].starts_with(&format!("• week {} ", weeks[0].1))
            && lines[2].ends_with("Kitchen · now ⚠️"),
        "{by_mxid}"
    );
    assert!(
        lines[3].starts_with(&format!("• week {} ", weeks[2].1)) && lines[3].contains("Kitchen"),
        "{by_mxid}"
    );
    assert_eq!(lines.len(), 4, "{by_mxid}");
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn cleaning_person_shows_a_non_matrix_participant_with_pinned_turns() {
    let (ctx, path, viewer, weeks) = cleaning_person_ctx();
    let reply = cmd_cleaning_person(&ctx, &viewer, &["dan", "2"])
        .await
        .unwrap()
        .unwrap();
    let lines: Vec<&str> = reply.lines().collect();
    assert!(lines[0].contains("Upcoming turns for Dan"), "{reply}");
    assert!(
        lines[1].starts_with(&format!("• week {} ", weeks[1].1)) && lines[1].ends_with("· Floor"),
        "his rotation turn: {reply}"
    );
    assert!(
        lines[2].starts_with(&format!("• week {} ", weeks[3].1))
            && lines[2].ends_with("Kitchen (imported)"),
        "an imported turn outside his own group: {reply}"
    );
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn cleaning_person_reports_an_ambiguous_name_instead_of_guessing() {
    let (ctx, path, viewer, weeks) = cleaning_person_ctx();
    let reply = cmd_cleaning_person(&ctx, &viewer, &["Alex"])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reply,
        "«Alex» matches 2 people: alex (@alex:example.org), Alex (no Matrix) — use the Matrix ID instead."
    );
    // The Matrix ID settles it.
    let alex = cmd_cleaning_person(&ctx, &viewer, &["@alex:example.org", "1"])
        .await
        .unwrap()
        .unwrap();
    assert!(alex.contains(&format!("• week {} ", weeks[2].1)), "{alex}");
    assert!(alex.contains("Floor"), "{alex}");
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn cleaning_person_without_upcoming_turns_or_name_says_so() {
    let (ctx, path, viewer, _) = cleaning_person_ctx();
    let zoe = cmd_cleaning_person(&ctx, &viewer, &["Zoe"])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(zoe, "📅 No upcoming turns for Zoe in the next two years.");
    let unknown = cmd_cleaning_person(&ctx, &viewer, &["Nobody"])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unknown, "Nobody is not registered.");
    let usage = cmd_cleaning_person(&ctx, &viewer, &[])
        .await
        .unwrap()
        .unwrap();
    assert!(usage.starts_with("Usage: !cleaning person"), "{usage}");
    let _ = tokio::fs::remove_file(path).await;
}

#[test]
fn cleaning_person_takes_multi_word_names_without_quotes() {
    assert_eq!(
        crate::commands::normalize_args(
            &State::default(),
            "!cleaning",
            &["person", "Mary", "Ann", "2"]
        ),
        ["person", "Mary Ann", "2"]
    );
    assert_eq!(
        crate::commands::normalize_args(&State::default(), "!cleaning", &["person", "Mary", "Ann"]),
        ["person", "Mary Ann"]
    );
}

// ── Person lookup: ambiguity is reported, never guessed ──────────────────────

#[tokio::test]
async fn next_finds_a_person_by_unique_display_name_or_matrix_id() {
    let (ctx, path, viewer, weeks) = cleaning_person_ctx();
    let by_name = cmd_next(&ctx, &viewer, &["Mia"]).await.unwrap().unwrap();
    let by_mxid = cmd_next(&ctx, &viewer, &["@mia:example.org"])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_name, by_mxid);
    assert!(
        by_name.starts_with(&format!("📅 Next turn for mia: **week {} ", weeks[0].1)),
        "{by_name}"
    );
    assert!(by_name.ends_with("· Floor, Kitchen"), "{by_name}");

    // A Matrix ID picks its owner even though the display name collides.
    let alex = cmd_next(&ctx, &viewer, &["@alex:example.org"])
        .await
        .unwrap()
        .unwrap();
    assert!(
        alex.starts_with(&format!("📅 Next turn for alex: **week {} ", weeks[2].1)),
        "{alex}"
    );
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn next_finds_a_non_matrix_participant_by_name() {
    let (ctx, path, viewer, weeks) = cleaning_person_ctx();
    let reply = cmd_next(&ctx, &viewer, &["dan"]).await.unwrap().unwrap();
    assert!(
        reply.starts_with(&format!("📅 Next turn for Dan: **week {} ", weeks[1].1)),
        "{reply}"
    );
    assert!(reply.ends_with("· Floor"), "{reply}");
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn next_reports_an_ambiguous_display_name() {
    let (ctx, path, viewer, _) = cleaning_person_ctx();
    let reply = cmd_next(&ctx, &viewer, &["Alex"]).await.unwrap().unwrap();
    assert_eq!(
        reply,
        "«Alex» matches 2 people: alex (@alex:example.org), Alex (no Matrix) — use the Matrix ID instead."
    );
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn admin_commands_refuse_an_ambiguous_person_instead_of_picking_one() {
    let (ctx, path, _, _) = cleaning_person_ctx();
    let admin = OwnedUserId::try_from("@admin:example.org").unwrap();
    let before = serde_json::to_string(&*ctx.state.lock().await).unwrap();
    for reply in [
        cmd_assign(&ctx, &admin, &["Floor", "Alex"]).await,
        cmd_removeperson(&ctx, &admin, &["Alex", "Kitchen"]).await,
        cmd_absent(&ctx, &admin, &["Alex"]).await,
        cmd_stats(&ctx, &["Alex"]).await,
    ] {
        let reply = reply.unwrap().unwrap();
        assert!(reply.starts_with("«Alex» matches 2 people"), "{reply}");
    }
    assert_eq!(
        serde_json::to_string(&*ctx.state.lock().await).unwrap(),
        before,
        "nothing may change on an ambiguous name"
    );
    let _ = tokio::fs::remove_file(path).await;
}
