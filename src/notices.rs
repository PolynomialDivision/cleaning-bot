//! Change notices: when who holds a turn coming up changes (an admin's
//! assignment, someone moving out, a swap, a new rhythm, anything), one
//! message in the cleaning room tells the new holder and the previous one.
//!
//! `State::told_holders` remembers who was last told about each slot: the
//! weekly plan (`scheduler`) and the swap and takeover messages record what
//! they told. Each scheduler tick compares it with the plan as it is now.
//!
//! ```text
//! 🔀 **Plan changed**
//! 🧹 Kitchen · 13 – 19 Oct: @alice ➡️ @bob
//! ```

use chrono::{Datelike, NaiveDate, Timelike};
use matrix_sdk::{ruma::OwnedTransactionId, Room};
use mxbot_common::{config::TimeOfDay, matrix_sdk};
use tracing::info;

use crate::{
    commands::Duty,
    domain::{CleaningGroup, GroupId, PersonId},
    rhythm::Turn,
    state::{slot_key, State},
    BotContext,
};

/// How far ahead changes are told: turns starting within two weeks.
const LOOKAHEAD_DAYS: i64 = 13;
/// No notices from this hour on (local time) until `reminder_time` the next day.
const QUIET_FROM_HOUR: u32 = 21;

/// Whether a change to one slot of `turn` is told on `today`: the group is
/// active, the slot isn't done, and the turn ends today or later and starts
/// within the next two weeks.
pub(crate) fn watched(
    state: &State,
    group: &CleaningGroup,
    slot_index: usize,
    turn: Turn,
    today: NaiveDate,
) -> bool {
    let (start, end) = turn.dates(&group.rhythm);
    group.is_active
        && end >= today
        && start <= today + chrono::Duration::days(LOOKAHEAD_DAYS)
        && !state.is_turn_slot_done(group, slot_index, turn)
}

/// Whether a change notice is still to come for this slot: it is watched and
/// the person last told about it isn't `holder`. A message showing `holder`
/// there (the plan) leaves telling to the notice.
pub(crate) fn notice_pending(
    state: &State,
    group_id: &str,
    slot_index: usize,
    turn: Turn,
    holder: Option<&PersonId>,
    today: NaiveDate,
) -> bool {
    let Some(told) = state
        .told_holders
        .get(&slot_key(group_id, slot_index, turn))
    else {
        return false;
    };
    told.as_ref() != holder
        && state
            .cleaning_groups
            .iter()
            .find(|g| g.id == group_id)
            .is_some_and(|g| watched(state, g, slot_index, turn, today))
}

/// One slot whose holder changed since they were told.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Change {
    pub(crate) group_id: GroupId,
    pub(crate) slot_index: usize,
    pub(crate) turn: Turn,
    pub(crate) now: Option<PersonId>,
    pub(crate) was: Option<PersonId>,
}

/// What a look at the watched slots found.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Scan {
    /// Slots nobody was told about yet (new in the window, or from before
    /// notices): recorded without a message.
    pub(crate) first_seen: Vec<Change>,
    /// Slots whose holder changed, and new slots of a turn already told
    /// about (a new rhythm's second shift, a group's new slot): a notice.
    pub(crate) changes: Vec<Change>,
}

/// Compare every watched slot's holder with who was told, on `today`.
/// Changes come in turn order (then group, then slot).
pub(crate) fn scan(state: &State, today: NaiveDate) -> Scan {
    let last = today + chrono::Duration::days(LOOKAHEAD_DAYS);
    let weeks = |d: NaiveDate| (d.iso_week().year(), d.iso_week().week());
    let mut found = Vec::new();
    for group in state.cleaning_groups.iter().filter(|g| g.is_active) {
        for turn in state.turns_between(group, weeks(today), weeks(last)) {
            for slot_index in State::slot_indices(group) {
                if !watched(state, group, slot_index, turn, today) {
                    continue;
                }
                let now = state
                    .slot_assignee(group, slot_index, turn)
                    .map(|p| p.id.clone());
                let told = state
                    .told_holders
                    .get(&slot_key(&group.id, slot_index, turn));
                let change = Change {
                    group_id: group.id.clone(),
                    slot_index,
                    turn,
                    was: told.cloned().flatten(),
                    now,
                };
                match told {
                    None => {
                        let new_slot = change.now.is_some()
                            && known_week(state, group, turn)
                            && turn.dates(&group.rhythm).0 < last;
                        found.push((new_slot, change))
                    }
                    Some(told) if *told != change.now => found.push((true, change)),
                    Some(_) => {}
                }
            }
        }
    }
    let start = |c: &Change| {
        state
            .group_by_id(&c.group_id)
            .map(|g| c.turn.dates(&g.rhythm).0)
    };
    found.sort_by_key(|(_, c)| start(c));
    let mut scan = Scan::default();
    for (changed, change) in found {
        if changed {
            scan.changes.push(change);
        } else {
            scan.first_seen.push(change);
        }
    }
    scan
}

/// Whether someone was told about a slot of `group` in `turn`'s week: a slot
/// without a record there is new (a new shift or slot), not just come into
/// view. (One that came into view today starts on the window's last day.)
fn known_week(state: &State, group: &CleaningGroup, turn: Turn) -> bool {
    state
        .turns_in_week(group, turn.year, turn.week)
        .into_iter()
        .any(|t| {
            State::slot_indices(group)
                .any(|i| state.told_holders.contains_key(&slot_key(&group.id, i, t)))
        })
}

/// Whether a notice may go out at `hour`:`minute` local time: from
/// `reminder_time` until 21:00.
pub(crate) fn may_send(hour: u32, minute: u32, from: &TimeOfDay) -> bool {
    (hour, minute) >= (from.hour, from.minute) && hour < QUIET_FROM_HOUR
}

/// The notice for `changes`, with the Matrix IDs it pings: the new holders
/// and the previous ones still living here (someone who moved out is named,
/// not pinged). `None` without a change to tell.
pub(crate) fn notice_text(state: &State, changes: &[Change]) -> Option<(String, Vec<String>)> {
    let mut lines = vec!["🔀 **Plan changed**".to_owned()];
    let mut mxids = Vec::new();
    let mut who = |id: Option<&PersonId>| -> Option<String> {
        let person = state.person_by_id(id?)?;
        match (&person.matrix_id, person.active) {
            (Some(mxid), true) => {
                mxids.push(mxid.clone());
                Some(mxid.clone())
            }
            _ => Some(person.display_name.clone()),
        }
    };
    for change in changes {
        let Some(group) = state.group_by_id(&change.group_id) else {
            continue;
        };
        let duty = Duty {
            group: group.clone(),
            slot_index: change.slot_index,
            turn: change.turn,
        };
        let was = who(change.was.as_ref());
        let now = who(change.now.as_ref()).unwrap_or_else(|| "nobody yet".to_owned());
        let who = match was {
            Some(was) => format!("{was} ➡️ {now}"),
            None => format!("➡️ {now}"),
        };
        lines.push(format!(
            "🧹 {} · {}: {who}",
            duty.place(),
            crate::trades::when(&duty)
        ));
    }
    if lines.len() == 1 {
        return None;
    }
    mxids.sort();
    mxids.dedup();
    Some((lines.join("\n"), mxids))
}

/// After a notice: its people count as told, unless something else told
/// about a slot meanwhile (its record moved on). A new slot had no record.
pub(crate) fn record_sent(state: &mut State, changes: &[Change]) {
    for c in changes {
        let key = slot_key(&c.group_id, c.slot_index, c.turn);
        if state.told_holders.get(&key).cloned().flatten() == c.was {
            state.mark_told(&c.group_id, c.slot_index, c.turn, c.now.as_ref());
        }
    }
}

/// The transaction ID of the notice for `changes`: the same changes give the
/// same ID, so a notice sent again after a crash shows only once.
fn txn_id(changes: &[Change]) -> OwnedTransactionId {
    let content: Vec<String> = changes
        .iter()
        .map(|c| {
            format!(
                "{}={}<{}",
                slot_key(&c.group_id, c.slot_index, c.turn),
                c.now.as_deref().unwrap_or("-"),
                c.was.as_deref().unwrap_or("-")
            )
        })
        .collect();
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, content.join(",").as_bytes());
    format!("notice-{id}").into()
}

/// Record the slots seen for the first time, and tell the changes when the
/// hour allows (`may_send`).
pub(crate) async fn run(
    ctx: &BotContext,
    room: &Room,
    local_now: chrono::DateTime<chrono_tz::Tz>,
) -> anyhow::Result<()> {
    let today = local_now.date_naive();
    let send_now = may_send(
        local_now.hour(),
        local_now.minute(),
        &ctx.config.schedule.reminder_time,
    );
    let (changes, text, mxids) = {
        let mut state = ctx.state.lock().await;
        let scan = scan(&state, today);
        for c in &scan.first_seen {
            state.mark_told(&c.group_id, c.slot_index, c.turn, c.now.as_ref());
        }
        if !scan.first_seen.is_empty() {
            state.save(&ctx.state_path).await?;
        }
        if !send_now {
            return Ok(());
        }
        let Some((text, mxids)) = notice_text(&state, &scan.changes) else {
            return Ok(());
        };
        (scan.changes, text, mxids)
    };
    let content = crate::scheduler::mention_message(ctx, &text, &mxids, room).await;
    mxbot_common::send::send_with_retry_txn(room, content, &txn_id(&changes))
        .await
        .map_err(|e| anyhow::anyhow!("send failed: {e}"))?;
    let mut state = ctx.state.lock().await;
    record_sent(&mut state, &changes);
    state.save(&ctx.state_path).await?;
    info!("Sent a change notice for {} turn(s)", changes.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CleaningGroup, Person, SlotAssignment};

    /// Kitchen (whole weeks: Alice) and Floor (two slots: Bob and Carol),
    /// frozen from last week to three weeks ahead.
    fn house(today: NaiveDate) -> State {
        let mut state = State::default();
        let people =
            ["alice", "bob", "carol"].map(|n| Person::new_matrix(&format!("@{n}:example.org")));
        let mut kitchen = CleaningGroup::new("Kitchen");
        let mut floor = CleaningGroup::new("Floor");
        floor.slots = vec![
            crate::domain::CleaningSlot::new("Scharni"),
            crate::domain::CleaningSlot::new("Colbe"),
        ];
        let ids: Vec<_> = people.iter().map(|p| p.id.clone()).collect();
        kitchen.member_ids = ids.clone();
        floor.member_ids = ids.clone();
        let week = (today.iso_week().year(), today.iso_week().week());
        for offset in -1..4 {
            let (y, w) = crate::state::add_weeks(week.0, week.1, offset);
            for (group, slot, who) in [(&kitchen, 0, 0), (&floor, 0, 1), (&floor, 1, 2)] {
                state.slot_assignments.push(SlotAssignment {
                    group_id: group.id.clone(),
                    slot_index: slot,
                    iso_year: y,
                    iso_week: w,
                    shift: 0,
                    person_id: Some(ids[who].clone()),
                    source: Default::default(),
                });
            }
        }
        state.persons = people.to_vec();
        state.cleaning_groups = vec![kitchen, floor];
        state
    }

    fn reassign(state: &mut State, group: usize, slot: usize, turn: Turn, to: Option<usize>) {
        let group_id = state.cleaning_groups[group].id.clone();
        let person = to.map(|i| state.persons[i].id.clone());
        let a = state
            .slot_assignments
            .iter_mut()
            .find(|a| {
                a.group_id == group_id
                    && a.slot_index == slot
                    && (a.iso_year, a.iso_week, a.shift) == (turn.year, turn.week, turn.shift)
            })
            .unwrap();
        a.person_id = person;
    }

    fn record(state: &mut State, scan: &Scan) {
        for c in scan.first_seen.iter().chain(&scan.changes) {
            state.mark_told(&c.group_id, c.slot_index, c.turn, c.now.as_ref());
        }
    }

    // A Wednesday: this week's turns and the next two weeks' are watched.
    fn wednesday() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 7).unwrap()
    }

    #[test]
    fn the_first_look_records_silently_and_only_a_change_is_told() {
        let today = wednesday();
        let mut state = house(today);
        // First run after the rollout: everything recorded, nothing told.
        let first = scan(&state, today);
        assert!(first.changes.is_empty());
        // 2 groups' slots (1 + 2) × this week and the next two (starting by
        // the 20th): last week is over, the 4th week starts too late.
        assert_eq!(first.first_seen.len(), 9);
        record(&mut state, &first);
        assert_eq!(scan(&state, today), Scan::default());

        // Next week's Kitchen goes from Alice to Bob.
        let next = Turn::new(2026, 42, 0);
        reassign(&mut state, 0, 0, next, Some(1));
        let found = scan(&state, today);
        assert!(found.first_seen.is_empty());
        let (alice, bob) = (state.persons[0].id.clone(), state.persons[1].id.clone());
        assert_eq!(
            found.changes,
            [Change {
                group_id: state.cleaning_groups[0].id.clone(),
                slot_index: 0,
                turn: next,
                now: Some(bob),
                was: Some(alice),
            }]
        );
        // Told: no second notice.
        record(&mut state, &found);
        assert_eq!(scan(&state, today), Scan::default());

        // A turn entering the window is recorded silently, a week later.
        let later = today + chrono::Duration::days(7);
        let entering = scan(&state, later);
        assert!(entering.changes.is_empty());
        assert_eq!(entering.first_seen.len(), 3);
        assert!(entering.first_seen.iter().all(|c| c.turn.week == 44));
    }

    #[test]
    fn several_changes_make_one_notice_pinging_both_sides() {
        let today = wednesday();
        let mut state = house(today);
        let first = scan(&state, today);
        record(&mut state, &first);
        // Bob and Carol swap next week's Floor; nobody holds this week's Kitchen any more.
        let next = Turn::new(2026, 42, 0);
        reassign(&mut state, 1, 0, next, Some(2));
        reassign(&mut state, 1, 1, next, Some(1));
        reassign(&mut state, 0, 0, Turn::new(2026, 41, 0), None);
        let found = scan(&state, today);
        let (text, mxids) = notice_text(&state, &found.changes).unwrap();
        assert_eq!(
            text,
            "🔀 **Plan changed**\n\
             🧹 Kitchen · 5 – 11 Oct: @alice:example.org ➡️ nobody yet\n\
             🧹 Floor / Scharni · 12 – 18 Oct: @bob:example.org ➡️ @carol:example.org\n\
             🧹 Floor / Colbe · 12 – 18 Oct: @carol:example.org ➡️ @bob:example.org"
        );
        assert_eq!(
            mxids,
            [
                "@alice:example.org",
                "@bob:example.org",
                "@carol:example.org"
            ]
        );
    }

    #[test]
    fn someone_who_moved_out_is_named_not_pinged() {
        let today = wednesday();
        let mut state = house(today);
        let first = scan(&state, today);
        record(&mut state, &first);
        let next = Turn::new(2026, 42, 0);
        reassign(&mut state, 0, 0, next, Some(2));
        state.persons[0].active = false;
        state.persons[0].display_name = "Alice".into();
        let found = scan(&state, today);
        let (text, mxids) = notice_text(&state, &found.changes).unwrap();
        assert_eq!(
            text,
            "🔀 **Plan changed**\n🧹 Kitchen · 12 – 18 Oct: Alice ➡️ @carol:example.org"
        );
        assert_eq!(mxids, ["@carol:example.org"]);
        // Someone no longer in the plan at all: just who has it now.
        state.persons.remove(0);
        let (text, _) = notice_text(&state, &found.changes).unwrap();
        assert!(text.ends_with(": ➡️ @carol:example.org"), "{text}");
    }

    #[test]
    fn done_turns_and_paused_groups_are_not_watched() {
        let today = wednesday();
        let mut state = house(today);
        let first = scan(&state, today);
        record(&mut state, &first);
        let this = Turn::new(2026, 41, 0);
        let kitchen = state.cleaning_groups[0].clone();
        state
            .apply_event(crate::analytics::DomainEvent::CleaningCompleted {
                completed_on: None,
                group_id: kitchen.id.clone(),
                slot_id: None,
                person_id: state.persons[0].id.clone(),
                responsible_person_ids: vec![],
                iso_year: this.year,
                iso_week: this.week,
                shift: 0,
            })
            .unwrap();
        reassign(&mut state, 0, 0, this, Some(1));
        assert!(scan(&state, today).changes.is_empty());
        assert!(!notice_pending(
            &state,
            &kitchen.id,
            0,
            this,
            Some(&state.persons[1].id),
            today
        ));

        let next = Turn::new(2026, 42, 0);
        reassign(&mut state, 0, 0, next, Some(1));
        assert!(notice_pending(
            &state,
            &kitchen.id,
            0,
            next,
            Some(&state.persons[1].id),
            today
        ));
        state.cleaning_groups[0].is_active = false;
        assert!(scan(&state, today).changes.is_empty());
    }

    #[test]
    fn notices_wait_for_the_day_and_repeat_with_the_same_id() {
        let nine = TimeOfDay::new(9, 0);
        assert!(!may_send(8, 59, &nine));
        assert!(may_send(9, 0, &nine));
        assert!(may_send(20, 59, &nine));
        assert!(!may_send(21, 0, &nine));
        assert!(!may_send(23, 30, &nine));

        let today = wednesday();
        let mut state = house(today);
        let first = scan(&state, today);
        record(&mut state, &first);
        reassign(&mut state, 0, 0, Turn::new(2026, 42, 0), Some(1));
        // Not sent yet (quiet hours, a crash): the same notice, the same ID.
        let a = scan(&state, today).changes;
        let b = scan(&state, today).changes;
        assert_eq!(txn_id(&a), txn_id(&b));
        // Another change makes another notice.
        reassign(&mut state, 1, 0, Turn::new(2026, 42, 0), Some(0));
        assert_ne!(txn_id(&a), txn_id(&scan(&state, today).changes));
        assert!(notice_text(&state, &[]).is_none());
    }

    #[test]
    fn old_records_are_pruned() {
        let mut state = house(wednesday());
        let kitchen = state.cleaning_groups[0].id.clone();
        for week in [40, 41, 42] {
            state.mark_told(&kitchen, 0, Turn::new(2026, week, 0), None);
        }
        state
            .told_holders
            .insert(slot_key("gone", 0, Turn::new(2026, 42, 0)), None);
        state.told_holders.insert("not a key".into(), None);
        // Monday of week 42: week 41 ended yesterday and stays a day longer.
        let monday = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
        assert!(state.prune_told_holders(monday));
        let left: Vec<_> = state.told_holders.keys().cloned().collect();
        assert_eq!(
            left,
            [41, 42].map(|w| slot_key(&kitchen, 0, Turn::new(2026, w, 0)))
        );
        assert!(!state.prune_told_holders(monday));

        let kind = crate::state::ReminderKind::Initial;
        for (group, week) in [("*", 32), ("*", 33), ("g", 32), ("g", 41)] {
            state.mark_reminder_sent(group, 2026, week, 0, kind.clone());
        }
        assert!(state.prune_sent_reminders((2026, 41)));
        let left: Vec<_> = state
            .sent_reminders
            .iter()
            .map(|r| (r.group_id.as_str(), r.iso_week))
            .collect();
        assert_eq!(left, [("*", 33), ("g", 41)]);
        assert!(!state.prune_sent_reminders((2026, 41)));
    }

    #[test]
    fn a_new_slot_of_a_known_week_is_told_but_one_coming_into_view_is_not() {
        let today = wednesday();
        let mut state = house(today);
        let first = scan(&state, today);
        record(&mut state, &first);
        // Floor gets a third slot, Alice on it.
        state.cleaning_groups[1]
            .slots
            .push(crate::domain::CleaningSlot::new("Attic"));
        let floor = state.cleaning_groups[1].clone();
        for week in 41..=44 {
            state.slot_assignments.push(SlotAssignment {
                group_id: floor.id.clone(),
                slot_index: 2,
                iso_year: 2026,
                iso_week: week,
                shift: 0,
                person_id: Some(state.persons[0].id.clone()),
                source: Default::default(),
            });
        }
        let found = scan(&state, today);
        assert!(found.first_seen.is_empty(), "{:?}", found.first_seen);
        assert_eq!(found.changes.len(), 3);
        assert!(found
            .changes
            .iter()
            .all(|c| c.slot_index == 2 && c.was.is_none()));
        let (text, mxids) = notice_text(&state, &found.changes[1..2]).unwrap();
        assert_eq!(
            text,
            "🔀 **Plan changed**\n🧹 Floor / Attic · 12 – 18 Oct: ➡️ @alice:example.org"
        );
        assert_eq!(mxids, ["@alice:example.org"]);
        // Told: the next look finds nothing (a new slot had no record).
        record_sent(&mut state, &found.changes);
        assert_eq!(scan(&state, today), Scan::default());

        // A week later, week 44 comes into view (starting on the window's
        // last day): recorded, not told, though its first slots are new too.
        let later = today + chrono::Duration::days(6);
        let entering = scan(&state, later);
        assert!(entering.changes.is_empty(), "{:?}", entering.changes);
        assert_eq!(entering.first_seen.len(), 4);
    }
}
