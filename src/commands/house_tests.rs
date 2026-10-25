//! The staircase: a house-wide group (`house`) with two people per place.

use super::*;
use crate::{domain::Person, state::State};

/// Two floors of three Matrix residents each, Lea in no group, and an empty
/// Staircase, every 2 weeks.
async fn house() -> (BotContext, std::path::PathBuf, OwnedUserId) {
    let mut state = State::default();
    state.created_at = Some(Utc::now());
    for (floor, people) in [
        ("1st Floor", ["ana", "bo", "cy"]),
        ("2nd Floor", ["di", "ed", "fe"]),
    ] {
        let mut group = CleaningGroup::new(floor);
        for name in people {
            let person = Person::new_matrix(&format!("@{name}:example.org"));
            group.member_ids.push(person.id.clone());
            state.persons.push(person);
        }
        state.cleaning_groups.push(group);
    }
    state.persons.push(Person::new_matrix("@lea:example.org"));
    let mut stairs = CleaningGroup::new("Staircase");
    stairs.rhythm.every_weeks = Some(2);
    state.cleaning_groups.push(stairs);
    for ev in resolver::materialize(&state, 8) {
        state.apply_event(ev).unwrap();
    }
    let (ctx, path, admin) = super::tests::test_context_with_horizon(state, 8);
    let reply = cmd_groups_house(&ctx, &admin, &["Staircase", "on"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("6 joined"), "{reply}");
    (ctx, path, admin)
}

async fn staircase_members(ctx: &BotContext) -> Vec<String> {
    let state = ctx.state.lock().await;
    let mut names: Vec<String> = state
        .members_of(state.group_by_name("Staircase").unwrap())
        .iter()
        .map(|p| p.display_name.clone())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn everyone_in_a_group_is_on_the_staircase_and_follows_moves() {
    let (ctx, path, admin) = house().await;
    assert_eq!(
        staircase_members(&ctx).await,
        ["ana", "bo", "cy", "di", "ed", "fe"]
    );
    {
        let state = ctx.state.lock().await;
        let stairs = state.group_by_name("Staircase").unwrap();
        assert!(state
            .slot_assignments
            .iter()
            .any(|a| a.group_id == stairs.id));
    }

    // Lea moves in: on a floor, so on the staircase too.
    cmd_member_add(&ctx, &admin, &["@lea:example.org", "1st Floor"])
        .await
        .unwrap();
    assert!(staircase_members(&ctx).await.contains(&"lea".to_owned()));

    // Fe moves out: off her last floor, off the staircase with the next
    // roll of the horizon, turns handed on.
    cmd_member_remove(&ctx, &admin, &["@fe:example.org", "2nd Floor"])
        .await
        .unwrap();
    crate::scheduler::roll_planning_horizon(&ctx).await.unwrap();
    assert!(!staircase_members(&ctx).await.contains(&"fe".to_owned()));
    {
        let state = ctx.state.lock().await;
        let fe = state
            .person_by_matrix_id("@fe:example.org")
            .unwrap()
            .id
            .clone();
        let stairs = state.group_by_name("Staircase").unwrap();
        let (y, w) = current_iso_week();
        assert!(!state
            .slot_assignments
            .iter()
            .any(|a| a.group_id == stairs.id
                && (a.iso_year, a.iso_week) > (y, w)
                && a.person_id.as_ref() == Some(&fe)));
        assert!(crate::validate::validate_state(&state)
            .warnings
            .iter()
            .all(|w| !w.contains("House-wide")));
    }
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn nobody_leaves_the_staircase_alone_but_an_admin_can_excuse_them() {
    let (ctx, path, admin) = house().await;
    {
        let mut state = ctx.state.lock().await;
        let stairs = state.group_by_name("Staircase").unwrap().id.clone();
        let refused = leave_group(&ctx, &mut state, "@bo:example.org", &stairs).unwrap();
        assert!(refused.unwrap_err().contains("whole house"));
        let refused = join_group(&ctx, &mut state, "@lea:example.org", &stairs).unwrap();
        assert!(refused.is_err());
    }

    // Excused, and it stays so when the house changes.
    cmd_member_remove(&ctx, &admin, &["@bo:example.org", "Staircase"])
        .await
        .unwrap();
    cmd_member_add(&ctx, &admin, &["@lea:example.org", "2nd Floor"])
        .await
        .unwrap();
    assert_eq!(
        staircase_members(&ctx).await,
        ["ana", "cy", "di", "ed", "fe", "lea"]
    );
    let overview = cmd_groups(&ctx, None).await.unwrap().unwrap();
    assert!(
        overview.contains("🏠 everyone (6) · excused: bo"),
        "{overview}"
    );

    // Back again, and someone in no group added by hand.
    cmd_member_add(&ctx, &admin, &["@bo:example.org", "Staircase"])
        .await
        .unwrap();
    {
        let mut state = ctx.state.lock().await;
        state.persons.push(Person::new_matrix("@max:example.org"));
    }
    cmd_member_add(&ctx, &admin, &["@max:example.org", "Staircase"])
        .await
        .unwrap();
    crate::scheduler::roll_planning_horizon(&ctx).await.unwrap();
    assert_eq!(
        staircase_members(&ctx).await,
        ["ana", "bo", "cy", "di", "ed", "fe", "lea", "max"]
    );

    // Off: an ordinary group with the members it has.
    cmd_groups_house(&ctx, &admin, &["Staircase", "off"])
        .await
        .unwrap();
    cmd_member_remove(&ctx, &admin, &["@max:example.org", "Staircase"])
        .await
        .unwrap();
    cmd_member_add(&ctx, &admin, &["@max:example.org", "1st Floor"])
        .await
        .unwrap();
    assert!(!staircase_members(&ctx).await.contains(&"max".to_owned()));
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn two_people_per_staircase_share_a_place_in_the_plan() {
    let (ctx, path, admin) = house().await;
    for place in ["Scharni", "Colbe"] {
        let reply = cmd_addslot(&ctx, &admin, &["Staircase", &format!("{place} x2")])
            .await
            .unwrap()
            .unwrap();
        assert!(reply.contains(&format!("{place} 1, {place} 2")), "{reply}");
        cmd_addroom(
            &ctx,
            &admin,
            &["Staircase", place, &format!("Stairs {place}")],
        )
        .await
        .unwrap();
    }
    let taken = cmd_addslot(&ctx, &admin, &["Staircase", "Colbe 2"])
        .await
        .unwrap()
        .unwrap();
    assert!(taken.contains("already exists"), "{taken}");
    cmd_setroomweight(&ctx, &admin, &["Staircase", "Stairs Colbe", "2"])
        .await
        .unwrap();
    cmd_resetplan(&ctx, &admin, &["Staircase"]).await.unwrap();

    let state = ctx.state.lock().await;
    let stairs = state.group_by_name("Staircase").unwrap().clone();
    let names: Vec<(&str, Option<&str>, &[String])> = stairs
        .slots
        .iter()
        .map(|s| (s.name.as_str(), s.place.as_deref(), s.room_names.as_slice()))
        .collect();
    assert_eq!(names.len(), 4);
    assert_eq!(names[1].0, "Scharni 2");
    assert_eq!(names[1].1, Some("Scharni"));
    assert_eq!(names[3].2, ["Stairs Colbe".to_owned()]);
    assert_eq!(stairs.slots[3].room_weights.get("Stairs Colbe"), Some(&2.0));
    assert_eq!(stairs.duty_load(3), 2.0);

    // The next due week (the running one stays unassigned for a new
    // group): four different people, two at each place.
    let (y, w) = current_iso_week();
    let (y, w) = state.next_due_week(&stairs, crate::state::add_weeks(y, w, 1));
    let (text, mentioned) =
        crate::scheduler::build_weekly_plan(&state, y, w, std::slice::from_ref(&stairs));
    let at = |place: &str| {
        text.lines()
            .filter(|l| l.contains(&format!("{place}: ")))
            .count()
    };
    assert_eq!((at("Scharni"), at("Colbe")), (2, 2), "{text}");
    assert_eq!(text.matches("Stairs Scharni").count(), 1, "{text}");
    assert!(!text.contains("Scharni 1"), "{text}");
    assert_eq!(mentioned.len(), 4, "{text}");

    // Each one's calendar names the place and who they clean it with.
    let snapshot = crate::schedule::build_schedule_from(&state, (y, w), 1);
    let first = snapshot
        .assignments
        .iter()
        .find(|a| a.group_id == stairs.id)
        .unwrap();
    let partner = &snapshot.crew(first)[0];
    let ics = crate::ical::render_ics(&snapshot, &first.assignee.as_ref().unwrap().id);
    let ics = ics.replace("\r\n ", "");
    assert!(ics.contains("Task: Scharni"), "{ics}");
    assert!(
        ics.contains(&format!("Together with: {}", partner.assignee_name())),
        "{ics}"
    );

    // Selectors offer the floors, not the staircase.
    let selector = crate::onboarding::new_selector(&state, "@lea:example.org", false);
    assert_eq!(selector.group_ids.len(), 2);
    assert!(selector
        .rendered
        .contains("🏠 **Staircase**: the whole house"));
    drop(state);
    let _ = tokio::fs::remove_file(path).await;
}

/// Who holds the staircase's turns, in order.
async fn staircase_plan(ctx: &BotContext) -> Vec<(i32, u32, usize, Option<String>)> {
    let state = ctx.state.lock().await;
    let stairs = state.group_by_name("Staircase").unwrap().id.clone();
    let mut plan: Vec<_> = state
        .slot_assignments
        .iter()
        .filter(|a| a.group_id == stairs)
        .map(|a| (a.iso_year, a.iso_week, a.slot_index, a.person_id.clone()))
        .collect();
    plan.sort();
    plan
}

/// Someone on the 1st Floor without an open turn there this week (so they
/// may leave it): their Matrix ID and person.
async fn free_to_move(ctx: &BotContext) -> (String, PersonId) {
    let state = ctx.state.lock().await;
    let floor = state.group_by_name("1st Floor").unwrap();
    state
        .members_of(floor)
        .iter()
        .find(|p| current_open_assignments(&state, &floor.id, &p.id).is_empty())
        .map(|p| (p.matrix_id.clone().unwrap(), p.id.clone()))
        .unwrap()
}

#[tokio::test]
async fn moving_floors_leaves_the_staircase_alone() {
    let (ctx, path, admin) = house().await;
    crate::scheduler::roll_planning_horizon(&ctx).await.unwrap();
    let before = staircase_plan(&ctx).await;
    // Out of one floor, into the other: never in no group at a roll.
    let (mover, _) = free_to_move(&ctx).await;
    cmd_member_remove(&ctx, &admin, &[&mover, "1st Floor"])
        .await
        .unwrap();
    cmd_member_add(&ctx, &admin, &[&mover, "2nd Floor"])
        .await
        .unwrap();
    crate::scheduler::roll_planning_horizon(&ctx).await.unwrap();
    assert_eq!(staircase_plan(&ctx).await, before);
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn who_moves_out_keeps_a_running_turn_until_it_is_done() {
    let (ctx, path, admin) = house().await;
    let (y, w) = current_iso_week();
    let (mover, person) = free_to_move(&ctx).await;
    let name = mover[1..mover.find(':').unwrap()].to_owned();
    let stairs = {
        let mut state = ctx.state.lock().await;
        let stairs = state.group_by_name("Staircase").unwrap().id.clone();
        // The new group's running week was left open; give it to them.
        state
            .apply_event(DomainEvent::SlotAssigned {
                group_id: stairs.clone(),
                slot_index: 0,
                iso_year: y,
                iso_week: w,
                shift: 0,
                person_id: Some(person.clone()),
                source: AssignmentSource::Assign,
                actor_id: None,
                previous_person_id: None,
            })
            .unwrap();
        stairs
    };
    let reply = cmd_member_remove(&ctx, &admin, &[&mover, "1st Floor"])
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("Removed"), "{reply}");
    crate::scheduler::roll_planning_horizon(&ctx).await.unwrap();
    assert!(staircase_members(&ctx).await.contains(&name));
    // Done: now they go.
    ctx.state
        .lock()
        .await
        .apply_event(DomainEvent::CleaningCompleted {
            completed_on: None,
            group_id: stairs,
            slot_id: None,
            person_id: person.clone(),
            responsible_person_ids: vec![person],
            iso_year: y,
            iso_week: w,
            shift: 0,
        })
        .unwrap();
    crate::scheduler::roll_planning_horizon(&ctx).await.unwrap();
    assert!(!staircase_members(&ctx).await.contains(&name));
    let _ = tokio::fs::remove_file(path).await;
}
