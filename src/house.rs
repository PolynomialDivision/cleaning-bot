//! House-wide groups: the staircase, which every resident cleans in turn,
//! whatever floor or corridor they live on.
//!
//! A house-wide group is an ordinary group (`CleaningGroup::house_wide`)
//! whose members the bot keeps itself: every **resident**, i.e. every active
//! person in at least one other group (paused ones too: a paused floor still
//! has its people), plus the people an admin added by hand
//! (`house_added`), minus those an admin excused (`house_excused`). Joining a
//! first group makes someone a resident, leaving their last one makes them
//! leave the house-wide groups too, through the same `apply_group_join` /
//! `apply_group_departure` every other membership change takes: the
//! rotation re-plans from its next cycle for a newcomer, a leaver's turns
//! are handed on. Newcomers join right away, after every membership change;
//! leavers go with the next roll of the planning horizon (within half an
//! hour, `Leavers::Go`), so someone moving floors (out of one group, into
//! the next) never drops off the staircase in between, and once their turn
//! of the running week is done or over, so nobody who moved out is still
//! reminded of one.
//!
//! Everything else about the group is the usual: its rhythm (`every 2`),
//! its slots (two people per staircase are two slots at one place,
//! `CleaningSlot::place`), reminders, ✅, 🆘 and swaps. Who gets which week
//! is decided with everyone's other duties in view (`resolver::Workload`).

use crate::{
    analytics::DomainEvent,
    commands::{apply_group_departure, apply_group_join},
    domain::{CleaningGroup, GroupId, PersonId},
    state::State,
    BotContext,
};

/// Who should be in the house-wide `group` now, in the plan's order of people.
pub fn residents(state: &State, group: &CleaningGroup) -> Vec<PersonId> {
    state
        .persons
        .iter()
        .filter(|p| p.active && !group.house_excused.contains(&p.id))
        .filter(|p| {
            group.house_added.contains(&p.id)
                || state
                    .cleaning_groups
                    .iter()
                    .any(|g| !g.house_wide && g.member_ids.contains(&p.id))
        })
        .map(|p| p.id.clone())
        .collect()
}

/// What `sync` changed in one house-wide group.
#[derive(Debug, Default, PartialEq)]
pub struct Change {
    pub group_id: GroupId,
    pub joined: Vec<PersonId>,
    pub left: Vec<PersonId>,
}

/// Whether `sync` also takes leavers out, or only lets newcomers in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leavers {
    Go,
    Stay,
}

/// Bring every active house-wide group's members in line with `residents`:
/// with `Leavers::Go` leavers first (their turns handed on; not while their
/// turn of the running week is open), then newcomers (seated from the next
/// cycle). Who an admin added by hand and is a resident anyway is no longer
/// remembered as added. Returns what changed, nothing for groups in line.
pub(crate) fn sync(
    ctx: &BotContext,
    state: &mut State,
    leavers: Leavers,
) -> anyhow::Result<Vec<Change>> {
    let groups: Vec<GroupId> = state
        .cleaning_groups
        .iter()
        .filter(|g| g.house_wide && g.is_active)
        .map(|g| g.id.clone())
        .collect();
    let mut changes = Vec::new();
    for group_id in groups {
        let Some(group) = state.group_by_id(&group_id).cloned() else {
            continue;
        };
        let added: Vec<PersonId> = group
            .house_added
            .iter()
            .filter(|p| {
                !residents(
                    state,
                    &CleaningGroup {
                        house_added: Vec::new(),
                        ..group.clone()
                    },
                )
                .contains(p)
            })
            .cloned()
            .collect();
        if added != group.house_added {
            state.apply_event(DomainEvent::HouseWideSet {
                group_id: group_id.clone(),
                house_wide: true,
                excused: group.house_excused.clone(),
                added,
            })?;
        }
        let wanted = residents(state, &group);
        let mut change = Change {
            group_id: group_id.clone(),
            ..Change::default()
        };
        let leaving: Vec<&PersonId> = match leavers {
            Leavers::Stay => Vec::new(),
            Leavers::Go => group
                .member_ids
                .iter()
                .filter(|p| !wanted.contains(p))
                .filter(|p| {
                    crate::commands::current_open_assignments(state, &group_id, p).is_empty()
                })
                .collect(),
        };
        for person_id in leaving {
            state.apply_event(DomainEvent::PersonLeftGroup {
                person_id: person_id.clone(),
                group_id: group_id.clone(),
            })?;
            apply_group_departure(ctx, state, person_id, &group_id)?;
            change.left.push(person_id.clone());
        }
        for person_id in wanted.iter().filter(|p| !group.member_ids.contains(p)) {
            apply_group_join(ctx, state, &group_id, person_id)?;
            change.joined.push(person_id.clone());
        }
        if !change.joined.is_empty() || !change.left.is_empty() {
            tracing::info!(
                group = %group.name,
                joined = change.joined.len(),
                left = change.left.len(),
                "House-wide group follows the residents"
            );
            changes.push(change);
        }
    }
    Ok(changes)
}

/// An admin adds `person_id` to, or takes them out of, the house-wide
/// `group_id` by hand (`!member add|remove`): remembered, so `sync` keeps
/// it that way. Call before the membership change itself. A no-op for an
/// ordinary group.
pub(crate) fn note_admin_choice(
    state: &mut State,
    group_id: &GroupId,
    person_id: &PersonId,
    member: bool,
) -> anyhow::Result<()> {
    let Some(group) = state.group_by_id(group_id).filter(|g| g.house_wide) else {
        return Ok(());
    };
    let mut excused = group.house_excused.clone();
    let mut added = group.house_added.clone();
    excused.retain(|p| p != person_id);
    added.retain(|p| p != person_id);
    if !member {
        excused.push(person_id.clone());
    } else if !residents(state, group).contains(person_id) {
        added.push(person_id.clone());
    }
    state.apply_event(DomainEvent::HouseWideSet {
        group_id: group_id.clone(),
        house_wide: true,
        excused,
        added,
    })
}

/// One line about a house-wide group for `!groups` and its details:
/// "🏠 everyone (12) · excused: Mia".
pub fn describe(state: &State, group: &CleaningGroup) -> Option<String> {
    if !group.house_wide {
        return None;
    }
    let names = |ids: &[PersonId]| {
        ids.iter()
            .filter_map(|id| state.person_by_id(id))
            .map(|p| p.display_name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut line = format!("🏠 everyone ({})", group.member_ids.len());
    if !group.house_excused.is_empty() {
        line.push_str(&format!(" · excused: {}", names(&group.house_excused)));
    }
    if !group.house_added.is_empty() {
        line.push_str(&format!(" · added by hand: {}", names(&group.house_added)));
    }
    Some(line)
}
