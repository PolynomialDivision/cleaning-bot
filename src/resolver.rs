//! Slot assignment resolver.
//!
//! `materialize` computes frozen assignments for the upcoming turns of every
//! group (one turn = one shift of one due week, in the group's own rhythm,
//! see `rhythm`) and returns them as `SlotAssigned` (+ `RotationQueueSet`)
//! events. It is a pure function: reads state, returns events to apply.
//! Idempotent: already-assigned turns are skipped and never recomputed or
//! overwritten.
//!
//! # Rotation queue
//!
//! Each `CleaningGroup` carries a persisted `rotation_queue: Vec<PersonId>`.
//! The literal turn order, front = next up. For every not-yet-frozen turn,
//! `materialize` pops one member per slot from the front of the queue and
//! pushes them to the back (a classic round-robin turn queue), then freezes
//! that pick as a `SlotAssigned` event. All turns and slots of the same week
//! draw from the same shared queue in sequence, so nobody is drawn twice in
//! one week, including when there are *fewer* members than turns × slots,
//! in which case the leftovers are left unassigned (`person_id: None`)
//! rather than silently double-booking someone.
//!
//! This deliberately replaces a stateless `index % member_count` formula.
//! That formula has to recompute *every* future week's assignee from
//! scratch whenever the member count changes: join or leave one person and
//! the whole rotation shifts. A persisted queue only has to say where a
//! *new* member enters the line: everyone already queued keeps their
//! existing relative order, and every already-frozen week is left alone.
//!
//! `reconcile_queue` keeps the queue in sync with `member_ids`: it drops
//! anyone no longer a member and appends anyone missing. It is also the only
//! "migration" a pre-queue `state.json` needs, see its docs below.
//! `commands::apply_group_join` / `apply_group_departure` call it too: a
//! join re-plans everything from the next cycle on (`cycle_anchor`,
//! `next_cycle_start`, `spread_newcomers`), a leave drops the leaver.

use std::collections::HashSet;

use crate::{
    analytics::DomainEvent,
    domain::{AssignmentSource, CleaningGroup, PersonId},
    rhythm::Turn,
    state::{add_weeks, current_iso_week, weeks_between, State},
};

/// Reconcile a group's persisted `rotation_queue` against its current
/// `member_ids`: drop ids that are no longer members, append members that
/// aren't queued yet (in `member_ids` order, so a freshly created group
/// starts out in a sensible order), and otherwise preserve everyone's
/// relative position untouched.
///
/// When the queue is completely empty but the group already has members, it
/// is seeded from `member_ids`. If the group already has recorded
/// assignments (i.e. it ran under a pre-queue version of the bot, upgrading
/// in place), the seed is rotated by however many assignments already
/// happened, so the upgrade continues the existing cycle instead of
/// restarting it at `member_ids[0]`.
pub fn reconcile_queue(state: &State, group: &CleaningGroup) -> Vec<PersonId> {
    let known: HashSet<&PersonId> = group.member_ids.iter().collect();
    let mut queue: Vec<PersonId> = group
        .rotation_queue
        .iter()
        .filter(|id| known.contains(id))
        .cloned()
        .collect();

    if queue.is_empty() && !group.member_ids.is_empty() {
        let prior = state
            .slot_assignments
            .iter()
            .filter(|a| a.group_id == group.id && a.person_id.is_some())
            .count();
        let shift = prior % group.member_ids.len();
        queue = group
            .member_ids
            .iter()
            .cloned()
            .cycle()
            .skip(shift)
            .take(group.member_ids.len())
            .collect();
    } else {
        for id in &group.member_ids {
            if !queue.contains(id) {
                queue.push(id.clone());
            }
        }
    }
    let mut seen = HashSet::new();
    queue.retain(|id| seen.insert(id.clone()) && state.person_by_id(id).is_some_and(|p| p.active));
    queue
}

/// Fill all unassigned turns of every group for its next `cycles_ahead` due
/// weeks (from the current week on, each group in its own rhythm).
///
/// Every shift of a due week is its own turn, and every slot of a turn draws
/// the next person from the group's rotation queue, so a group cleaned
/// twice a week hands its two shifts to two consecutive people in the
/// rotation. Nobody is drawn twice within one week. Which of them gets the
/// start of the week takes turns (`seat_by_shift`).
///
/// Returns `SlotAssigned` events plus, for each group that had at least one
/// turn filled, a trailing `RotationQueueSet` capturing the queue's new
/// state. Already-stored assignments are skipped so this is safe to call
/// repeatedly, and it never revisits or changes a turn it already froze.
/// Callers rely on that to make join/leave additive rather than destructive.
pub fn materialize(state: &State, cycles_ahead: usize) -> Vec<DomainEvent> {
    let mut workload = Workload::forecast(state, cycles_ahead);
    let mut events = Vec::new();
    for group in &state.cleaning_groups {
        events.extend(materialize_with(state, group, cycles_ahead, &mut workload));
    }
    events
}

/// `materialize` for one group, weighed against everyone's stored duties.
pub fn materialize_group(
    state: &State,
    group: &CleaningGroup,
    cycles_ahead: usize,
) -> Vec<DomainEvent> {
    materialize_with(
        state,
        group,
        cycles_ahead,
        &mut Workload::forecast(state, cycles_ahead),
    )
}

/// `materialize_group` with the duties drawn so far in this run, in other
/// groups too (`workload`), which it extends by its own draws.
///
/// # Who is drawn
///
/// The queue decides *how often* everyone cleans: each pick comes from the
/// front of it, so within a group everyone gets the same number of turns.
/// Which of the people nearly due gets *this* week is decided by what else
/// they have to do around then (`pick_week`): of the first few in line, the
/// ones whose other duties clash least with this week, compared with the
/// next due week (waiting only helps someone with less on then), a place
/// further back in line counting against them a little (`LINE_COST`), so the
/// plain rotation order wins unless a real clash is spared. Someone put off
/// keeps their place at the front, after `MAX_DEFERRALS` due weeks of it is
/// drawn first whatever it costs, and makes the delay up on their next turn
/// (they go back in line ahead of the people who jumped them). A clash
/// postpones a turn by a week or two; nobody cleans less often for it.
fn materialize_with(
    state: &State,
    group: &CleaningGroup,
    cycles_ahead: usize,
    workload: &mut Workload,
) -> Vec<DomainEvent> {
    if cycles_ahead == 0 || group.member_ids.is_empty() || !group.is_active {
        return vec![];
    }
    let mut events: Vec<DomainEvent> = Vec::new();
    let num_slots = group.slots.len().max(1);
    let first_due = state.next_due_week(group, current_iso_week());
    let mut queue = reconcile_queue(state, group);
    let mut deferred: std::collections::BTreeMap<PersonId, u32> = group
        .rotation_deferred
        .iter()
        .filter(|(pid, _)| queue.contains(pid))
        .map(|(pid, n)| (pid.clone(), *n))
        .collect();
    let mut any_pop = false;
    // The queue reflects every rotation draw up to the group's last one.
    // A pinned turn after that (an import, `!plan assign`, a takeover or
    // swap the rotation hasn't drawn past yet) counts as that person's turn
    // too: they go to the back of the line like anyone who just cleaned, so
    // the rotation doesn't hand them another turn right after it.
    let drawn_until = state
        .slot_assignments
        .iter()
        .filter(|a| {
            a.group_id == group.id
                && a.source == AssignmentSource::RoundRobin
                && a.person_id.is_some()
        })
        .map(|a| Turn::new(a.iso_year, a.iso_week, a.shift))
        .max();

    // This run's draws, which `last_shift` can't find in `state` yet.
    let mut planned: Vec<(Turn, PersonId)> = Vec::new();

    let mut due = first_due;
    for _ in 0..cycles_ahead {
        let (dy, dw) = due;
        due = state.next_due_week(group, add_weeks(dy, dw, 1));

        // Track who's already been drawn for *this* week so nobody is
        // picked twice: skipping absent members means a draw can land past
        // the front, so identity (not count) is what has to be deduplicated.
        let mut used_this_week: HashSet<PersonId> = state
            .slot_assignments
            .iter()
            .filter(|a| a.group_id == group.id && (a.iso_year, a.iso_week) == (dy, dw))
            .filter_map(|a| a.person_id.clone())
            .collect();

        let turns = state.turns_in_week(group, dy, dw);
        let frozen = |turn: Turn, si: usize| {
            state.slot_assignments.iter().find(|a| {
                a.group_id == group.id
                    && a.slot_index == si
                    && (a.iso_year, a.iso_week, a.shift) == (turn.year, turn.week, turn.shift)
            })
        };
        let mut open: Vec<(Turn, usize)> = Vec::new();
        for &turn in &turns {
            for si in 0..num_slots {
                // Skip if already frozen. This is what makes materialize additive.
                let Some(frozen) = frozen(turn, si) else {
                    open.push((turn, si));
                    continue;
                };
                let pinned_after_last_draw = frozen.source != AssignmentSource::RoundRobin
                    && drawn_until.is_none_or(|last| turn > last);
                if let (true, Some(pid)) = (pinned_after_last_draw, &frozen.person_id) {
                    if let Some(pos) = queue.iter().position(|q| q == pid) {
                        let pinned = queue.remove(pos);
                        queue.push(pinned);
                        any_pop = true;
                    }
                    // Their turn came, if by other means: nothing left to make up.
                    deferred.remove(pid);
                }
            }
        }

        // Who is in line this week: queue members not already used this
        // week and not on record absence, in line order. Anyone passed over
        // is not touched, so they keep their place in line and lose no turn.
        let line: Vec<PersonId> = queue
            .iter()
            .filter(|pid| {
                !used_this_week.contains(*pid) && !state.is_absent(pid, &group.id, dy, dw)
            })
            .cloned()
            .collect();
        let picks = pick_week(
            group,
            &line,
            &open,
            turns.len() * num_slots,
            &deferred,
            workload,
        );
        let picked: HashSet<&PersonId> = picks.iter().collect();
        // Whoever was in line ahead of someone drawn this week, but not
        // drawn themselves, was put off: by as many people as were drawn
        // behind them. Whoever was drawn is owed the places they lost (`debt`).
        let mut debt: std::collections::HashMap<PersonId, u32> = Default::default();
        for (rank, pid) in line.iter().enumerate() {
            if picked.contains(pid) {
                debt.extend(deferred.remove(pid).map(|n| (pid.clone(), n)));
                continue;
            }
            let jumped = line[rank + 1..]
                .iter()
                .filter(|p| picked.contains(p))
                .count() as u32;
            if jumped > 0 {
                *deferred.entry(pid.clone()).or_default() += jumped;
            }
        }

        // The week's open turns, in order, go to the people drawn, in line
        // order; each is removed from wherever they sit and pushed to the
        // back like a normal draw, but ahead of the people who were drawn
        // before them: that is made up next time, so being put off never
        // means cleaning less often. Seats beyond them stay unassigned.
        let mut drawn: Vec<(Turn, usize, Option<PersonId>)> = Vec::new();
        let mut picks = picks.into_iter();
        for (turn, si) in open {
            let person_id = picks.next().inspect(|picked| {
                if let Some(pos) = queue.iter().position(|q| q == picked) {
                    queue.remove(pos);
                }
                let owed = debt.get(picked).copied().unwrap_or(0) as usize;
                queue.insert(queue.len().saturating_sub(owed), picked.clone());
                any_pop = true;
                used_this_week.insert(picked.clone());
            });
            drawn.push((turn, si, person_id));
        }

        seat_by_shift(state, group, &planned, &mut drawn);
        for (turn, si, person_id) in drawn {
            if let Some(pid) = &person_id {
                planned.push((turn, pid.clone()));
                workload.add(pid, group, si, turn);
            }
            events.push(DomainEvent::SlotAssigned {
                group_id: group.id.clone(),
                slot_index: si,
                iso_year: turn.year,
                iso_week: turn.week,
                shift: turn.shift,
                person_id,
                source: AssignmentSource::RoundRobin,
                // Automatic: nobody "did" this, and materialize only ever
                // fills a not-yet-frozen turn, so there is no prior occupant
                // to record either.
                actor_id: None,
                previous_person_id: None,
            });
        }
    }

    if any_pop {
        events.push(DomainEvent::RotationQueueSet {
            group_id: group.id.clone(),
            queue,
        });
    }
    if deferred != group.rotation_deferred {
        events.push(DomainEvent::RotationDeferralsSet {
            group_id: group.id.clone(),
            deferred,
        });
    }
    events
}

/// Who cleans the `open` turns of one due week, of the people in `line`
/// (in line order), returned in line order: as many as there are open turns
/// (fewer if the line is shorter).
///
/// Anyone put off by `MAX_DEFERRALS` weeks' worth of people (the week's open
/// turns each) already goes first. The rest come
/// from the front of the line (the open turns and as many again, but never
/// reaching the people drawn the due week before: the week's seats at the
/// back of the line, so in a small group the rotation runs strictly in
/// order), whoever's other duties clash least with this week compared with
/// the group's next due week (`Workload::clash`): putting someone off only
/// helps if they have less on then. Each place further back in line counts
/// against them a little (`LINE_COST`).
fn pick_week(
    group: &CleaningGroup,
    line: &[PersonId],
    open: &[(Turn, usize)],
    seats_in_week: usize,
    deferred: &std::collections::BTreeMap<PersonId, u32>,
    workload: &Workload,
) -> Vec<PersonId> {
    let (Some(first), Some(last)) = (open.first(), open.last()) else {
        return Vec::new();
    };
    let start = first.0.dates(&group.rhythm).0;
    let end = last.0.dates(&group.rhythm).1;
    // The group's next due week: where someone put off would land.
    let later = chrono::Duration::weeks(group.rhythm.every_weeks() as i64);
    let load = open.iter().map(|&(_, si)| group.duty_load(si)).sum::<f64>() / open.len() as f64;
    let reach = (open.len() * 2)
        .min(line.len().saturating_sub(seats_in_week))
        .max(open.len());
    let mut ranked: Vec<(bool, f64, usize)> = line
        .iter()
        .take(reach)
        .enumerate()
        .map(|(rank, pid)| {
            let overdue =
                deferred.get(pid).copied().unwrap_or(0) as usize >= MAX_DEFERRALS * open.len();
            // What drawing them now costs more than drawing them next time:
            // someone just as busy then gains nothing by waiting.
            let clash = workload.clash(pid, &group.id, start, end, load)
                - workload.clash(pid, &group.id, start + later, end + later, load);
            (!overdue, clash + LINE_COST * load * rank as f64, rank)
        })
        .collect();
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut ranks: Vec<usize> = ranked.iter().take(open.len()).map(|r| r.2).collect();
    ranks.sort_unstable();
    ranks.into_iter().map(|rank| line[rank].clone()).collect()
}

/// How far back in line a candidate may sit and still be drawn first: each
/// place behind the front costs this much times the turn's load (clash units,
/// see `Workload::clash`). Enough that a vague difference never reorders the
/// rotation, small enough that a real clash (another duty in the same week)
/// moves someone a few places.
const LINE_COST: f64 = 0.15;

/// Due weeks someone may be put off to spare them a clash before they are
/// drawn first anyway.
pub const MAX_DEFERRALS: usize = 2;

/// Of a clash between two duties: on overlapping days, in the same week, a
/// week apart, two weeks apart. Multiplied by both duties' loads.
const CLASH_OVERLAP: f64 = 1.0;
const CLASH_SAME_WEEK: f64 = 0.6;
const CLASH_NEXT_WEEK: f64 = 0.25;
const CLASH_TWO_WEEKS: f64 = 0.08;
/// Of everything else someone has to do in the four weeks around a turn, a
/// small pull towards whoever has less on.
const BUSY_WEIGHT: f64 = 0.03;

/// Everyone's duties, in every active group, from a little before today on:
/// what a draw weighs a candidate's turn against.
#[derive(Default)]
pub struct Workload {
    duties: std::collections::HashMap<PersonId, Vec<Duty>>,
}

struct Duty {
    group_id: crate::domain::GroupId,
    start: chrono::NaiveDate,
    end: chrono::NaiveDate,
    load: f64,
}

impl Workload {
    /// The duties stored in `state` (frozen turns of active groups), from
    /// five weeks back on: older ones can't clash with anything planned.
    pub fn of(state: &State) -> Self {
        let since = crate::state::today() - chrono::Duration::days(35);
        let mut workload = Workload::default();
        for a in &state.slot_assignments {
            let (Some(pid), Some(group)) = (&a.person_id, state.group_by_id(&a.group_id)) else {
                continue;
            };
            let turn = Turn::new(a.iso_year, a.iso_week, a.shift);
            if group.is_active && turn.dates(&group.rhythm).1 >= since {
                workload.add(pid, group, a.slot_index, turn);
            }
        }
        workload
    }

    /// `of` with what the rotation of every group will most likely bring
    /// until the last week any of them is planned to with `cycles` due
    /// weeks: a group cleaned every 2 weeks is planned twice as far ahead
    /// as a weekly one, and is weighed against the weekly one's coming
    /// turns there too, not against nothing. The forecast is plain
    /// planning on a scratch copy, so it is what will happen unless
    /// something changes.
    pub fn forecast(state: &State, cycles: usize) -> Self {
        let last_due = |group: &CleaningGroup| {
            let mut due = state.next_due_week(group, current_iso_week());
            for _ in 1..cycles {
                due = state.next_due_week(group, add_weeks(due.0, due.1, 1));
            }
            due
        };
        let active = || {
            state
                .cleaning_groups
                .iter()
                .filter(|g| g.is_active && !g.member_ids.is_empty())
        };
        let Some(until) = active().map(last_due).max() else {
            return Workload::of(state);
        };
        // Planned in the order `materialize` goes, each group seeing the
        // forecasts before it; kept are only the turns beyond a group's own
        // horizon, which `materialize` adds itself as it draws them.
        let mut planning = Workload::of(state);
        let mut workload = Workload::of(state);
        for group in active() {
            let first = state.next_due_week(group, current_iso_week());
            let weeks = weeks_between(first, until).max(0) as usize + 1;
            let due = (0..weeks)
                .filter(|&i| {
                    let (y, w) = add_weeks(first.0, first.1, i as i64);
                    state.is_due_week(group, y, w)
                })
                .count();
            if due <= cycles {
                continue;
            }
            let horizon = last_due(group);
            for event in materialize_with(state, group, due, &mut planning) {
                if let DomainEvent::SlotAssigned {
                    slot_index,
                    iso_year,
                    iso_week,
                    shift,
                    person_id: Some(pid),
                    ..
                } = event
                {
                    if (iso_year, iso_week) > horizon {
                        workload.add(
                            &pid,
                            group,
                            slot_index,
                            Turn::new(iso_year, iso_week, shift),
                        );
                    }
                }
            }
        }
        workload
    }

    pub fn add(&mut self, person: &PersonId, group: &CleaningGroup, slot_index: usize, turn: Turn) {
        let (start, end) = turn.dates(&group.rhythm);
        self.duties.entry(person.clone()).or_default().push(Duty {
            group_id: group.id.clone(),
            start,
            end,
            load: group.duty_load(slot_index),
        });
    }

    /// What it costs `person` to take a turn of `group` from `start` to
    /// `end` that is `load` work, on top of their duties in *other* groups
    /// (their own group's spacing is the queue's job): each duty on the same
    /// days, in the same week, or a week or two apart counts by its load
    /// times this one, closer ones much more; plus a little for all they
    /// have on in the four weeks around it. 0 for someone with nothing else
    /// on, so a single group's rotation runs exactly in line order.
    pub fn clash(
        &self,
        person: &PersonId,
        group: &crate::domain::GroupId,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
        load: f64,
    ) -> f64 {
        let Some(duties) = self.duties.get(person) else {
            return 0.0;
        };
        let monday = |d: chrono::NaiveDate| {
            d - chrono::Duration::days(chrono::Datelike::weekday(&d).num_days_from_monday() as i64)
        };
        duties
            .iter()
            .filter(|d| &d.group_id != group)
            .map(|d| {
                let gap = (d.start - end).num_days().max((start - d.end).num_days());
                let weeks_apart = (monday(d.start) - monday(start)).num_weeks().abs();
                let closeness = if gap <= 0 {
                    CLASH_OVERLAP
                } else if weeks_apart == 0 {
                    CLASH_SAME_WEEK
                } else if weeks_apart == 1 {
                    CLASH_NEXT_WEEK
                } else if weeks_apart == 2 {
                    CLASH_TWO_WEEKS
                } else {
                    0.0
                };
                let busy = if gap <= 14 { BUSY_WEIGHT } else { 0.0 };
                (closeness + busy) * load * d.load
            })
            .sum()
    }
}

/// Who of the people drawn for one week (`drawn`, in turn order) cleans
/// which shift: slot by slot, everyone gets the shift after the one they
/// had last time, so in a week split into Mon–Wed / Thu–Sun nobody always
/// has the start of the week. A plain draw would, whenever the group has
/// an even number of members. Who cleans in which week stays as drawn.
fn seat_by_shift(
    state: &State,
    group: &CleaningGroup,
    planned: &[(Turn, PersonId)],
    drawn: &mut [(Turn, usize, Option<PersonId>)],
) {
    let Some(&(first, ..)) = drawn.first() else {
        return;
    };
    let rhythm = group.rhythm.for_week(first.year, first.week);
    let shifts = rhythm.shift_count() as u8;
    if shifts < 2 {
        return;
    }
    for slot in State::slot_indices(group) {
        let seats: Vec<usize> = (0..drawn.len())
            .filter(|&i| drawn[i].1 == slot && drawn[i].2.is_some())
            .collect();
        let mut people: Vec<(PersonId, Option<u8>)> = seats
            .iter()
            .filter_map(|&i| drawn[i].2.clone())
            .map(|pid| {
                let next =
                    last_shift(state, group, planned, &pid, first.week()).map(|s| (s + 1) % shifts);
                (pid, next)
            })
            .collect();
        // Shift by shift: whoever is due for it, else someone without a
        // shift so far, else the next in line.
        for i in seats {
            let shift = drawn[i].0.shift;
            let pos = people
                .iter()
                .position(|(_, next)| *next == Some(shift))
                .or_else(|| people.iter().position(|(_, next)| next.is_none()))
                .unwrap_or(0);
            drawn[i].2 = Some(people.remove(pos).0);
        }
    }
}

/// The shift `person` had on their last turn in `group` before week
/// `before`, counting only weeks split into shifts, from what is stored
/// and from `planned` (draws not stored yet).
fn last_shift(
    state: &State,
    group: &CleaningGroup,
    planned: &[(Turn, PersonId)],
    person: &PersonId,
    before: (i32, u32),
) -> Option<u8> {
    let stored = state
        .slot_assignments
        .iter()
        .filter(|a| a.group_id == group.id && a.person_id.as_ref() == Some(person))
        .map(|a| Turn::new(a.iso_year, a.iso_week, a.shift));
    let drawn = planned
        .iter()
        .filter(|(_, pid)| pid == person)
        .map(|(turn, _)| *turn);
    stored
        .chain(drawn)
        .filter(|t| t.week() < before && group.rhythm.for_week(t.year, t.week).is_split())
        .max()
        .map(|t| t.shift)
}

/// Once, for plans made before `seat_by_shift`: re-seats the weeks from
/// `from` on the same way, so the start of the week takes turns there too.
/// Only plain rotation is touched. A slot's week whose turns were all
/// drawn by the rotation, none of them done or asked about; anything
/// assigned, swapped or taken over stays as it is. Only swaps who has
/// which shift within a week: nobody gains or loses a week. Returns how
/// many turns changed hands.
pub fn rebalance_shifts(state: &mut State, from: (i32, u32)) -> anyhow::Result<usize> {
    let mut changed = 0;
    for group in state.cleaning_groups.clone() {
        if !group.is_active {
            continue;
        }
        let mut weeks: Vec<(i32, u32)> = state
            .slot_assignments
            .iter()
            .filter(|a| a.group_id == group.id && (a.iso_year, a.iso_week) >= from)
            .map(|a| (a.iso_year, a.iso_week))
            .collect();
        weeks.sort_unstable();
        weeks.dedup();
        for (year, week) in weeks {
            let turns = state.turns_in_week(&group, year, week);
            for slot in State::slot_indices(&group) {
                let Some(mut drawn) = plain_rotation(state, &group, slot, &turns) else {
                    continue;
                };
                let before = drawn.clone();
                seat_by_shift(state, &group, &[], &mut drawn);
                for ((turn, _, person_id), (_, _, previous)) in drawn.into_iter().zip(before) {
                    if person_id == previous {
                        continue;
                    }
                    state.apply_event(DomainEvent::SlotAssigned {
                        group_id: group.id.clone(),
                        slot_index: slot,
                        iso_year: turn.year,
                        iso_week: turn.week,
                        shift: turn.shift,
                        person_id,
                        source: AssignmentSource::RoundRobin,
                        actor_id: None,
                        previous_person_id: previous,
                    })?;
                    changed += 1;
                }
            }
        }
    }
    Ok(changed)
}

/// The turns of one slot's week as the rotation drew them: `None` unless
/// each one was drawn for someone and nothing happened to any of them yet
/// (done, skipped, asked about, a swap pending).
fn plain_rotation(
    state: &State,
    group: &CleaningGroup,
    slot: usize,
    turns: &[Turn],
) -> Option<Vec<(Turn, usize, Option<PersonId>)>> {
    let &first = turns.first()?;
    let week = first.week();
    let touched = state.help_requests.values().any(|r| {
        r.group_id == group.id && r.slot_index == slot && (r.iso_year, r.iso_week) == week
    }) || state.swap_requests.iter().any(|r| {
        r.status == crate::state::SwapStatus::Pending
            && r.group_id == group.id
            && r.slot_index == slot
            && (r.iso_year, r.iso_week) == week
    });
    if touched {
        return None;
    }
    turns
        .iter()
        .map(|&turn| {
            if state.completion_for(group, slot, turn).is_some() {
                return None;
            }
            let a = state.slot_assignments.iter().find(|a| {
                a.group_id == group.id
                    && a.slot_index == slot
                    && (a.iso_year, a.iso_week, a.shift) == (turn.year, turn.week, turn.shift)
            })?;
            (a.source == AssignmentSource::RoundRobin && a.person_id.is_some())
                .then(|| (turn, slot, a.person_id.clone()))
        })
        .collect()
}

/// Preview who `materialize` would assign to one (group, slot, turn) beyond
/// the already-materialized horizon, without persisting anything. Used as
/// the fallback in `State::slot_assignee` for turns nobody has frozen yet.
/// It plans every group as `materialize` does (as the plan views do,
/// `schedule::project`), so they agree; what is frozen later can still
/// differ when the other groups' plans change in between, since every draw
/// weighs them.
pub fn preview_slot_assignee(
    state: &State,
    group: &CleaningGroup,
    slot_index: usize,
    turn: Turn,
) -> Option<PersonId> {
    let first_due = state.next_due_week(group, current_iso_week());
    let offset = weeks_between(first_due, turn.week());
    if offset < 0 {
        return None;
    }
    let cycles = offset as usize + 1;

    materialize(state, cycles)
        .into_iter()
        .find_map(|e| match e {
            DomainEvent::SlotAssigned {
                group_id,
                slot_index: si,
                iso_year,
                iso_week,
                shift,
                person_id,
                ..
            } if group_id == group.id
                && si == slot_index
                && Turn::new(iso_year, iso_week, shift) == turn =>
            {
                person_id
            }
            _ => None,
        })
}

/// Put the people drawn for `dropped` assignments back at the front of the
/// queue, in the order they were drawn: undoing those draws, so clearing
/// future turns (e.g. after a rhythm change) costs nobody their place.
pub fn rewind_queue(
    queue: &[PersonId],
    dropped: &[crate::domain::SlotAssignment],
) -> Vec<PersonId> {
    let mut drawn: Vec<&crate::domain::SlotAssignment> = dropped.iter().collect();
    drawn.sort_by_key(|a| (a.iso_year, a.iso_week, a.shift, a.slot_index));
    let mut front: Vec<PersonId> = Vec::new();
    for a in drawn {
        if let Some(pid) = &a.person_id {
            if !front.contains(pid) {
                front.push(pid.clone());
            }
        }
    }
    let rest: Vec<PersonId> = queue
        .iter()
        .filter(|pid| !front.contains(pid))
        .cloned()
        .collect();
    front.retain(|pid| queue.contains(pid));
    front.extend(rest);
    front
}

/// The member every rotation cycle starts with: of the current members, the
/// one whose first frozen turn in the group comes earliest. The head of the
/// original rotation. Later joiners never displace them, so cycle boundaries
/// stay put as the group grows. `None` while nobody has a turn yet.
pub fn cycle_anchor(state: &State, group: &CleaningGroup) -> Option<PersonId> {
    state
        .slot_assignments
        .iter()
        .filter(|a| a.group_id == group.id)
        .filter_map(|a| {
            let pid = a
                .person_id
                .as_ref()
                .filter(|p| group.member_ids.contains(p))?;
            Some(((a.iso_year, a.iso_week, a.shift, a.slot_index), pid))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, pid)| pid.clone())
}

/// Where the next rotation cycle starts: the first frozen turn after week
/// `after` held by `anchor` (see `cycle_anchor`). `None` if the anchor has
/// no frozen turn that far out yet.
pub fn next_cycle_start(
    state: &State,
    group: &CleaningGroup,
    anchor: &PersonId,
    after: (i32, u32),
) -> Option<Turn> {
    state
        .slot_assignments
        .iter()
        .filter(|a| {
            a.group_id == group.id
                && (a.iso_year, a.iso_week) > after
                && a.person_id.as_ref() == Some(anchor)
        })
        .map(|a| Turn::new(a.iso_year, a.iso_week, a.shift))
        .min()
}

/// One cycle's turn order with `newcomers` seated into `old` (which starts
/// with the cycle's anchor): newcomer `i` follows `old[i·n/k]`, so with one
/// newcomer they come right after the anchor and several are spread evenly
/// through the cycle, only ever side by side when they outnumber `old`.
pub fn spread_newcomers(old: &[PersonId], newcomers: &[PersonId]) -> Vec<PersonId> {
    let (n, k) = (old.len(), newcomers.len());
    if n == 0 {
        return newcomers.to_vec();
    }
    let mut order = Vec::with_capacity(n + k);
    let mut next = 0;
    for (i, pid) in old.iter().enumerate() {
        order.push(pid.clone());
        while next < k && next * n / k == i {
            order.push(newcomers[next].clone());
            next += 1;
        }
    }
    order
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CleaningGroup, CleaningSlot, Person},
        state::State,
    };
    use chrono::Utc;

    fn two_person_state() -> (State, String, String) {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let p1 = Person::new_named("Alice");
        let p2 = Person::new_named("Bob");
        let (id1, id2) = (p1.id.clone(), p2.id.clone());
        st.persons.extend([p1, p2]);
        let mut g = CleaningGroup::new("Kitchen");
        g.member_ids.extend([id1.clone(), id2.clone()]);
        st.cleaning_groups.push(g);
        (st, id1, id2)
    }

    #[test]
    fn first_materialization_assigns_in_order() {
        let (st, id1, id2) = two_person_state();
        let evs = materialize(&st, 2);
        // Cycle 1 → Alice (front of queue), Cycle 2 → Bob.
        if let DomainEvent::SlotAssigned { person_id, .. } = &evs[0] {
            assert_eq!(person_id.as_deref(), Some(id1.as_str()));
        }
        if let DomainEvent::SlotAssigned { person_id, .. } = &evs[1] {
            assert_eq!(person_id.as_deref(), Some(id2.as_str()));
        }
    }

    #[test]
    fn already_stored_slots_are_skipped() {
        let (mut st, id1, _id2) = two_person_state();
        // Store the first assignment manually.
        let first_evs = materialize(&st, 1);
        for ev in &first_evs {
            st.apply_event(ev.clone()).unwrap();
        }
        // Running materialize again should produce no new events for week 1.
        let second_evs = materialize(&st, 1);
        assert!(
            second_evs.is_empty(),
            "should skip already-stored assignment"
        );
        // The stored assignment should still be Alice.
        let stored = &st.slot_assignments[0];
        assert_eq!(stored.person_id.as_deref(), Some(id1.as_str()));
    }

    #[test]
    fn rotation_stable_after_member_leaves() {
        let (mut st, id1, id2) = two_person_state();
        // Materialize week 1 (Alice) and week 2 (Bob).
        let evs = materialize(&st, 2);
        for ev in evs {
            st.apply_event(ev).unwrap();
        }

        // Alice leaves.
        let gid = st.cleaning_groups[0].id.clone();
        st.apply_event(DomainEvent::PersonLeftGroup {
            person_id: id1.clone(),
            group_id: gid,
        })
        .unwrap();

        // Materialize week 3: only Bob remains in member_ids, so reconcile_queue
        // drops Alice even though nothing explicitly touched the stored queue.
        let new_evs = materialize(&st, 3);
        let unfrozen: Vec<_> = new_evs
            .iter()
            .filter(|e| matches!(e, DomainEvent::SlotAssigned { .. }))
            .collect();
        assert!(!unfrozen.is_empty());
        if let DomainEvent::SlotAssigned { person_id, .. } = &unfrozen[0] {
            assert_eq!(
                person_id.as_deref(),
                Some(id2.as_str()),
                "Bob should be next"
            );
        }
    }

    #[test]
    fn multi_slot_round_robin() {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let people: Vec<Person> = (0..4)
            .map(|i| Person::new_named(&format!("P{i}")))
            .collect();
        let ids: Vec<_> = people.iter().map(|p| p.id.clone()).collect();
        st.persons.extend(people);

        let mut g = CleaningGroup::new("Floor");
        g.member_ids.extend(ids.clone());
        let mut s0 = CleaningSlot::new("Scharni");
        s0.id = "s0".into();
        let mut s1 = CleaningSlot::new("Colbe");
        s1.id = "s1".into();
        g.slots.extend([s0, s1]);
        st.cleaning_groups.push(g);

        let evs = materialize(&st, 2);
        let slot_evs: Vec<_> = evs
            .iter()
            .filter(|e| matches!(e, DomainEvent::SlotAssigned { .. }))
            .collect();
        assert_eq!(slot_evs.len(), 4);
        // Week1/slot0 → P0, Week1/slot1 → P1, Week2/slot0 → P2, Week2/slot1 → P3.
        if let (
            DomainEvent::SlotAssigned { person_id: p0, .. },
            DomainEvent::SlotAssigned { person_id: p1, .. },
        ) = (slot_evs[0], slot_evs[1])
        {
            assert_eq!(p0.as_deref(), Some(ids[0].as_str()));
            assert_eq!(p1.as_deref(), Some(ids[1].as_str()));
        }
        if let (
            DomainEvent::SlotAssigned { person_id: p2, .. },
            DomainEvent::SlotAssigned { person_id: p3, .. },
        ) = (slot_evs[2], slot_evs[3])
        {
            assert_eq!(p2.as_deref(), Some(ids[2].as_str()));
            assert_eq!(p3.as_deref(), Some(ids[3].as_str()));
        }
    }

    #[test]
    fn three_person_rotation_cycles_stably() {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let people: Vec<Person> = ["Anna", "Bob", "Carla"]
            .iter()
            .map(|n| Person::new_named(n))
            .collect();
        let ids: Vec<_> = people.iter().map(|p| p.id.clone()).collect();
        st.persons.extend(people);
        let mut g = CleaningGroup::new("Floor");
        g.member_ids.extend(ids.clone());
        st.cleaning_groups.push(g);

        let evs = materialize(&st, 6);
        let names: Vec<Option<String>> = evs
            .iter()
            .filter_map(|e| match e {
                DomainEvent::SlotAssigned { person_id, .. } => Some(person_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            vec![
                Some(ids[0].clone()),
                Some(ids[1].clone()),
                Some(ids[2].clone()),
                Some(ids[0].clone()),
                Some(ids[1].clone()),
                Some(ids[2].clone()),
            ]
        );
    }

    #[test]
    fn materialize_is_deterministic() {
        let (st, ..) = two_person_state();
        let a = materialize(&st, 8);
        let b = materialize(&st, 8);
        let render = |evs: &[DomainEvent]| format!("{evs:?}");
        assert_eq!(render(&a), render(&b));
    }

    #[test]
    fn reconcile_seeds_fresh_queue_from_member_ids() {
        let (st, id1, id2) = two_person_state();
        let group = &st.cleaning_groups[0];
        assert_eq!(reconcile_queue(&st, group), vec![id1, id2]);
    }

    #[test]
    fn reconcile_continues_the_cycle_after_an_upgrade_from_a_queueless_state() {
        // Simulate a pre-queue installation: history exists (3 prior
        // assignments to a 2-person group) but `rotation_queue` was never
        // populated (as if this state.json predates the field).
        let (mut st, id1, id2) = two_person_state();
        let gid = st.cleaning_groups[0].id.clone();
        for (i, pid) in [id1.clone(), id2.clone(), id1.clone()]
            .into_iter()
            .enumerate()
        {
            st.slot_assignments.push(crate::domain::SlotAssignment {
                group_id: gid.clone(),
                slot_index: 0,
                iso_year: 2020,
                iso_week: (i + 1) as u32,
                shift: 0,
                person_id: Some(pid),
                source: Default::default(),
            });
        }
        st.cleaning_groups[0].rotation_queue.clear();

        // 3 prior assignments, 2 members → shift by 1 → queue continues [Bob, Alice].
        let queue = reconcile_queue(&st, &st.cleaning_groups[0]);
        assert_eq!(queue, vec![id2, id1]);
    }

    fn make_group(
        name: &str,
        member_count: usize,
        slot_count: usize,
    ) -> (State, crate::domain::GroupId, Vec<PersonId>) {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let people: Vec<Person> = (0..member_count)
            .map(|i| Person::new_named(&format!("P{i}")))
            .collect();
        let ids: Vec<_> = people.iter().map(|p| p.id.clone()).collect();
        st.persons.extend(people);
        let mut g = CleaningGroup::new(name);
        let gid = g.id.clone();
        g.member_ids.extend(ids.clone());
        for i in 0..slot_count {
            let mut slot = CleaningSlot::new(&format!("Slot{i}"));
            slot.id = format!("s{i}");
            g.slots.push(slot);
        }
        st.cleaning_groups.push(g);
        (st, gid, ids)
    }

    fn week_slot_picks(
        evs: &[DomainEvent],
        week_index: usize,
        num_slots: usize,
    ) -> Vec<Option<PersonId>> {
        let slot_evs: Vec<_> = evs
            .iter()
            .filter(|e| matches!(e, DomainEvent::SlotAssigned { .. }))
            .collect();
        (0..num_slots)
            .map(|si| match slot_evs[week_index * num_slots + si] {
                DomainEvent::SlotAssigned { person_id, .. } => person_id.clone(),
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn one_member_two_slots_leaves_the_second_slot_unassigned_not_double_booked() {
        let (st, _gid, ids) = make_group("Floor", 1, 2);
        let evs = materialize(&st, 1);
        let week0 = week_slot_picks(&evs, 0, 2);
        assert_eq!(week0[0], Some(ids[0].clone()), "the one member gets slot 0");
        assert_eq!(
            week0[1], None,
            "slot 1 must stay unassigned, not double-book P0"
        );
    }

    #[test]
    fn two_members_three_slots_leaves_the_third_slot_unassigned_not_double_booked() {
        let (st, _gid, ids) = make_group("Floor", 2, 3);
        let evs = materialize(&st, 1);
        let week0 = week_slot_picks(&evs, 0, 3);
        assert_eq!(week0[0], Some(ids[0].clone()));
        assert_eq!(week0[1], Some(ids[1].clone()));
        assert_eq!(
            week0[2], None,
            "no third distinct member: must not repeat P0"
        );

        // Next week: with only 2 people ever available, both are drawn
        // again (2 picks exactly returns a 2-length queue to its starting
        // order): same pairing, still no repeat *within* the week, and
        // still no third pick.
        let (st, _gid, ids) = make_group("Floor", 2, 3);
        let evs = materialize(&st, 2);
        let week1 = week_slot_picks(&evs, 1, 3);
        assert_eq!(week1[0], Some(ids[0].clone()));
        assert_eq!(week1[1], Some(ids[1].clone()));
        assert_eq!(week1[2], None);
    }

    #[test]
    fn two_members_two_slots_and_three_members_two_slots_never_double_book() {
        // 2 members / 2 slots: exact fit, both slots filled, no None.
        let (st, ..) = make_group("Floor", 2, 2);
        let evs = materialize(&st, 1);
        let week0 = week_slot_picks(&evs, 0, 2);
        assert!(week0.iter().all(|p| p.is_some()));
        assert_ne!(week0[0], week0[1], "must not double-book the same person");

        // 3 members / 2 slots: surplus member, both slots filled, no repeats within the week.
        let (st, ..) = make_group("Floor", 3, 2);
        let evs = materialize(&st, 1);
        let week0 = week_slot_picks(&evs, 0, 2);
        assert!(week0.iter().all(|p| p.is_some()));
        assert_ne!(week0[0], week0[1]);
    }

    #[test]
    fn repeated_materialize_and_apply_with_the_same_horizon_is_a_no_op() {
        // Simulates dashboard refreshes / repeated bot restarts hitting the
        // same already-materialized range: must never advance the queue.
        let (mut st, id1, id2) = two_person_state();
        for ev in materialize(&st, 4) {
            st.apply_event(ev).unwrap();
        }
        let queue_after_first = st.cleaning_groups[0].rotation_queue.clone();
        let assignments_after_first: Vec<_> = st
            .slot_assignments
            .iter()
            .map(|a| (a.iso_week, a.person_id.clone()))
            .collect();

        // "Refresh" three more times with the identical horizon.
        for _ in 0..3 {
            let evs = materialize(&st, 4);
            assert!(
                evs.is_empty(),
                "re-materializing the same horizon must produce zero events"
            );
            for ev in evs {
                st.apply_event(ev).unwrap();
            }
        }

        assert_eq!(
            st.cleaning_groups[0].rotation_queue, queue_after_first,
            "queue must not drift"
        );
        let assignments_after: Vec<_> = st
            .slot_assignments
            .iter()
            .map(|a| (a.iso_week, a.person_id.clone()))
            .collect();
        assert_eq!(assignments_after, assignments_after_first);
        let _ = (id1, id2);
    }

    #[test]
    fn reconcile_continues_correctly_with_a_realistic_mix_of_past_and_future_assignments() {
        // A production-like state: 2 already-completed past weeks and 2
        // already-materialized future weeks for a 3-person group, then
        // `rotation_queue` is cleared to simulate a pre-queue upgrade.
        let (mut st, gid, ids) = make_group("Floor", 3, 0);
        let (cur_y, cur_w) = crate::state::current_iso_week();
        let weeks: Vec<(i32, u32)> = (-2..2i64).map(|n| add_weeks(cur_y, cur_w, n)).collect();
        for (i, &(y, w)) in weeks.iter().enumerate() {
            st.slot_assignments.push(crate::domain::SlotAssignment {
                group_id: gid.clone(),
                slot_index: 0,
                iso_year: y,
                iso_week: w,
                shift: 0,
                person_id: Some(ids[i % 3].clone()),
                source: Default::default(),
            });
        }
        st.cleaning_groups[0].rotation_queue.clear();

        // 4 prior assignments (2 past, 2 future, reconcile doesn't
        // distinguish), 3 members → shift by 1 → queue continues at P1.
        let queue = reconcile_queue(&st, &st.cleaning_groups[0]);
        assert_eq!(queue, vec![ids[1].clone(), ids[2].clone(), ids[0].clone()]);

        // Materializing forward must not touch the already-frozen weeks and
        // must be deterministic across repeated calls.
        let a = materialize(&st, 4);
        let b = materialize(&st, 4);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        for ev in a {
            if let DomainEvent::SlotAssigned {
                iso_year, iso_week, ..
            } = &ev
            {
                assert!(
                    weeks.iter().all(|&(y, w)| (*iso_year, *iso_week) != (y, w)),
                    "must not re-touch an already-frozen week"
                );
            }
        }
    }

    #[test]
    fn preview_beyond_the_horizon_agrees_with_materialize() {
        let (st, id1, id2) = two_person_state();
        // Week 5 is beyond what's been materialized (nothing has yet).
        let (y, w) = add_weeks(current_iso_week().0, current_iso_week().1, 4);
        let group = &st.cleaning_groups[0];
        let preview = preview_slot_assignee(&st, group, 0, Turn::new(y, w, 0));

        let evs = materialize(&st, 5);
        let expected = evs
            .iter()
            .find_map(|e| match e {
                DomainEvent::SlotAssigned {
                    iso_year,
                    iso_week,
                    shift: 0,
                    person_id,
                    ..
                } if *iso_year == y && *iso_week == w => Some(person_id.clone()),
                _ => None,
            })
            .flatten();
        assert_eq!(preview, expected);
        assert!(preview == Some(id1) || preview == Some(id2));
    }

    // ── !member away eligibility ──────────────────────────────────────────────

    fn absence(
        person_id: &PersonId,
        group_id: &str,
        from: (i32, u32),
        duration_weeks: u32,
    ) -> crate::state::Absence {
        crate::state::Absence {
            person_id: person_id.clone(),
            group_id: group_id.to_owned(),
            from_year: from.0,
            from_week: from.1,
            duration_weeks,
        }
    }

    #[test]
    fn absent_member_is_skipped_but_keeps_their_queue_position() {
        // Queue: Anna → Bob → Carla. Anna is absent for the due week.
        let (mut st, gid, ids) = make_group("Floor", 3, 0);
        let (anna, bob, carla) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
        let due = current_iso_week();
        st.absences.push(absence(&anna, &gid, due, 1));

        let evs = materialize(&st, 1);
        let picked = week_slot_picks(&evs, 0, 1);
        assert_eq!(
            picked[0],
            Some(bob.clone()),
            "Bob is next in line after Anna is skipped"
        );

        for ev in evs {
            st.apply_event(ev).unwrap();
        }
        assert_eq!(
            st.cleaning_groups[0].rotation_queue,
            vec![anna, carla, bob],
            "Anna keeps her place at the front instead of losing her turn or being pushed to the back"
        );
    }

    #[test]
    fn returning_from_absence_continues_the_rotation_fairly() {
        // Anna is absent for weeks 1-2, back for week 3. Over three weeks
        // everyone should clean exactly once: Anna's own turn deferred to
        // her return, never lost, never doubled up.
        let (mut st, gid, ids) = make_group("Floor", 3, 0);
        let (anna, bob, carla) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
        let due = current_iso_week();
        st.absences.push(absence(&anna, &gid, due, 2));

        let evs = materialize(&st, 3);
        let picks: Vec<_> = (0..3)
            .map(|i| week_slot_picks(&evs, i, 1)[0].clone())
            .collect();
        assert_eq!(picks, vec![Some(bob), Some(carla), Some(anna)]);
    }

    #[test]
    fn several_members_absent_at_once_are_all_skipped() {
        let (mut st, gid, ids) = make_group("Floor", 3, 0);
        let (anna, bob, carla) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
        let due = current_iso_week();
        st.absences.push(absence(&anna, &gid, due, 1));
        st.absences.push(absence(&bob, &gid, due, 1));

        let evs = materialize(&st, 1);
        assert_eq!(
            week_slot_picks(&evs, 0, 1),
            vec![Some(carla)],
            "only Carla is eligible"
        );
    }

    #[test]
    fn all_members_absent_leaves_the_slot_unassigned_not_a_fallback_pick() {
        let (mut st, gid, ids) = make_group("Floor", 2, 0);
        let due = current_iso_week();
        for id in &ids {
            st.absences.push(absence(id, &gid, due, 1));
        }

        let evs = materialize(&st, 1);
        assert_eq!(
            week_slot_picks(&evs, 0, 1),
            vec![None],
            "nobody eligible: must stay unassigned, not fall back to an absent member"
        );
        // Nobody was actually drawn, so the queue is untouched. No RotationQueueSet.
        assert!(!evs
            .iter()
            .any(|e| matches!(e, DomainEvent::RotationQueueSet { .. })));
    }

    #[test]
    fn multi_slot_with_one_member_absent_does_not_double_book_the_rest() {
        // 3 members, 2 slots, one absent: the other two fill both slots.
        // No repeat, and the absent member is never drawn as a fallback.
        let (mut st, gid, ids) = make_group("Floor", 3, 2);
        let (anna, bob, carla) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
        let due = current_iso_week();
        st.absences.push(absence(&anna, &gid, due, 1));

        let evs = materialize(&st, 1);
        let picked = week_slot_picks(&evs, 0, 2);
        assert!(
            picked.iter().all(|p| p.is_some()),
            "both slots must be filled from the remaining two members"
        );
        assert_ne!(picked[0], picked[1], "must not double-book the same person");
        assert!(
            picked.iter().all(|p| p != &Some(anna.clone())),
            "the absent member must never be drawn"
        );
        let picked_set: std::collections::HashSet<_> = picked.into_iter().flatten().collect();
        assert_eq!(picked_set, std::collections::HashSet::from([bob, carla]));
    }

    #[test]
    fn materialize_with_absences_is_deterministic_across_repeated_runs() {
        // Same guarantee as `materialize_is_deterministic`, but exercising
        // the absence-skip path: matters for restart/event-replay, which
        // must reproduce the identical queue and assignments every time.
        let (mut st, gid, ids) = make_group("Floor", 3, 0);
        let due = current_iso_week();
        st.absences.push(absence(&ids[0], &gid, due, 2));

        let a = materialize(&st, 6);
        let b = materialize(&st, 6);
        assert_eq!(
            format!("{a:?}"),
            format!("{b:?}"),
            "must be a pure function of state"
        );

        for ev in a {
            st.apply_event(ev).unwrap();
        }
        let queue_after_first_pass = st.cleaning_groups[0].rotation_queue.clone();
        let assignments_after_first_pass: Vec<_> = st
            .slot_assignments
            .iter()
            .map(|a| (a.iso_week, a.person_id.clone()))
            .collect();

        // Simulate a restart: materialize again over the same now-frozen
        // horizon (idempotent, every slot is already stored).
        let replay_evs = materialize(&st, 6);
        assert!(
            replay_evs.is_empty(),
            "already-frozen weeks must not be revisited on a restart"
        );
        assert_eq!(st.cleaning_groups[0].rotation_queue, queue_after_first_pass);
        let assignments_after_replay: Vec<_> = st
            .slot_assignments
            .iter()
            .map(|a| (a.iso_week, a.person_id.clone()))
            .collect();
        assert_eq!(assignments_after_first_pass, assignments_after_replay);
    }

    fn ids(names: &[&str]) -> Vec<PersonId> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn spread_newcomers_follows_the_anchor_and_keeps_old_order() {
        let old = ids(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);
        assert_eq!(spread_newcomers(&old, &[]), old);
        assert_eq!(
            spread_newcomers(&old, &ids(&["X"])),
            ids(&["a", "X", "b", "c", "d", "e", "f", "g", "h", "i", "j"])
        );
        assert_eq!(
            spread_newcomers(&old, &ids(&["X", "Y"])),
            ids(&["a", "X", "b", "c", "d", "e", "f", "Y", "g", "h", "i", "j"])
        );
        assert_eq!(
            spread_newcomers(&old, &ids(&["X", "Y", "Z"])),
            ids(&["a", "X", "b", "c", "d", "Y", "e", "f", "g", "Z", "h", "i", "j"])
        );
    }

    #[test]
    fn spread_newcomers_only_bunches_up_when_they_outnumber_the_rest() {
        assert_eq!(
            spread_newcomers(&ids(&["a", "b"]), &ids(&["X", "Y", "Z"])),
            ids(&["a", "X", "Y", "b", "Z"])
        );
        assert_eq!(spread_newcomers(&[], &ids(&["X", "Y"])), ids(&["X", "Y"]));
    }

    #[test]
    fn cycle_anchor_is_the_member_with_the_earliest_turn() {
        let (mut st, id1, id2) = two_person_state();
        assert_eq!(cycle_anchor(&st, &st.cleaning_groups[0]), None);
        for ev in materialize(&st, 4) {
            st.apply_event(ev).unwrap();
        }
        let group = st.cleaning_groups[0].clone();
        assert_eq!(cycle_anchor(&st, &group), Some(id1.clone()));
        let (y, w) = current_iso_week();
        assert_eq!(
            next_cycle_start(&st, &group, &id1, (y, w)),
            Some(Turn::new(add_weeks(y, w, 2).0, add_weeks(y, w, 2).1, 0))
        );

        // Once the anchor leaves, the next-earliest member takes over.
        st.apply_event(DomainEvent::PersonLeftGroup {
            person_id: id1,
            group_id: group.id.clone(),
        })
        .unwrap();
        assert_eq!(cycle_anchor(&st, &st.cleaning_groups[0]), Some(id2));
    }

    /// Kitchen with Anna, Bob, Carla; nothing frozen yet.
    fn three_member_state() -> (State, Vec<PersonId>) {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let people: Vec<Person> = ["Anna", "Bob", "Carla"]
            .iter()
            .map(|n| Person::new_named(n))
            .collect();
        let ids: Vec<PersonId> = people.iter().map(|p| p.id.clone()).collect();
        st.persons = people;
        let mut g = CleaningGroup::new("Kitchen");
        g.member_ids = ids.clone();
        g.rotation_queue = ids.clone();
        st.cleaning_groups.push(g);
        (st, ids)
    }

    fn pin(st: &mut State, offset: i64, who: &PersonId) {
        let (y, w) = current_iso_week();
        let (py, pw) = add_weeks(y, w, offset);
        st.apply_event(DomainEvent::SlotAssigned {
            group_id: st.cleaning_groups[0].id.clone(),
            slot_index: 0,
            iso_year: py,
            iso_week: pw,
            shift: 0,
            person_id: Some(who.clone()),
            source: AssignmentSource::Import,
            actor_id: None,
            previous_person_id: None,
        })
        .unwrap();
    }

    fn plan(st: &State, weeks: i64) -> Vec<Option<PersonId>> {
        let (y, w) = current_iso_week();
        (0..weeks)
            .map(|i| {
                let (y, w) = add_weeks(y, w, i);
                st.slot_assignments
                    .iter()
                    .find(|a| (a.iso_year, a.iso_week) == (y, w))
                    .and_then(|a| a.person_id.clone())
            })
            .collect()
    }

    #[test]
    fn a_pinned_week_counts_as_that_persons_turn() {
        let (mut st, ids) = three_member_state();
        let [anna, bob, carla] = [&ids[0], &ids[1], &ids[2]];
        // The paper plan has Carla on week 1.
        pin(&mut st, 1, carla);
        for ev in materialize(&st, 6) {
            st.apply_event(ev).unwrap();
        }
        // Carla goes to the back after her pinned week like anyone who
        // cleaned: the rotation doesn't hand her week 3 straight after.
        assert_eq!(
            plan(&st, 6),
            [anna, carla, bob, anna, carla, bob].map(|p| Some(p.clone()))
        );
        // Filling again changes nothing (the pin isn't applied twice).
        assert!(materialize(&st, 6).is_empty());
    }

    #[test]
    fn pins_the_rotation_already_drew_past_change_nothing() {
        // A week pinned after it was already planned (a takeover, say) was
        // planned with the old draw: re-applying it later would shift
        // everyone, so it's left alone.
        let (mut st, ids) = three_member_state();
        for ev in materialize(&st, 4) {
            st.apply_event(ev).unwrap();
        }
        let mut pinned = st.clone();
        pin(&mut pinned, 1, &ids[2]);
        for state in [&mut st, &mut pinned] {
            for ev in materialize(state, 7) {
                state.apply_event(ev).unwrap();
            }
        }
        let (unpinned_plan, pinned_plan) = (plan(&st, 7), plan(&pinned, 7));
        assert_eq!(pinned_plan[4..], unpinned_plan[4..]);
        assert_eq!(
            pinned.cleaning_groups[0].rotation_queue,
            st.cleaning_groups[0].rotation_queue
        );
    }

    /// Kitchen cleaned Mon–Wed + Thu–Sun by `names`; nothing frozen yet.
    fn split_state(names: &[&str]) -> (State, Vec<PersonId>) {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let people: Vec<Person> = names.iter().map(|n| Person::new_named(n)).collect();
        let ids: Vec<PersonId> = people.iter().map(|p| p.id.clone()).collect();
        st.persons = people;
        let mut g = CleaningGroup::new("Kitchen");
        g.member_ids = ids.clone();
        g.rotation_queue = ids.clone();
        g.rhythm = crate::rhythm::Rhythm {
            every_weeks: Some(1),
            shift_starts: vec![0, 3],
            ..Default::default()
        };
        st.cleaning_groups.push(g);
        (st, ids)
    }

    /// Who has Mon–Wed and Thu–Sun in each of the next `weeks` weeks.
    fn shifts(st: &State, weeks: i64) -> Vec<[Option<PersonId>; 2]> {
        let (y, w) = current_iso_week();
        (0..weeks)
            .map(|i| {
                let (y, w) = add_weeks(y, w, i);
                [0, 1].map(|shift| {
                    st.slot_assignments
                        .iter()
                        .find(|a| (a.iso_year, a.iso_week, a.shift) == (y, w, shift))
                        .and_then(|a| a.person_id.clone())
                })
            })
            .collect()
    }

    fn week(a: &PersonId, b: &PersonId) -> [Option<PersonId>; 2] {
        [Some(a.clone()), Some(b.clone())]
    }

    #[test]
    fn in_split_weeks_the_start_of_the_week_takes_turns() {
        // Four people: a plain draw would pair Anna+Bob and Carla+Dan with
        // Anna and Carla always on Mon–Wed.
        let (mut st, ids) = split_state(&["Anna", "Bob", "Carla", "Dan"]);
        let [a, b, c, d] = [&ids[0], &ids[1], &ids[2], &ids[3]];
        for ev in materialize(&st, 6) {
            st.apply_event(ev).unwrap();
        }
        assert_eq!(
            shifts(&st, 6),
            [
                week(a, b),
                week(c, d),
                week(b, a),
                week(d, c),
                week(a, b),
                week(c, d)
            ]
        );
        // A plan made this way has nothing left to re-seat.
        let before = st.slot_assignments.clone();
        assert_eq!(rebalance_shifts(&mut st, current_iso_week()).unwrap(), 0);
        assert_eq!(st.slot_assignments, before);
    }

    #[test]
    fn an_odd_rotation_alternates_as_drawn() {
        let (mut st, ids) = split_state(&["Anna", "Bob", "Carla"]);
        let [a, b, c] = [&ids[0], &ids[1], &ids[2]];
        for ev in materialize(&st, 4) {
            st.apply_event(ev).unwrap();
        }
        assert_eq!(
            shifts(&st, 4),
            [week(a, b), week(c, a), week(b, c), week(a, b)]
        );
    }

    #[test]
    fn rebalancing_an_old_plan_re_seats_only_plain_rotation_weeks() {
        let (mut st, ids) = split_state(&["Anna", "Bob", "Carla", "Dan"]);
        let [a, b, c, d] = [&ids[0], &ids[1], &ids[2], &ids[3]];
        let gid = st.cleaning_groups[0].id.clone();
        // A plan from before: Anna and Carla always start the week.
        let (y, w) = current_iso_week();
        for i in 0..6 {
            let (py, pw) = add_weeks(y, w, i);
            let pair = if i % 2 == 0 { [a, b] } else { [c, d] };
            for (shift, who) in pair.into_iter().enumerate() {
                st.apply_event(DomainEvent::SlotAssigned {
                    group_id: gid.clone(),
                    slot_index: 0,
                    iso_year: py,
                    iso_week: pw,
                    shift: shift as u8,
                    person_id: Some(who.clone()),
                    // Week 4 was swapped by hand: it stays.
                    source: if i == 4 {
                        AssignmentSource::Swap
                    } else {
                        AssignmentSource::RoundRobin
                    },
                    actor_id: None,
                    previous_person_id: None,
                })
                .unwrap();
            }
        }
        let changed = rebalance_shifts(&mut st, add_weeks(y, w, 1)).unwrap();
        assert_eq!(changed, 4);
        assert_eq!(
            shifts(&st, 6),
            [
                week(a, b), // this week: left alone
                week(c, d),
                week(b, a),
                week(d, c),
                week(a, b), // swapped by hand: left alone
                week(c, d)  // Carla had Thu–Sun last time
            ]
        );
        // Who cleans which week didn't change, and it all was rotation.
        assert!(st
            .slot_assignments
            .iter()
            .filter(|a| a.iso_week != add_weeks(y, w, 4).1)
            .all(|a| a.source == AssignmentSource::RoundRobin));
    }

    #[test]
    fn a_disabled_group_is_never_planned() {
        let (mut st, _) = three_member_state();
        st.cleaning_groups[0].is_active = false;
        assert!(materialize(&st, 6).is_empty());
    }

    // ── The house: floors, bathrooms and the staircase, side by side ─────────

    /// 16 residents on 4 floors of 4: each floor has a weekly floor turn and
    /// a weekly bathroom (so everyone has one or the other two weeks in
    /// four), and the house-wide staircase every 2 weeks, two people at
    /// Scharni and two at Colbe, planned last, as it was added.
    fn house() -> (State, Vec<PersonId>) {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let people: Vec<Person> = (0..16)
            .map(|i| Person::new_named(&format!("P{i}")))
            .collect();
        let ids: Vec<PersonId> = people.iter().map(|p| p.id.clone()).collect();
        st.persons.extend(people);
        for floor in 0..4 {
            let members = ids[floor * 4..floor * 4 + 4].to_vec();
            let mut g = CleaningGroup::new(&format!("Floor {floor}"));
            g.member_ids = members.clone();
            st.cleaning_groups.push(g);
            let mut bath = CleaningGroup::new(&format!("Bathroom {floor}"));
            bath.member_ids = members;
            bath.weight = 1.5;
            // Bathroom turns start half a cycle after floor turns.
            bath.member_ids.rotate_left(floor % 4);
            st.cleaning_groups.push(bath);
        }
        let mut stairs = CleaningGroup::new("Staircase");
        stairs.house_wide = true;
        stairs.weight = 1.5;
        stairs.rhythm.every_weeks = Some(2);
        for place in ["Scharni", "Colbe"] {
            for n in 1..=2 {
                let mut slot = CleaningSlot::new(&format!("{place} {n}"));
                slot.place = Some(place.into());
                stairs.slots.push(slot);
            }
        }
        stairs.member_ids = ids.clone();
        st.cleaning_groups.push(stairs);
        for ev in materialize(&st, 52) {
            st.apply_event(ev).unwrap();
        }
        (st, ids)
    }

    fn turns_of(st: &State, group: &str, person: &PersonId) -> Vec<(i32, u32)> {
        let gid = &st.group_by_name(group).unwrap().id;
        let mut weeks: Vec<(i32, u32)> = st
            .slot_assignments
            .iter()
            .filter(|a| &a.group_id == gid && a.person_id.as_ref() == Some(person))
            .map(|a| (a.iso_year, a.iso_week))
            .collect();
        weeks.sort_unstable();
        weeks
    }

    /// Staircase turns that fall into a week with another duty of the same person.
    fn clashes(st: &State, ids: &[PersonId]) -> usize {
        ids.iter()
            .map(|pid| {
                let other: HashSet<(i32, u32)> = st
                    .cleaning_groups
                    .iter()
                    .filter(|g| !g.house_wide)
                    .flat_map(|g| turns_of(st, &g.name, pid))
                    .collect();
                turns_of(st, "Staircase", pid)
                    .iter()
                    .filter(|w| other.contains(w))
                    .count()
            })
            .sum()
    }

    #[test]
    fn the_staircase_goes_round_the_whole_house_evenly() {
        let (st, ids) = house();
        let counts: Vec<usize> = ids
            .iter()
            .map(|p| turns_of(&st, "Staircase", p).len())
            .collect();
        // 52 due weeks × 4 people = 208 turns for 16 people: 13 each.
        assert_eq!(counts.iter().sum::<usize>(), 208);
        assert!(
            counts.iter().all(|&n| (12..=14).contains(&n)),
            "staircase turns per person: {counts:?}"
        );
        // Every due week: four different people, two per staircase.
        let gid = &st.group_by_name("Staircase").unwrap().id;
        let mut weeks: std::collections::BTreeMap<(i32, u32), Vec<&PersonId>> = Default::default();
        for a in st.slot_assignments.iter().filter(|a| &a.group_id == gid) {
            weeks
                .entry((a.iso_year, a.iso_week))
                .or_default()
                .extend(a.person_id.as_ref());
        }
        assert_eq!(weeks.len(), 52);
        for people in weeks.values() {
            assert_eq!(people.len(), 4);
            assert_eq!(people.iter().collect::<HashSet<_>>().len(), 4);
        }
    }

    /// `st` with the staircase planned blind, by plain rotation: as if
    /// nobody had any other duty.
    fn planned_blind(st: &State) -> State {
        let mut blind = st.clone();
        let stairs = blind.group_by_name("Staircase").unwrap().clone();
        blind.slot_assignments.retain(|a| a.group_id != stairs.id);
        let stairs = CleaningGroup {
            rotation_queue: Vec::new(),
            rotation_deferred: Default::default(),
            ..stairs
        };
        let alone = State {
            persons: blind.persons.clone(),
            cleaning_groups: vec![stairs.clone()],
            created_at: blind.created_at,
            ..State::default()
        };
        for ev in materialize(&alone, 52) {
            if matches!(ev, DomainEvent::SlotAssigned { .. }) {
                blind.apply_event(ev).unwrap();
            }
        }
        blind
    }

    #[test]
    fn the_staircase_avoids_weeks_with_other_duties_and_nobody_waits_long() {
        let (st, ids) = house();
        let clashing = clashes(&st, &ids);
        let blind = clashes(&planned_blind(&st), &ids);
        // Blind, every fourth staircase turn meets the same person's floor
        // or bathroom week; planned with them in view, few do.
        assert!(
            clashing * 3 <= blind && clashing <= 16,
            "{clashing} of 208 staircase turns clash, {blind} planned blind"
        );
        // Put off to spare a clash, never for long: at most a cycle (4 due
        // weeks) and `MAX_DEFERRALS` more between two turns of anyone.
        for pid in &ids {
            let weeks = turns_of(&st, "Staircase", pid);
            for pair in weeks.windows(2) {
                let gap = weeks_between(pair[0], pair[1]);
                assert!(
                    gap <= 2 * (4 + MAX_DEFERRALS as i64 + 1),
                    "{gap} weeks between turns"
                );
            }
        }
    }

    #[test]
    fn a_plan_made_bit_by_bit_is_as_fair_and_never_changes_what_is_frozen() {
        let (whole, ids) = house();
        let mut bits = whole.clone();
        bits.slot_assignments.clear();
        for g in &mut bits.cleaning_groups {
            g.rotation_queue.clear();
            g.rotation_deferred.clear();
        }
        // The horizon grows a few weeks at a time, as it rolls in the bot.
        for weeks in (4..=52).step_by(4) {
            let before = bits.slot_assignments.clone();
            for ev in materialize(&bits, weeks) {
                bits.apply_event(ev).unwrap();
            }
            assert!(before.iter().all(|a| bits.slot_assignments.contains(a)));
        }
        assert_eq!(bits.slot_assignments.len(), whole.slot_assignments.len());
        let counts: Vec<usize> = ids
            .iter()
            .map(|p| turns_of(&bits, "Staircase", p).len())
            .collect();
        assert!(counts.iter().all(|&n| (12..=14).contains(&n)), "{counts:?}");
        let clashing = clashes(&bits, &ids);
        assert!(
            clashing * 3 <= clashes(&planned_blind(&bits), &ids),
            "{clashing} clash"
        );
    }

    /// Six people in the Hall (weekly), Ann also in the Garden, alone, on
    /// the given rhythm.
    fn hall_and_garden(garden: crate::rhythm::Rhythm) -> (State, Vec<PersonId>) {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let people: Vec<Person> = ["Ann", "Ben", "Cat", "Dan", "Eve", "Fay"]
            .iter()
            .map(|n| Person::new_named(n))
            .collect();
        let ids: Vec<PersonId> = people.iter().map(|p| p.id.clone()).collect();
        st.persons.extend(people);
        let mut busy = CleaningGroup::new("Garden");
        busy.member_ids = vec![ids[0].clone()];
        busy.rhythm = garden;
        st.cleaning_groups.push(busy);
        let mut g = CleaningGroup::new("Hall");
        g.member_ids = ids.clone();
        st.cleaning_groups.push(g);
        for ev in materialize(&st, 36) {
            st.apply_event(ev).unwrap();
        }
        (st, ids)
    }

    #[test]
    fn someone_busy_every_week_gains_nothing_by_waiting_so_is_not_put_off() {
        let (st, ids) = hall_and_garden(crate::rhythm::Rhythm::weekly());
        let (y, w) = current_iso_week();
        let ann = turns_of(&st, "Hall", &ids[0]);
        let in_line: Vec<_> = (0..6).map(|i| add_weeks(y, w, 6 * i)).collect();
        assert_eq!(ann, in_line);
    }

    #[test]
    fn someone_put_off_makes_it_up_and_cleans_as_often_as_everyone() {
        // The Garden every other week, from this one: Ann's Hall turns in
        // line order fall on Garden weeks, so she cleans a week later or
        // earlier, and makes it up.
        let garden = crate::rhythm::Rhythm {
            every_weeks: Some(2),
            ..crate::rhythm::Rhythm::weekly()
        };
        let (st, ids) = hall_and_garden(garden);
        let (y, w) = current_iso_week();
        let counts: Vec<usize> = ids.iter().map(|p| turns_of(&st, "Hall", p).len()).collect();
        // Equally often, but for where 36 weeks happen to end.
        assert!(counts.iter().all(|&n| (5..=7).contains(&n)), "{counts:?}");
        let ann = turns_of(&st, "Hall", &ids[0]);
        let garden_weeks = turns_of(&st, "Garden", &ids[0]);
        for (i, week) in ann.iter().enumerate() {
            let late = weeks_between(add_weeks(y, w, 6 * i as i64), *week);
            // A week later, or earlier: before a Garden week rather than in it.
            assert!(
                late.abs() <= MAX_DEFERRALS as i64,
                "turn {i}: {late} weeks late"
            );
            assert!(!garden_weeks.contains(week), "turn {i} in a Garden week");
        }
    }

    #[test]
    fn without_other_duties_the_rotation_runs_in_line_order() {
        let (st, a, b) = two_person_state();
        let events = materialize(&st, 6);
        assert!(!events
            .iter()
            .any(|e| matches!(e, DomainEvent::RotationDeferralsSet { .. })));
        let picked: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                DomainEvent::SlotAssigned { person_id, .. } => person_id.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(picked, [&a, &b, &a, &b, &a, &b].map(|s| s.clone()));
    }
}
