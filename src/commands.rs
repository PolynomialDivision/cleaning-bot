use anyhow::Result;
use chrono::Utc;
use matrix_sdk::{
    ruma::{
        events::{
            relation::{Reply, Thread},
            room::message::{
                FileInfo, FileMessageEventContent, MessageType, Relation, RoomMessageEventContent,
            },
        },
        OwnedEventId, OwnedUserId, UInt,
    },
    Room,
};
use uuid::Uuid;

use crate::{
    analytics::{self, DomainEvent},
    domain::{
        new_calendar_token, AssignmentSource, CalendarToken, CleaningGroup, GroupId, Person,
        PersonId,
    },
    format, resolver,
    schedule::build_schedule,
    scheduler,
    state::{add_weeks, current_iso_week, week_dates, weeks_between, SwapStatus},
    BotContext,
};

/// Shell-like tokenizer: splits on whitespace but keeps "quoted strings" together.
/// Quotes are stripped from the resulting tokens.
/// Example: `!addroom "2. Stock" "Scharni Toilette"` → ["!addroom", "2. Stock", "Scharni Toilette"]
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' | '\'' => quoted = !quoted,
            ' ' | '\t' if !quoted => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

pub async fn handle(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    body: &str,
    event_id: OwnedEventId,
    thread_root: OwnedEventId,
) -> Result<Option<RoomMessageEventContent>> {
    let mut tokens = tokenize(body);
    let cmd_owned = if tokens.is_empty() {
        String::new()
    } else {
        tokens.remove(0)
    };
    let cmd = cmd_owned.as_str();
    let arg_strings = tokens;
    let args: Vec<&str> = arg_strings.iter().map(String::as_str).collect();

    // Update the sender's display name from Matrix on every command (lightweight).
    {
        let sender_mxid = sender.as_str().to_owned();
        let refs = vec![sender_mxid.as_str()];
        let fetched = format::fetch_names(room, &refs).await;
        if let Some(name) = fetched.get(sender_mxid.as_str()) {
            if !name.is_empty() && name != &sender_mxid {
                let mut state = ctx.state.lock().await;
                if let Some(p) = state
                    .persons
                    .iter_mut()
                    .find(|p| p.matrix_id.as_deref() == Some(&sender_mxid))
                {
                    p.display_name = name.clone();
                }
            }
        }
    }

    // Commands that need direct room access.
    match cmd {
        "!linkmatrix" => {
            let s = cmd_linkmatrix(ctx, sender, room, &args).await?;
            return Ok(s.map(RoomMessageEventContent::text_plain));
        }
        "!cleanplan" => return cmd_cleanplan(ctx, sender, room, &args).await,
        "!remind" => return cmd_remind(ctx, sender, room, &args).await,
        "!announceweek" => return cmd_announceweek(ctx, sender, room).await,
        "!repostplan" => return cmd_announceweek(ctx, sender, room).await,
        "!testnotify" => return cmd_testnotify(room).await,
        "!pdf" => return cmd_pdf(ctx, sender, room, &args, event_id, thread_root).await,
        "!ical" => return cmd_ical(ctx, sender, room, &args).await,
        "!icalreset" => return cmd_icalreset(ctx, sender, room, &args).await,
        _ => {}
    }

    let reply: Option<String> = match cmd {
        "!done" => cmd_done(ctx, sender, &args).await,
        "!status" => cmd_status(ctx).await,
        "!stats" => cmd_stats(ctx, &args).await,
        "!groups" => cmd_floors(ctx).await,
        "!cleaning" => cmd_cleaning(ctx, sender, &args).await,
        "!joingroup" => cmd_joinfloor(ctx, sender, &args).await,
        "!leavegroup" => cmd_leavefloor(ctx, sender, &args).await,
        "!swap" => cmd_swap(ctx, sender, &args).await,
        "!acceptswap" => cmd_acceptswap(ctx, sender, &args).await,
        "!rejectswap" => cmd_rejectswap(ctx, sender, &args).await,
        "!assign" => cmd_assign(ctx, sender, &args).await,
        "!unassign" => cmd_unassign(ctx, sender, &args).await,
        "!importplan" => cmd_importplan(ctx, sender, &args).await,
        "!takeover" => cmd_takeover(ctx, sender, &args).await,
        "!adduser" => cmd_adduser(ctx, sender, &args).await,
        "!removeuser" => cmd_removeuser(ctx, sender, &args).await,
        "!addperson" => cmd_addperson(ctx, sender, &args).await,
        "!removeperson" => cmd_removeperson(ctx, sender, &args).await,
        "!addgroup" => cmd_addfloor(ctx, sender, &args).await,
        "!removegroup" => cmd_removefloor(ctx, sender, &args).await,
        "!resetplan" => cmd_resetplan(ctx, sender, &args).await,
        "!addslot" => cmd_addslot(ctx, sender, &args).await,
        "!removeslot" => cmd_removeslot(ctx, sender, &args).await,
        "!addroom" => cmd_addroom(ctx, sender, &args).await,
        "!removeroom" => cmd_removeroom(ctx, sender, &args).await,
        "!undo" => cmd_undo(ctx, sender, &args).await,
        "!next" => cmd_next(ctx, sender, &args).await,
        "!skip" => cmd_skip(ctx, sender, &args).await,
        "!leaderboard" => cmd_leaderboard(ctx).await,
        "!fairness" => cmd_fairness(ctx, &args).await,
        "!planfairness" => cmd_fairness(ctx, &args).await,
        "!workload" => cmd_workload(ctx).await,
        "!groupstats" => cmd_groupstats(ctx).await,
        "!setgroupweight" => cmd_setgroupweight(ctx, sender, &args).await,
        "!setroomweight" => cmd_setroomweight(ctx, sender, &args).await,
        "!disablegroup" => cmd_disablegroup(ctx, sender, &args).await,
        "!enablegroup" => cmd_enablegroup(ctx, sender, &args).await,
        "!listgroups" => cmd_listgroups(ctx).await,
        "!validate" => cmd_validate(ctx, sender).await,
        "!absent" => cmd_absent(ctx, sender, &args).await,
        "!back" => cmd_back(ctx, sender, &args).await,
        "!blame" => cmd_blame(ctx, &args).await,
        "!help" => Ok(Some(help_text())),
        _ => Ok(None),
    }?;

    // Every mutation that can change who's responsible for, or the status
    // of, the running week's plan goes through this single refresh call —
    // the pinned Matrix message is re-rendered straight from `State`, never
    // patched in place, so it can never drift from the persisted domain state.
    if command_may_change_current_plan(cmd) {
        let (year, week) = current_iso_week();
        scheduler::refresh_pinned_plan(ctx, room, year, week).await;
    }

    match reply {
        None => Ok(None),
        Some(s) => Ok(Some(format::mentionify_rich(&s, room).await)),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// True for every command whose successful effect can change who's
/// responsible for, or the completion status of, the currently displayed
/// weekly plan — `handle` re-renders the pinned message from `State` after
/// any of these (see the call site below) instead of leaving it to drift
/// until the next scheduler tick or restart. Pulled out as its own function
/// (rather than an inline `matches!` at the call site) so this list — which
/// `!importplan` was added to alongside the other admin overrides — is
/// covered by a direct unit test instead of only being verified by reading
/// the dispatcher.
fn command_may_change_current_plan(cmd: &str) -> bool {
    matches!(
        cmd,
        "!done"
            | "!skip"
            | "!undo"
            | "!assign"
            | "!unassign"
            | "!takeover"
            | "!acceptswap"
            | "!importplan"
    )
}

fn require_admin(ctx: &BotContext, sender: &OwnedUserId) -> Result<()> {
    if ctx.admin_users.contains(sender) {
        Ok(())
    } else {
        Err(anyhow::anyhow!("__not_admin__"))
    }
}

/// Returns the MXID or display_name depending on whether the person has Matrix.
fn person_key(p: &Person) -> &str {
    p.matrix_id.as_deref().unwrap_or(&p.display_name)
}

/// Format a CLI deviation as a percentage and a human-readable label.
///
/// Thresholds:  > +10% → overloaded  |  < -10% → under-contributing  |  else → balanced
fn load_icon(actual: f64, expected: f64) -> &'static str {
    let (_, label) = load_delta_pct(actual, expected);
    if label == "under-contributing" {
        "🔴"
    } else if label == "overloaded" {
        "🟠"
    } else {
        "🟢"
    }
}

fn load_delta_pct(actual: f64, expected: f64) -> (String, &'static str) {
    if expected < 0.01 {
        // No expected history yet — can't compute a meaningful percentage.
        return ("n/a".to_owned(), "balanced");
    }
    let pct = (actual - expected) / expected * 100.0;
    let pct_str = if pct.abs() < 0.05 {
        "±0.0%".to_owned()
    } else if pct > 0.0 {
        format!("+{pct:.1}%")
    } else {
        format!("{pct:.1}%")
    };
    let label = if pct > 10.0 {
        "overloaded"
    } else if pct < -10.0 {
        "under-contributing"
    } else {
        "balanced"
    };
    (pct_str, label)
}

/// Fill not-yet-frozen future weeks for every group using the current
/// rotation queue, up to `weeks_ahead` due-cycles. Idempotent and purely
/// additive — `resolver::materialize` never revisits or changes a week it
/// already froze, so this is safe to call at any time without disturbing
/// anyone's existing plan.
fn materialize_and_apply(
    ctx: &BotContext,
    state: &mut crate::state::State,
    weeks_ahead: usize,
) -> anyhow::Result<()> {
    let interval = ctx.config.schedule.interval_weeks;
    for ev in resolver::materialize(state, interval, weeks_ahead) {
        state.apply_event(ev)?;
    }
    Ok(())
}

/// How many due-cycles ahead of `first_due_week` a group is *already*
/// materialized (i.e. has a frozen `SlotAssignment` for), based on its
/// furthest currently-stored assignment. 0 means nothing is frozen yet.
fn group_horizon_weeks_ahead(
    state: &crate::state::State,
    group_id: &GroupId,
    interval: u32,
) -> usize {
    let first_due = crate::state::first_due_week(state, interval);
    let max_offset = state
        .slot_assignments
        .iter()
        .filter(|a| a.group_id == *group_id)
        .filter_map(|a| {
            let d = crate::state::weeks_between(first_due, (a.iso_year, a.iso_week));
            (d >= 0).then_some(d as usize)
        })
        .max();
    match max_offset {
        Some(d) => d / (interval.max(1) as usize) + 1,
        None => 0,
    }
}

/// Admin escape hatch (`!resetplan`): explicitly wipe and redistribute a
/// group's future schedule, unlike a join/leave which never touches
/// already-frozen weeks. Clears future `slot_assignments`, resets the
/// rotation queue to plain `member_ids` order, then refills.
fn reset_and_rematerialize(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &str,
) -> anyhow::Result<usize> {
    let (cur_y, cur_w) = current_iso_week();
    let before = state.slot_assignments.len();
    // Drop assignments after the active week; they'll be rebuilt with the full list.
    state.slot_assignments.retain(|a| {
        a.group_id != group_id || a.iso_year < cur_y || (a.iso_year == cur_y && a.iso_week <= cur_w)
    });
    let cleared = before - state.slot_assignments.len();

    if let Some(group) = state.group_by_id(&group_id.to_owned()).cloned() {
        state.apply_event(DomainEvent::RotationQueueSet {
            group_id: group_id.to_owned(),
            queue: group.member_ids.clone(),
        })?;
    }
    // Explicit admin escape hatch: commit the full configured horizon, not
    // just one more due-cycle — unlike a join/leave, this is a deliberate
    // "redistribute everything now" action.
    let weeks = ctx.config.schedule.materialize_weeks as usize;
    materialize_and_apply(ctx, state, weeks)?;
    Ok(cleared)
}

/// Insert `person_id` into `group_id`'s rotation queue and fill in any
/// newly-reachable future weeks.
///
/// Must be called with `state` already locked and `person_id` already a
/// registered `Person`, BEFORE `PersonJoinedGroup` is applied (this function
/// applies it). Never touches an already-frozen week: the active week is
/// explicitly frozen first (using the *pre-join* queue) so nobody's current
/// task changes because someone else just joined, and everything already
/// materialized beyond that stays exactly as it was.
///
/// Insertion point: right after the last currently-queued member who
/// hasn't had a single turn yet (any `SlotAssignment` in this group), and
/// right before the first one who has. A newcomer must never skip someone
/// who is still waiting for their *first* turn — but if everyone already
/// queued has had at least one, the newcomer belongs at the very front,
/// ahead of anyone about to start a repeat lap. When nobody queued has had
/// a turn yet (a brand-new group being set up), this is equivalent to
/// appending at the back, so founding members keep their natural join order.
pub(crate) fn apply_group_join(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &GroupId,
    person_id: &PersonId,
) -> anyhow::Result<()> {
    let rotation_was_empty = state
        .group_by_id(group_id)
        .is_some_and(|g| g.member_ids.is_empty());

    // Freeze the active week (if not already frozen) using the OLD queue,
    // before the newcomer can possibly be picked for a week already underway.
    freeze_schedule_before_join(ctx, state, group_id)?;

    if let Some(group) = state.group_by_id(group_id).cloned() {
        let mut queue = resolver::reconcile_queue(state, &group);
        queue.retain(|id| id != person_id);
        let has_had_a_turn = |pid: &PersonId| {
            state
                .slot_assignments
                .iter()
                .any(|a| a.group_id == *group_id && a.person_id.as_deref() == Some(pid.as_str()))
        };
        let insert_at = queue
            .iter()
            .rposition(|pid| !has_had_a_turn(pid))
            .map_or(0, |i| i + 1);
        queue.insert(insert_at, person_id.clone());
        state.apply_event(DomainEvent::RotationQueueSet {
            group_id: group_id.clone(),
            queue,
        })?;
    }

    state.apply_event(DomainEvent::PersonJoinedGroup {
        person_id: person_id.clone(),
        group_id: group_id.clone(),
    })?;

    if rotation_was_empty {
        // First-ever member: don't let materialize hand them a week that's
        // already underway — their first real turn starts next week.
        keep_active_week_unassigned_for_first_member(ctx, state, group_id)?;
    }

    // Reveal exactly one more due-cycle than is already frozen (capped at
    // the configured horizon) — enough that the joiner's own turn becomes
    // visible soon, without a single early member's join greedily claiming
    // the *entire* configured horizon before anyone else has a chance to
    // join. (Deeper horizons still get filled by bot startup or !resetplan.)
    let interval = ctx.config.schedule.interval_weeks;
    let materialize_weeks = ctx.config.schedule.materialize_weeks as usize;
    let horizon = group_horizon_weeks_ahead(state, group_id, interval);
    let weeks_ahead = (horizon + 1).min(materialize_weeks.max(horizon));
    materialize_and_apply(ctx, state, weeks_ahead)
}

/// Drop `person_id` from `group_id`'s rotation queue, clear their future
/// (not-yet-past, not-current) frozen assignments, and refill the resulting
/// gaps from the remaining queue. Other members' already-frozen weeks are
/// never touched.
///
/// Must be called with `state` already locked, AFTER `PersonLeftGroup` is
/// applied (so `reconcile_queue` naturally drops the leaver). Returns the
/// number of future assignments that were cleared and refilled.
fn apply_group_departure(
    ctx: &BotContext,
    state: &mut crate::state::State,
    person_id: &PersonId,
    group_id: &GroupId,
) -> anyhow::Result<usize> {
    let removed = remove_future_assignments_for_person(state, person_id, group_id);

    if let Some(group) = state.group_by_id(group_id).cloned() {
        let queue = resolver::reconcile_queue(state, &group);
        state.apply_event(DomainEvent::RotationQueueSet {
            group_id: group_id.clone(),
            queue,
        })?;
    }

    // Refill within whatever horizon this group already had — a leave
    // creates gaps, it never needs to extend the horizon further out.
    let interval = ctx.config.schedule.interval_weeks;
    let horizon = group_horizon_weeks_ahead(state, group_id, interval);
    materialize_and_apply(ctx, state, horizon)?;
    Ok(removed)
}

fn remove_future_assignments_for_person(
    state: &mut crate::state::State,
    person_id: &str,
    group_id: &str,
) -> usize {
    let (cur_y, cur_w) = current_iso_week();
    let before = state.slot_assignments.len();
    state.slot_assignments.retain(|a| {
        a.group_id != group_id
            || a.person_id.as_deref() != Some(person_id)
            || a.iso_year < cur_y
            || (a.iso_year == cur_y && a.iso_week <= cur_w)
    });
    before - state.slot_assignments.len()
}

/// Extract an optional trailing `week <1-53>` clause from `args`.
///
/// Returns the remaining args (with the clause removed) and the resolved
/// `(year, week)` — the current week when no clause is present, rolling into
/// next year when the requested week number has already passed this year.
/// `None` means a `week` keyword was present but not followed by a valid
/// 1-53 number; the caller should show its own usage message in that case.
fn extract_week_arg<'a>(args: &'a [&'a str]) -> Option<(&'a [&'a str], (i32, u32))> {
    let (cur_y, cur_w) = current_iso_week();
    match args.iter().position(|a| a.eq_ignore_ascii_case("week")) {
        Some(pos) => {
            let n: u32 = args
                .get(pos + 1)
                .and_then(|s| s.parse().ok())
                .filter(|n| (1..=53).contains(n))?;
            let y = if n < cur_w { cur_y + 1 } else { cur_y };
            Some((&args[..pos], (y, n)))
        }
        None => Some((args, (cur_y, cur_w))),
    }
}

/// Resolve `<group> [<slot>]` against `state`, matching the convention used
/// by `!addroom`/`!removeroom`: the token right after the group name is only
/// treated as a slot name when the group is multi-slot and it actually
/// matches one of its slots.  Returns the group id, the slot index to use in
/// a `SlotAssignment` (0 for single-slot groups), and the remaining args.
fn resolve_group_and_slot<'a>(
    state: &crate::state::State,
    group_name: &str,
    rest: &'a [&'a str],
) -> std::result::Result<(GroupId, usize, &'a [&'a str]), String> {
    let group = state
        .group_by_name(group_name)
        .ok_or_else(|| format!("Group «{group_name}» not found."))?;
    if group.is_multi_slot() {
        match rest.first().and_then(|s| {
            group
                .slots
                .iter()
                .position(|slot| slot.name.eq_ignore_ascii_case(s))
        }) {
            Some(idx) => Ok((group.id.clone(), idx, &rest[1..])),
            None => Err(format!(
                "«{group_name}» has slots. Specify one: {}",
                group
                    .slots
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    } else {
        Ok((group.id.clone(), 0, rest))
    }
}

/// Resolve what a `!takeover` invocation refers to, applying self-service
/// defaults instead of requiring the full explicit `<group> <slot>` syntax:
///
/// - No group given → the sender's own group, provided they're in exactly
///   one (never guessed among several — that returns a message listing them).
/// - No slot given → auto-picked when the target group/week has exactly one
///   takeable slot (not already completed/skipped, not already the sender's);
///   with zero or several candidates, returns a message instead of guessing.
/// - `!takeover <slot>` (a single token that isn't a known group name) is
///   read as a slot name within the sender's own single group.
///
/// The full explicit syntax (`!takeover <group> [<slot>] [week <N>]`) keeps
/// working unchanged — it's just the first two branches below, same as
/// `resolve_group_and_slot`.
fn resolve_takeover_target(
    state: &crate::state::State,
    sender_person_id: &PersonId,
    rest: &[&str],
    year: i32,
    week: u32,
    interval: u32,
) -> std::result::Result<(GroupId, usize), String> {
    fn own_group(
        state: &crate::state::State,
        sender_person_id: &PersonId,
    ) -> std::result::Result<CleaningGroup, String> {
        match state.groups_for_person(sender_person_id).as_slice() {
            [] => Err("You are not in any cleaning group. Specify one: !takeover <group> [<slot>]".into()),
            [g] => Ok((*g).clone()),
            many => Err(format!(
                "You are in multiple groups — specify one: {}\nUsage: !takeover <group> [<slot>] [week <N>]",
                many.iter().map(|g| g.name.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    let (group, slot_token): (CleaningGroup, Option<&str>) = match rest.first() {
        Some(&first) if state.group_by_name(first).is_some() => (
            state.group_by_name(first).unwrap().clone(),
            rest.get(1).copied(),
        ),
        Some(&first) => (own_group(state, sender_person_id)?, Some(first)),
        None => (own_group(state, sender_person_id)?, None),
    };

    let slot_index = match slot_token {
        Some(name) => {
            if !group.is_multi_slot() {
                return Err(format!("«{}» does not have slots.", group.name));
            }
            match group
                .slots
                .iter()
                .position(|s| s.name.eq_ignore_ascii_case(name))
            {
                Some(idx) => idx,
                None => {
                    return Err(format!(
                        "«{name}» is not a group or a slot of «{}». Slots: {}",
                        group.name,
                        group
                            .slots
                            .iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
        }
        None => auto_pick_takeover_slot(state, &group, sender_person_id, year, week, interval)?,
    };
    Ok((group.id.clone(), slot_index))
}

/// Auto-pick the slot for a group/week when `!takeover` wasn't given one
/// explicitly: 0 for a single-slot group, or the sole takeable slot of a
/// multi-slot group. Never guesses between several — returns the choices
/// (or "nothing to take over") as a message instead.
fn auto_pick_takeover_slot(
    state: &crate::state::State,
    group: &CleaningGroup,
    sender_person_id: &PersonId,
    year: i32,
    week: u32,
    interval: u32,
) -> std::result::Result<usize, String> {
    if !group.is_multi_slot() {
        return Ok(0);
    }
    let candidates: Vec<usize> = group
        .slots
        .iter()
        .enumerate()
        .filter(|(i, slot)| {
            !state.is_slot_completed(&group.id, &slot.id, year, week)
                && state
                    .slot_assignee(group, *i, year, week, interval)
                    .is_none_or(|p| &p.id != sender_person_id)
        })
        .map(|(i, _)| i)
        .collect();
    match candidates.as_slice() {
        [] => Err(format!(
            "Nothing to take over in «{}» this week — every slot is already done, skipped, or already yours.",
            group.name
        )),
        [i] => Ok(*i),
        many => Err(format!(
            "«{}» has multiple open slots this week: {}\nSpecify one: !takeover {} <slot>",
            group.name,
            many.iter().map(|&i| group.slots[i].name.as_str()).collect::<Vec<_>>().join(", "),
            group.name,
        )),
    }
}

fn validate_matrix_user_id(mxid: &str) -> std::result::Result<(), String> {
    OwnedUserId::try_from(mxid)
        .map(|_| ())
        .map_err(|_| format!("«{mxid}» is not a valid Matrix user ID. Use @user:server."))
}

fn freeze_schedule_before_join(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &GroupId,
) -> anyhow::Result<()> {
    let (cur_y, cur_w) = current_iso_week();
    let interval = ctx.config.schedule.interval_weeks;
    for event in resolver::materialize(state, interval, 1) {
        // Apply this group's active-week pick (so it's locked in before the
        // join can affect it) AND the queue advancement that produced it —
        // otherwise whoever got picked would stay stuck at the queue's front
        // and get reused for the very next week too.
        let applies = match &event {
            DomainEvent::SlotAssigned {
                group_id: g,
                iso_year,
                iso_week,
                ..
            } => g == group_id && *iso_year == cur_y && *iso_week == cur_w,
            DomainEvent::RotationQueueSet { group_id: g, .. } => g == group_id,
            _ => false,
        };
        if applies {
            state.apply_event(event)?;
        }
    }
    Ok(())
}

/// Freezes the active week as explicitly unassigned for a group that just
/// got its first member, so their real first turn starts next week instead
/// of a week already underway. Deliberately does NOT apply the
/// `RotationQueueSet` that the underlying preview-materialize would have
/// produced (it would have advanced the queue past this person) — the queue
/// must stay exactly as `apply_group_join` left it (them at the front) so
/// they're picked again, for real, next week.
fn keep_active_week_unassigned_for_first_member(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &GroupId,
) -> anyhow::Result<()> {
    let (cur_y, cur_w) = current_iso_week();
    let interval = ctx.config.schedule.interval_weeks;
    let active_placeholders: Vec<DomainEvent> = resolver::materialize(state, interval, 1)
        .into_iter()
        .filter_map(|event| match event {
            DomainEvent::SlotAssigned {
                group_id: event_group_id,
                slot_index,
                iso_year,
                iso_week,
                source,
                ..
            } if event_group_id == *group_id && iso_year == cur_y && iso_week == cur_w => {
                Some(DomainEvent::SlotAssigned {
                    group_id: event_group_id,
                    slot_index,
                    iso_year,
                    iso_week,
                    person_id: None,
                    source,
                    actor_id: None,
                    previous_person_id: None,
                })
            }
            _ => None,
        })
        .collect();
    for event in active_placeholders {
        state.apply_event(event)?;
    }
    Ok(())
}

fn current_open_assignments(
    state: &crate::state::State,
    group_id: &GroupId,
    person_id: &PersonId,
    interval: u32,
) -> Vec<String> {
    let (year, week) = current_iso_week();
    let Some(group) = state.group_by_id(group_id) else {
        return Vec::new();
    };
    let has_frozen_current = state.slot_assignments.iter().any(|assignment| {
        assignment.group_id == *group_id
            && assignment.iso_year == year
            && assignment.iso_week == week
    });
    if !has_frozen_current && !state.is_due(group_id, year, week, interval) {
        return Vec::new();
    }

    if group.is_multi_slot() {
        group
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| !state.is_slot_completed(group_id, &slot.id, year, week))
            .filter_map(|(index, slot)| {
                state
                    .slot_assignee(group, index, year, week, interval)
                    .filter(|person| &person.id == person_id)
                    .map(|_| slot.name.clone())
            })
            .collect()
    } else if !state.is_completed(group_id, year, week)
        && state
            .responsible_person(group, year, week, interval)
            .is_some_and(|person| &person.id == person_id)
    {
        vec![group.name.clone()]
    } else {
        Vec::new()
    }
}

fn person_label(person: &Person) -> String {
    match person.matrix_id.as_deref() {
        Some(mxid) if person.display_name != mxid => format!("{} ({mxid})", person.display_name),
        Some(mxid) => mxid.to_owned(),
        None => format!("{} (no Matrix)", person.display_name),
    }
}

fn next_assignment_summary(state: &crate::state::State, group_id: &GroupId) -> String {
    let (cur_y, cur_w) = current_iso_week();
    let Some(group) = state.group_by_id(group_id) else {
        return "Next: unavailable.".to_owned();
    };
    if group.member_ids.is_empty() {
        return "Next: rotation is empty.".to_owned();
    }

    let next_week = state
        .slot_assignments
        .iter()
        .filter(|a| {
            a.group_id == *group_id
                && (a.iso_year > cur_y || (a.iso_year == cur_y && a.iso_week > cur_w))
        })
        .map(|a| (a.iso_year, a.iso_week))
        .min();

    let Some((year, week)) = next_week else {
        return "Next: no future assignment is materialized.".to_owned();
    };
    let mut names: Vec<String> = state
        .slot_assignments
        .iter()
        .filter(|a| a.group_id == *group_id && a.iso_year == year && a.iso_week == week)
        .filter_map(|a| a.person_id.as_ref())
        .filter_map(|id| state.person_by_id(id))
        .map(person_label)
        .collect();
    names.dedup();
    let who = if names.is_empty() {
        "unassigned".to_owned()
    } else {
        names.join(", ")
    };
    let dates = week_dates(year, week);
    format!("Next: {who} · week {week} ({dates}).")
}

/// Fetch Matrix display names for all known Matrix users and update state.
/// Keeps Person.display_name in sync with the real Matrix profile name.
/// Called before PDF generation and whenever a command comes in.
async fn refresh_display_names(ctx: &BotContext, room: &Room) {
    let mxids: Vec<String> = ctx
        .state
        .lock()
        .await
        .persons
        .iter()
        .filter_map(|p| p.matrix_id.clone())
        .collect();
    if mxids.is_empty() {
        return;
    }
    let refs: Vec<&str> = mxids.iter().map(String::as_str).collect();
    let fetched = format::fetch_names(room, &refs).await;
    if fetched.is_empty() {
        return;
    }
    let mut state = ctx.state.lock().await;
    for p in &mut state.persons {
        if let Some(mxid) = &p.matrix_id {
            if let Some(name) = fetched.get(mxid.as_str()) {
                if !name.is_empty() && name != mxid {
                    p.display_name = name.clone();
                }
            }
        }
    }
}

// ── !done [group] ─────────────────────────────────────────────────────────────

async fn cmd_done(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let (year, week) = current_iso_week();
    let sender_mxid = sender.as_str();
    let mut state = ctx.state.lock().await;

    // Resolve sender to a Person.
    let sender_person_id = match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
        Some(id) => id,
        None => {
            return Ok(Some(format!(
                "You are not registered. Ask an admin to run !adduser {sender_mxid} <group>."
            )))
        }
    };

    // Determine target group(s).
    let target_group_ids: Vec<String> = if let Some(name) = args.first() {
        match state.group_by_name(name) {
            Some(g) => vec![g.id.clone()],
            None => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        state
            .groups_for_person(&sender_person_id)
            .iter()
            .map(|g| g.id.clone())
            .collect()
    };

    if target_group_ids.is_empty() {
        return Ok(Some("You are not assigned to any cleaning group.".into()));
    }

    let interval = ctx.config.schedule.interval_weeks;
    let mut marked = vec![];
    let mut already_done = vec![];

    for group_id in &target_group_ids {
        let group = state.group_by_id(group_id).unwrap().clone();
        let is_member = group.member_ids.contains(&sender_person_id);
        // Whoever currently holds *any* slot for this week may mark it done,
        // even if they're not a formal member — covers a takeover (!takeover,
        // !assign) or an accepted swap, both of which update the same frozen
        // assignment `!done` reads here. Based on the *current* assignment,
        // never the original round-robin pick.
        let is_current_assignee = if group.is_multi_slot() {
            group.slots.iter().enumerate().any(|(i, _)| {
                state
                    .slot_assignee(&group, i, year, week, interval)
                    .is_some_and(|p| p.id == sender_person_id)
            })
        } else {
            state
                .responsible_person(&group, year, week, interval)
                .is_some_and(|p| p.id == sender_person_id)
        };
        if !is_member && !is_current_assignee {
            return Ok(Some(format!("You are not a member of «{}».", group.name)));
        }

        if group.is_multi_slot() {
            // Mark the slot(s) assigned to this person.
            let slot_assignments: Vec<(String, String)> = group
                .slots
                .iter()
                .enumerate()
                .filter_map(|(slot_idx, slot)| {
                    let assignee = state.slot_assignee(&group, slot_idx, year, week, interval)?;
                    if assignee.id == sender_person_id {
                        Some((slot.id.clone(), slot.name.clone()))
                    } else {
                        None
                    }
                })
                .collect();

            if slot_assignments.is_empty() {
                let name = group.name.clone();
                return Ok(Some(format!(
                    "You are not assigned to any slot in «{name}» this week."
                )));
            }

            for (slot_id, slot_name) in slot_assignments {
                if state.is_slot_completed(group_id, &slot_id, year, week) {
                    already_done.push(format!("{} / {slot_name}", group.name));
                    continue;
                }
                let responsible_ids = vec![sender_person_id.clone()];
                state.apply_event(DomainEvent::CleaningCompleted {
                    group_id: group_id.clone(),
                    slot_id: Some(slot_id),
                    person_id: sender_person_id.clone(),
                    responsible_person_ids: responsible_ids,
                    iso_year: year,
                    iso_week: week,
                })?;
                // Report group as fully done only when all slots complete.
                if state.is_completed(group_id, year, week) {
                    marked.push(format!("{} ✅ fully done", group.name));
                } else {
                    marked.push(format!("{} / {slot_name}", group.name));
                }
            }
        } else {
            if state.is_completed(group_id, year, week) {
                already_done.push(group.name.clone());
                continue;
            }
            let responsible_ids: Vec<String> = state
                .responsible_person(&group, year, week, interval)
                .map(|p| vec![p.id.clone()])
                .unwrap_or_default();
            state.apply_event(DomainEvent::CleaningCompleted {
                group_id: group_id.clone(),
                slot_id: None,
                person_id: sender_person_id.clone(),
                responsible_person_ids: responsible_ids,
                iso_year: year,
                iso_week: week,
            })?;
            marked.push(group.name.clone());
        }
    }

    state.save(&ctx.state_path).await?;
    drop(state);

    let mut lines = vec![];
    if !marked.is_empty() {
        lines.push(format!("✅ Cleaned: {}", marked.join(", ")));
    }
    if !already_done.is_empty() {
        lines.push(format!("Already done: {}", already_done.join(", ")));
    }
    Ok(Some(lines.join("\n")))
}

// ── !status ───────────────────────────────────────────────────────────────────

async fn cmd_status(ctx: &BotContext) -> Result<Option<String>> {
    let (year, week) = current_iso_week();
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;

    let active_groups: Vec<_> = state
        .cleaning_groups
        .iter()
        .filter(|g| g.is_active)
        .collect();
    if active_groups.is_empty() {
        return Ok(Some("No active cleaning groups configured yet.".into()));
    }

    let mut lines = vec![format!(
        "📋 **Cleaning status** · week {week} ({})",
        week_dates(year, week)
    )];
    for group in &active_groups {
        let done = state.is_completed(&group.id, year, week);
        let icon = if done { "✅" } else { "❌" };
        let who = if done {
            state
                .completions
                .iter()
                .find(|c| c.group_id == group.id && c.iso_year == year && c.iso_week == week)
                .and_then(|c| state.person_by_id(&c.completed_by_id))
                .map(|p| format!(" · {}", p.display_name))
                .unwrap_or_default()
        } else {
            match state.responsible_person(group, year, week, interval) {
                Some(p) => format!(" · {}", person_key(p)),
                None => " · (nobody assigned)".into(),
            }
        };
        let rooms_str = group
            .rooms_text()
            .map(|r| format!("\n  {r}"))
            .unwrap_or_default();
        lines.push(format!("{icon} **{}**{who}{rooms_str}", group.name));
    }
    Ok(Some(lines.join("\n")))
}

// ── !stats [@user] ────────────────────────────────────────────────────────────

async fn cmd_stats(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let (start_y, start_w) = state.tracking_start();

    // Per-person view.
    if let Some(query) = args.first().copied() {
        let person_id = match state.find_person(query).map(|p| p.id.clone()) {
            Some(id) => id,
            None => return Ok(Some(format!("Person «{query}» not found."))),
        };
        let ps = match analytics::person_stats(&state, &person_id, interval) {
            Some(s) => s,
            None => return Ok(Some(format!("{query} is not in any cleaning group."))),
        };
        let mut completions: Vec<_> = state
            .completions
            .iter()
            .filter(|c| c.completed_by_id == person_id)
            .collect();
        completions.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));
        let pct = (ps.completion_rate * 100.0).round() as u32;
        let streak_str = if ps.streak >= 2 {
            format!(" 🔥{}", ps.streak)
        } else {
            String::new()
        };
        let mut lines = vec![
            format!(
                "📊 **Stats** · {} · since W{start_w} ({})",
                ps.display_name,
                week_dates(start_y, start_w)
            ),
            format!(
                "Group: {} · {}/{} ({}%){streak_str}",
                ps.group_names, ps.completed, ps.due_weeks, pct
            ),
            format!("Missed: {} · Skipped: {}", ps.missed, ps.skipped),
        ];
        if ps.swaps_given > 0 || ps.swaps_taken > 0 {
            lines.push(format!(
                "Swaps: given {} · taken {}",
                ps.swaps_given, ps.swaps_taken
            ));
        }
        if let Some(last) = completions.first() {
            lines.push(format!(
                "Last: week {} ({})",
                last.iso_week,
                week_dates(last.iso_year, last.iso_week)
            ));
        }
        if completions.len() > 1 {
            lines.push("Recent:".into());
            for c in completions.iter().take(5) {
                let gname = state
                    .group_by_id(&c.group_id)
                    .map(|g| g.name.as_str())
                    .unwrap_or("?");
                lines.push(format!(
                    "  • {gname} · week {} ({})",
                    c.iso_week,
                    week_dates(c.iso_year, c.iso_week)
                ));
            }
        }
        return Ok(Some(lines.join("\n")));
    }

    // Group summary view.
    let mut lines = vec![format!(
        "📊 **Cleaning stats** · since week {start_w} ({})",
        week_dates(start_y, start_w)
    )];
    let (cur_y, cur_w) = current_iso_week();
    let active_groups: Vec<_> = state
        .cleaning_groups
        .iter()
        .filter(|g| g.is_active)
        .collect();
    if active_groups.is_empty() {
        lines.push("  No active cleaning groups configured yet.".into());
        return Ok(Some(lines.join("\n")));
    }

    for group in &active_groups {
        let gs = match analytics::group_stats(&state, &group.id, interval) {
            Some(s) => s,
            None => continue,
        };
        let pct = (gs.completion_rate * 100.0).round() as u32;
        let this_week = state.is_completed(&group.id, cur_y, cur_w);
        lines.push(String::new());
        lines.push(format!("🏢 {} ({} members)", group.name, gs.member_count));
        lines.push(format!(
            "Completed: {}/{} ({pct}%) · Missed: {}",
            gs.completed, gs.due_weeks, gs.missed
        ));
        lines.push(format!(
            "Streak: {} · This week: {}",
            gs.current_streak,
            if this_week { "✅" } else { "❌" }
        ));
        if let Some(last) = state.last_completion(&group.id) {
            let by = state
                .person_by_id(&last.completed_by_id)
                .map(|p| p.display_name.as_str())
                .unwrap_or("?");
            lines.push(format!(
                "Last: week {} ({}) by {by}",
                last.iso_week,
                week_dates(last.iso_year, last.iso_week)
            ));
        }
        // Per-member counts
        for pid in &group.member_ids {
            if let Some(p) = state.person_by_id(pid) {
                let cnt = state
                    .completions
                    .iter()
                    .filter(|c| c.group_id == group.id && c.completed_by_id == *pid && !c.skipped)
                    .count();
                lines.push(format!("  {}: {cnt}", p.display_name));
            }
        }
    }
    Ok(Some(lines.join("\n")))
}

// ── !floors / !areas ─────────────────────────────────────────────────────────

async fn cmd_floors(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let active: Vec<_> = state
        .cleaning_groups
        .iter()
        .filter(|g| g.is_active)
        .collect();
    if active.is_empty() {
        return Ok(Some("No active cleaning groups configured.".into()));
    }
    let mut lines = vec!["🏢 Cleaning areas:".to_owned()];
    for group in &active {
        let members_text = if group.member_ids.is_empty() {
            "(no members)".to_owned()
        } else {
            group
                .member_ids
                .iter()
                .filter_map(|id| state.person_by_id(id))
                .map(|p| {
                    if p.matrix_id.is_some() {
                        p.display_name.clone()
                    } else {
                        format!("{} (no Matrix)", p.display_name)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let rooms_str = if group.room_names.is_empty() {
            String::new()
        } else {
            format!(" · {}", group.room_names.join(", "))
        };
        lines.push(format!("  • **{}**: {members_text}{rooms_str}", group.name));
    }
    Ok(Some(lines.join("\n")))
}

// ── !joinfloor <group> ────────────────────────────────────────────────────────

async fn cmd_joinfloor(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let group_name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !joingroup <group>".into())),
    };
    let mxid = sender.as_str();
    let mut state = ctx.state.lock().await;

    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    // PersonCreated is idempotent — safe even if this Matrix user already exists.
    let new_person_id = uuid::Uuid::new_v4().to_string();
    state.apply_event(DomainEvent::PersonCreated {
        person_id: new_person_id,
        display_name: mxid.to_owned(),
        matrix_id: Some(mxid.to_owned()),
    })?;
    let person_id = state.person_by_matrix_id(mxid).unwrap().id.clone();
    if state
        .group_by_id(&group_id)
        .map(|g| g.member_ids.contains(&person_id))
        .unwrap_or(false)
    {
        return Ok(Some(format!("You are already in «{group_name}».")));
    }
    apply_group_join(ctx, &mut state, &group_id, &person_id)?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Joined «{group_name}».")))
}

// ── !leavefloor <group> ───────────────────────────────────────────────────────

async fn cmd_leavefloor(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let group_name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !leavegroup <group>".into())),
    };
    let mxid = sender.as_str();
    let mut state = ctx.state.lock().await;

    let person_id = match state.person_by_matrix_id(mxid).map(|p| p.id.clone()) {
        Some(id) => id,
        None => return Ok(Some("You are not registered in any group.".into())),
    };
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    if !state
        .group_by_id(&group_id)
        .map(|g| g.member_ids.contains(&person_id))
        .unwrap_or(false)
    {
        return Ok(Some(format!("You are not in «{group_name}».")));
    }
    let open = current_open_assignments(
        &state,
        &group_id,
        &person_id,
        ctx.config.schedule.interval_weeks,
    );
    if !open.is_empty() {
        return Ok(Some(format!(
            "You cannot leave «{group_name}» while your current assignment is open ({}). \
             Complete or skip it first.",
            open.join(", ")
        )));
    }
    state.apply_event(DomainEvent::PersonLeftGroup {
        person_id: person_id.clone(),
        group_id: group_id.clone(),
    })?;
    apply_group_departure(ctx, &mut state, &person_id, &group_id)?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Left «{group_name}».")))
}

// ── Rotation management ───────────────────────────────────────────────────────

async fn cmd_cleaning(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let subcommand = args.first().map(|value| value.to_ascii_lowercase());
    match subcommand.as_deref() {
        Some("add") => add_matrix_participant(ctx, sender, &args[1..]).await,
        Some("remove") => remove_matrix_participant(ctx, sender, &args[1..]).await,
        Some("people") => cmd_cleaning_people(ctx, &args[1..]).await,
        _ => Ok(Some(
            "Usage: !cleaning add @user:server <group> | remove @user:server <group> | people [group]"
                .to_owned(),
        )),
    }
}

async fn cmd_cleaning_people(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let groups: Vec<&CleaningGroup> = match args.first() {
        Some(group_name) => match state.group_by_name(group_name) {
            Some(group) => vec![group],
            None => return Ok(Some(format!("Group «{group_name}» not found."))),
        },
        None => state
            .cleaning_groups
            .iter()
            .filter(|group| group.is_active)
            .collect(),
    };

    if groups.is_empty() {
        return Ok(Some("No active cleaning groups.".to_owned()));
    }

    let mut sections = Vec::new();
    for group in groups {
        let mut lines = vec![format!("🔁 **{}**", group.name)];
        if group.member_ids.is_empty() {
            lines.push("Rotation is empty.".to_owned());
        } else {
            for (index, person_id) in group.member_ids.iter().enumerate() {
                let label = state
                    .person_by_id(person_id)
                    .map(person_label)
                    .unwrap_or_else(|| format!("unknown ({person_id})"));
                lines.push(format!("{}. {label}", index + 1));
            }
            lines.push(next_assignment_summary(&state, &group.id));
        }
        sections.push(lines.join("\n"));
    }
    Ok(Some(sections.join("\n\n")))
}

async fn add_matrix_participant(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (mxid, group_name) = match (args.first(), args.get(1)) {
        (Some(mxid), Some(group)) => (*mxid, *group),
        _ => return Ok(Some("Usage: !cleaning add @user:server <group>".to_owned())),
    };
    if let Err(message) = validate_matrix_user_id(mxid) {
        return Ok(Some(message));
    }

    // The lock intentionally covers validation, mutation, rescheduling and save.
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(group_name) {
        Some(group) => group.id.clone(),
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };

    if let Some(person) = state.person_by_matrix_id(mxid) {
        if state
            .group_by_id(&group_id)
            .is_some_and(|group| group.member_ids.contains(&person.id))
        {
            return Ok(Some(format!(
                "{mxid} is already in «{group_name}». No changes made."
            )));
        }
    } else {
        state.apply_event(DomainEvent::PersonCreated {
            person_id: Uuid::new_v4().to_string(),
            display_name: mxid.to_owned(),
            matrix_id: Some(mxid.to_owned()),
        })?;
    }

    let person_id = state
        .person_by_matrix_id(mxid)
        .expect("validated Matrix person must exist")
        .id
        .clone();
    apply_group_join(ctx, &mut state, &group_id, &person_id)?;
    let next = next_assignment_summary(&state, &group_id);
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "✅ Added {mxid} to «{group_name}».\n\
         Takes effect from the next open week; already-planned weeks are unchanged.\n{next}"
    )))
}

async fn remove_matrix_participant(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (mxid, group_name) = match (args.first(), args.get(1)) {
        (Some(mxid), Some(group)) => (*mxid, *group),
        _ => {
            return Ok(Some(
                "Usage: !cleaning remove @user:server <group>".to_owned(),
            ))
        }
    };
    if let Err(message) = validate_matrix_user_id(mxid) {
        return Ok(Some(message));
    }

    let mut state = ctx.state.lock().await;
    let person_id = match state.person_by_matrix_id(mxid) {
        Some(person) => person.id.clone(),
        None => return Ok(Some(format!("{mxid} is not registered. No changes made."))),
    };
    let group_id = match state.group_by_name(group_name) {
        Some(group) => group.id.clone(),
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    if !state
        .group_by_id(&group_id)
        .is_some_and(|group| group.member_ids.contains(&person_id))
    {
        return Ok(Some(format!(
            "{mxid} is not in «{group_name}». No changes made."
        )));
    }

    let open = current_open_assignments(
        &state,
        &group_id,
        &person_id,
        ctx.config.schedule.interval_weeks,
    );
    if !open.is_empty() {
        return Ok(Some(format!(
            "Cannot remove {mxid} from «{group_name}»: their current assignment is still open \
             ({}). Complete, skip, or manually reassign it first. No changes made.",
            open.join(", ")
        )));
    }

    state.apply_event(DomainEvent::PersonLeftGroup {
        person_id: person_id.clone(),
        group_id: group_id.clone(),
    })?;
    let refilled = apply_group_departure(ctx, &mut state, &person_id, &group_id)?;
    let next = next_assignment_summary(&state, &group_id);
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "✅ Removed {mxid} from «{group_name}».\n\
         Current, completed, and other members' future assignments were preserved. \
         Refilled {refilled} vacated week(s).\n{next}"
    )))
}

// ── Admin: legacy Matrix participant aliases ──────────────────────────────────

async fn cmd_adduser(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    add_matrix_participant(ctx, sender, args).await
}

async fn cmd_removeuser(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    remove_matrix_participant(ctx, sender, args).await
}

// ── Admin: !addperson <name> <group> ─────────────────────────────────────────

async fn cmd_addperson(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (name, group_name) = match (args.first(), args.get(1)) {
        (Some(n), Some(f)) => (n.to_string(), f.to_string()),
        _ => return Ok(Some("Usage: !addperson <display_name> <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    let person_id = if let Some(person) = state.find_person(&name) {
        person.id.clone()
    } else {
        state.apply_event(DomainEvent::PersonCreated {
            person_id: Uuid::new_v4().to_string(),
            display_name: name.clone(),
            matrix_id: None,
        })?;
        state
            .find_person(&name)
            .expect("created person must exist")
            .id
            .clone()
    };
    if state
        .group_by_id(&group_id)
        .map(|g| g.member_ids.contains(&person_id))
        .unwrap_or(false)
    {
        return Ok(Some(format!(
            "{name} is already in «{group_name}». No changes made."
        )));
    }
    apply_group_join(ctx, &mut state, &group_id, &person_id)?;
    let next = next_assignment_summary(&state, &group_id);
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "✅ Added {name} (no Matrix) to «{group_name}».\n\
         Takes effect from the next open week; already-planned weeks are unchanged.\n{next}"
    )))
}

// ── Admin: !removeperson <name> <group> ──────────────────────────────────────

async fn cmd_removeperson(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (query, group_name) = match (args.first(), args.get(1)) {
        (Some(n), Some(f)) => (n.to_string(), f.to_string()),
        _ => return Ok(Some("Usage: !removeperson <name> <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let person_id = match state.find_person(&query).map(|p| p.id.clone()) {
        Some(id) => id,
        None => return Ok(Some(format!("Person «{query}» not found."))),
    };
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    if !state
        .group_by_id(&group_id)
        .map(|g| g.member_ids.contains(&person_id))
        .unwrap_or(false)
    {
        return Ok(Some(format!(
            "{query} is not in «{group_name}». No changes made."
        )));
    }
    let open = current_open_assignments(
        &state,
        &group_id,
        &person_id,
        ctx.config.schedule.interval_weeks,
    );
    if !open.is_empty() {
        return Ok(Some(format!(
            "Cannot remove {query} from «{group_name}»: their current assignment is still open \
             ({}). Complete, skip, or manually reassign it first. No changes made.",
            open.join(", ")
        )));
    }
    state.apply_event(DomainEvent::PersonLeftGroup {
        person_id: person_id.clone(),
        group_id: group_id.clone(),
    })?;
    let refilled = apply_group_departure(ctx, &mut state, &person_id, &group_id)?;
    let next = next_assignment_summary(&state, &group_id);
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "✅ Removed {query} from «{group_name}».\n\
         Current, completed, and other members' future assignments were preserved. \
         Refilled {refilled} vacated week(s).\n{next}"
    )))
}

// ── Admin: !linkmatrix <name> <@user:server> ─────────────────────────────────
//
// Normally refuses when `name` already has a Matrix ID linked — but if that
// *existing* matrix_id doesn't even parse as a valid Matrix user ID (data
// corruption: a manual edit, an old bug, ...), there is nothing legitimate to
// protect, so this repairs it in place instead of refusing. A person whose
// existing matrix_id already parses correctly is never touched this way —
// only an invalid one may be replaced, never a valid one overwritten by
// another.

async fn cmd_linkmatrix(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (name, mxid) = match (args.first(), args.get(1)) {
        (Some(n), Some(m)) => (n.to_string(), m.to_string()),
        _ => {
            return Ok(Some(
                "Usage: !linkmatrix <display_name> <@user:server>".into(),
            ))
        }
    };
    if !mxid.starts_with('@') || !mxid.contains(':') {
        return Ok(Some(format!(
            "«{mxid}» does not look like a Matrix ID (@user:server)."
        )));
    }

    // Fetch the Matrix display name immediately so the record looks the same
    // as one created via !adduser from the start. This is the only step that
    // needs a live `Room` — everything else is pure state mutation, split out
    // into `apply_linkmatrix` so that logic (including the repair path) is
    // directly unit-testable without a `Room`.
    let fetched = format::fetch_names(room, &[mxid.as_str()]).await;
    let display_name = fetched
        .get(mxid.as_str())
        .filter(|n| !n.is_empty() && n.as_str() != mxid.as_str())
        .cloned();

    apply_linkmatrix(ctx, &name, &mxid, display_name.as_deref()).await
}

/// True when `person_id` shows any sign of actually being used — group
/// membership, a slot assignment, or a completion — as opposed to an empty
/// placeholder (e.g. a stub created by the greeting reaction that nobody
/// ever finished onboarding). Used to disambiguate between several people
/// sharing a display name; deliberately broader than the stub-merge check
/// below (which only looks at completions), since a mere "which of these
/// same-named records is actually somebody" question should also count
/// group membership and open assignments as "real".
fn person_has_activity(state: &crate::state::State, person_id: &str) -> bool {
    state
        .cleaning_groups
        .iter()
        .any(|g| g.member_ids.iter().any(|m| m == person_id))
        || state
            .slot_assignments
            .iter()
            .any(|a| a.person_id.as_deref() == Some(person_id))
        || state
            .completions
            .iter()
            .any(|c| c.completed_by_id == person_id)
}

/// Room-independent core of `!linkmatrix`: resolves `name`, decides whether
/// to link fresh, repair an invalid existing `matrix_id`, or refuse, then
/// performs the stub auto-merge and the actual `PersonMatrixLinked` event —
/// everything `cmd_linkmatrix` does except the live display-name fetch
/// (passed in as `fetched_display_name` so this stays testable without a
/// `Room`).
async fn apply_linkmatrix(
    ctx: &BotContext,
    name: &str,
    mxid: &str,
    fetched_display_name: Option<&str>,
) -> Result<Option<String>> {
    let (person_id, was_repair) = {
        let state = ctx.state.lock().await;

        // `name` can match more than one person (e.g. a real participant
        // with a corrupted matrix_id and an unrelated stub that happens to
        // share their display name) — `find_person` alone would just return
        // whichever comes first in storage order, which is exactly what let
        // an unrelated already-linked stub silently block repairing the real
        // participant. Consider every match instead.
        let matches: Vec<&Person> = state
            .persons
            .iter()
            .filter(|p| p.id == name || p.matches(name))
            .collect();
        let Some(&first) = matches.first() else {
            return Ok(Some(format!("No person named «{name}» found.")));
        };

        // Only records whose *current* matrix_id is missing or doesn't even
        // parse are candidates for linking/repair — a match that already has
        // a valid, different matrix_id is never a target for this command.
        let repairable: Vec<&Person> = matches
            .iter()
            .filter(|p| {
                p.matrix_id
                    .as_deref()
                    .is_none_or(|m| validate_matrix_user_id(m).is_err())
            })
            .copied()
            .collect();
        let Some(&chosen) = repairable.first() else {
            return Ok(Some(format!(
                "«{}» already has a Matrix account linked.",
                first.display_name
            )));
        };

        let chosen = if repairable.len() == 1 {
            chosen
        } else {
            // More than one same-named record needs a link. Auto-pick only
            // if exactly one of them shows real activity — an empty
            // placeholder among them is skipped, never preferred. Two (or
            // more) with real activity is genuine ambiguity: refuse rather
            // than silently guess which one the admin meant.
            let real: Vec<&Person> = repairable
                .iter()
                .copied()
                .filter(|p| person_has_activity(&state, &p.id))
                .collect();
            match real.as_slice() {
                [only] => *only,
                [] => chosen, // none has activity — equally arbitrary, pick deterministically (first by storage order)
                _ => {
                    let ids = real
                        .iter()
                        .map(|p| p.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Ok(Some(format!(
                        "Multiple people named «{name}» need a Matrix link and more than one has real \
                         activity (group membership, an assignment, or a completion) — refusing to guess. \
                         Re-run with the exact PersonId instead of the name: {ids}"
                    )));
                }
            }
        };

        let is_repair = chosen.matrix_id.is_some();
        (chosen.id.clone(), is_repair)
    };

    let mut state = ctx.state.lock().await;

    // Auto-merge: if the MXID belongs to a stub person created by the greeting
    // reaction (no cleaning history), remove it so the link can proceed cleanly.
    if let Some(stub_id) = state.person_by_matrix_id(mxid).map(|p| p.id.clone()) {
        let has_history = state
            .completions
            .iter()
            .any(|c| c.completed_by_id == stub_id);
        if has_history {
            return Ok(Some(format!(
                "{mxid} is linked to another person who already has cleaning history. Cannot auto-merge."
            )));
        }
        let stub_group_ids: Vec<String> = state
            .cleaning_groups
            .iter()
            .filter(|g| g.member_ids.contains(&stub_id))
            .map(|g| g.id.clone())
            .collect();
        for gid in &stub_group_ids {
            state.apply_event(DomainEvent::PersonLeftGroup {
                person_id: stub_id.clone(),
                group_id: gid.clone(),
            })?;
            apply_group_departure(ctx, &mut state, &stub_id, gid)?;
        }
        state.persons.retain(|p| p.id != stub_id);
    }

    state.apply_event(DomainEvent::PersonMatrixLinked {
        person_id: person_id.clone(),
        matrix_id: mxid.to_owned(),
    })?;
    if let Some(dn) = fetched_display_name {
        if let Some(p) = state.persons.iter_mut().find(|p| p.id == person_id) {
            p.display_name = dn.to_owned();
        }
    }
    state.save(&ctx.state_path).await?;

    let shown = fetched_display_name.unwrap_or(name);
    let verb = if was_repair { "repaired" } else { "linked" };
    Ok(Some(format!(
        "✅ {shown} ({mxid}) {verb}. All previous history preserved."
    )))
}

// ── Admin: !addfloor <name> ───────────────────────────────────────────────────

async fn cmd_addfloor(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !addgroup <name>".into())),
    };
    let mut state = ctx.state.lock().await;
    if state.group_by_name(&name).is_some() {
        return Ok(Some(format!("Group «{name}» already exists.")));
    }
    let group_id = uuid::Uuid::new_v4().to_string();
    state.apply_event(DomainEvent::GroupCreated {
        group_id,
        name: name.clone(),
    })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Created cleaning group «{name}».")))
}

// ── Admin: !removefloor <name> ────────────────────────────────────────────────

async fn cmd_removefloor(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !removegroup <name>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&name) {
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{name}» not found."))),
    };
    state.apply_event(DomainEvent::GroupDeleted { group_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Removed group «{name}».")))
}

// ── Admin: !addslot <group> <slot_name> ──────────────────────────────────────

// ── Admin: !resetplan <group> ─────────────────────────────────────────────────
// Clears all future (>= today) slot assignments for a group and rematerializes.
// Use this after the initial setup when you've added all members and want the
// rotation to distribute fairly from now on.  Safe to run at any time — past
// completed weeks are never touched.

async fn cmd_resetplan(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let group_name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !resetplan <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    reset_and_rematerialize(ctx, &mut state, &group_id)?;
    let assignee = {
        let interval = ctx.config.schedule.interval_weeks;
        let (cur_y, cur_w) = current_iso_week();
        let g = state.group_by_id(&group_id).unwrap().clone();
        state
            .responsible_person(&g, cur_y, cur_w, interval)
            .map(|p| p.display_name.clone())
            .unwrap_or_else(|| "(nobody)".into())
    };
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "✅ Plan reset for «{group_name}». This week: {assignee}."
    )))
}

async fn cmd_addslot(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (group_name, slot_name) = match (args.first(), args.get(1..).map(|s| s.join(" "))) {
        (Some(g), Some(s)) if !s.is_empty() => (g.to_string(), s),
        _ => return Ok(Some("Usage: !addslot <group> <slot_name>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    if state
        .group_by_id(&group_id)
        .and_then(|g| g.slot_by_name(&slot_name))
        .is_some()
    {
        return Ok(Some(format!(
            "Slot «{slot_name}» already exists in «{group_name}»."
        )));
    }
    let slot_id = uuid::Uuid::new_v4().to_string();
    state.apply_event(DomainEvent::SlotAdded {
        group_id,
        slot_id,
        slot_name: slot_name.clone(),
    })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "✅ Added slot «{slot_name}» to «{group_name}»."
    )))
}

// ── Admin: !removeslot <group> <slot_name> ───────────────────────────────────

async fn cmd_removeslot(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (group_name, slot_name) = match (args.first(), args.get(1..).map(|s| s.join(" "))) {
        (Some(g), Some(s)) if !s.is_empty() => (g.to_string(), s),
        _ => return Ok(Some("Usage: !removeslot <group> <slot_name>".into())),
    };
    let mut state = ctx.state.lock().await;
    let (group_id, slot_id) = match state.group_by_name(&group_name) {
        Some(g) => match g.slot_by_name(&slot_name) {
            Some(s) => (g.id.clone(), s.id.clone()),
            None => {
                return Ok(Some(format!(
                    "Slot «{slot_name}» not found in «{group_name}»."
                )))
            }
        },
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    state.apply_event(DomainEvent::SlotRemoved { group_id, slot_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "✅ Removed slot «{slot_name}» from «{group_name}»."
    )))
}

// ── Admin: !addroom <group> [<slot>] <room> ──────────────────────────────────
//
// If the group has slots and the second argument matches a slot name, the room
// is added to that slot.  Otherwise the room is added to the group directly
// (single-slot mode, or a group-level room for backwards compatibility).

async fn cmd_addroom(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    if args.len() < 2 {
        return Ok(Some("Usage: !addroom <group> [<slot>] <room name>".into()));
    }
    let group_name = args[0].to_string();
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };

    // Detect slot targeting: if args[1] matches a slot name and there are more args, route to slot.
    let (slot_id, room_name) = {
        let g = state.group_by_id(&group_id).unwrap();
        if args.len() >= 3 {
            if let Some(slot) = g.slot_by_name(args[1]) {
                (Some(slot.id.clone()), args[2..].join(" "))
            } else {
                (None, args[1..].join(" "))
            }
        } else if g.is_multi_slot() {
            return Ok(Some(format!(
                "«{group_name}» has slots. Usage: !addroom \"{group_name}\" <slot_name> <room>.\nSlots: {}",
                g.slots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
            )));
        } else {
            (None, args[1..].join(" "))
        }
    };

    state.apply_event(DomainEvent::RoomAdded {
        group_id,
        slot_id,
        room_name: room_name.clone(),
    })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Added room «{room_name}».")))
}

// ── Admin: !removeroom <group> [<slot>] <room> ───────────────────────────────

async fn cmd_removeroom(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    if args.len() < 2 {
        return Ok(Some(
            "Usage: !removeroom <group> [<slot>] <room name>".into(),
        ));
    }
    let group_name = args[0].to_string();
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };

    let (slot_id, room_name) = {
        let g = state.group_by_id(&group_id).unwrap();
        if args.len() >= 3 {
            if let Some(slot) = g.slot_by_name(args[1]) {
                (Some(slot.id.clone()), args[2..].join(" "))
            } else {
                (None, args[1..].join(" "))
            }
        } else {
            (None, args[1..].join(" "))
        }
    };

    state.apply_event(DomainEvent::RoomRemoved {
        group_id,
        slot_id,
        room_name: room_name.clone(),
    })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "✅ Removed room «{room_name}» from «{group_name}»."
    )))
}

// ── !swap @target [group] [week N] ───────────────────────────────────────────

async fn cmd_swap(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let target_mxid = match args.first() {
        Some(t) => *t,
        None => return Ok(Some("Usage: !swap @user [group] [week <N>]".into())),
    };
    if !target_mxid.starts_with('@') {
        return Ok(Some(
            "Swap targets must be Matrix users (@user:server).".into(),
        ));
    }
    let sender_mxid = sender.as_str();
    let (cur_y, cur_w) = current_iso_week();

    let (group_args, (year, week)) = match extract_week_arg(&args[1..]) {
        Some(v) => v,
        None => return Ok(Some("Usage: !swap @user [group] [week <1-53>]".into())),
    };

    if (year, week) < (cur_y, cur_w) {
        return Ok(Some(format!(
            "Week {week} ({}) is in the past.",
            week_dates(year, week)
        )));
    }
    if sender_mxid == target_mxid {
        return Ok(Some("You cannot swap with yourself.".into()));
    }

    let mut state = ctx.state.lock().await;

    let sender_person_id = match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
        Some(id) => id,
        None => return Ok(Some(format!("You ({sender_mxid}) are not registered."))),
    };

    let group_id = if let Some(name) = group_args.first() {
        match state.group_by_name(name) {
            Some(g) => g.id.clone(),
            None => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        match state
            .groups_for_person(&sender_person_id)
            .first()
            .map(|g| g.id.clone())
        {
            Some(id) => id,
            None => {
                return Ok(Some(
                    "You are not in any group. Specify: !swap @user <group>".into(),
                ))
            }
        }
    };

    let group = match state.group_by_id(&group_id) {
        Some(g) => g.clone(),
        None => return Ok(Some("Group not found.".into())),
    };

    // !swap/!acceptswap only ever write slot_index 0 (see cmd_acceptswap) —
    // fine for single-slot groups, but silently wrong for multi-slot ones
    // (it would overwrite whichever slot happens to be first, not the one
    // the requester actually holds). Point at the slot-aware commands instead.
    if group.is_multi_slot() {
        return Ok(Some(format!(
            "«{}» has multiple slots — !swap doesn't support slot selection. \
             Use !takeover {} <slot> or ask an admin for !assign instead.",
            group.name, group.name
        )));
    }

    if !group.member_ids.contains(&sender_person_id) {
        return Ok(Some(format!("You are not a member of «{}».", group.name)));
    }

    let dupe = state.swap_requests.iter().any(|s| {
        s.group_id == group_id
            && s.iso_year == year
            && s.iso_week == week
            && s.status == SwapStatus::Pending
            && s.requester == sender_mxid
    });
    if dupe {
        return Ok(Some(format!(
            "You already have a pending swap for «{}» week {week}.",
            group.name
        )));
    }

    state.apply_event(DomainEvent::SwapRequested {
        group_id: group_id.clone(),
        requester_mxid: sender_mxid.to_owned(),
        target_mxid: target_mxid.to_owned(),
        iso_year: year,
        iso_week: week,
    })?;
    // The swap ID was allocated inside apply_event.
    let id = state.swap_requests.last().map(|s| s.id).unwrap_or(0);
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "🔄 Swap #{id} · «{}» week {week} ({})\n{target_mxid}: !acceptswap {id} or !rejectswap {id}",
        group.name, week_dates(year, week)
    )))
}

// ── !acceptswap <id> ──────────────────────────────────────────────────────────

async fn cmd_acceptswap(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let id: u64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return Ok(Some("Usage: !acceptswap <id>".into())),
    };
    let sender_mxid = sender.as_str();
    let mut state = ctx.state.lock().await;

    let req = match state.swap_requests.iter().find(|r| r.id == id) {
        Some(r) => r,
        None => return Ok(Some(format!("Swap request #{id} not found."))),
    };
    if req.target != sender_mxid {
        return Ok(Some("This swap is not addressed to you.".into()));
    }
    if req.status != SwapStatus::Pending {
        return Ok(Some(format!("Request #{id} is already {:?}.", req.status)));
    }
    let requester = req.requester.clone();
    let group_id = req.group_id.clone();
    let iso_year = req.iso_year;
    let iso_week = req.iso_week;

    // A swap is only single-slot-group aware today, same as !swap itself.
    if state.is_completed(&group_id, iso_year, iso_week) {
        let group_name = state
            .group_by_id(&group_id)
            .map(|g| g.name.clone())
            .unwrap_or_default();
        return Ok(Some(format!(
            "«{group_name}» week {iso_week} is already completed or skipped — swap #{id} can no longer be accepted."
        )));
    }

    let requester_id = state
        .person_by_matrix_id(&requester)
        .map(|p| p.id.clone())
        .unwrap_or_else(|| requester.clone());
    let replacement_id = state
        .person_by_matrix_id(sender_mxid)
        .map(|p| p.id.clone())
        .unwrap_or_else(|| sender_mxid.to_owned());

    // The requester may no longer actually hold this week's assignment —
    // they could have left the group, or an admin/!takeover could have
    // reassigned it since the swap was requested. Accepting anyway would
    // silently hand the week to `sender` at the expense of whoever holds it
    // now, without their consent, so refuse and cancel the stale request
    // instead of blindly overwriting it.
    let interval = ctx.config.schedule.interval_weeks;
    let group = state.group_by_id(&group_id).cloned();
    let current_holder_id = group
        .as_ref()
        .and_then(|g| state.responsible_person(g, iso_year, iso_week, interval))
        .map(|p| p.id.clone());
    if current_holder_id.as_deref() != Some(requester_id.as_str()) {
        state.apply_event(DomainEvent::SwapRejected { swap_id: id })?;
        state.save(&ctx.state_path).await?;
        let group_name = group.map(|g| g.name).unwrap_or_default();
        let holder_label = current_holder_id
            .as_ref()
            .and_then(|pid| state.person_by_id(pid))
            .map(person_label)
            .unwrap_or_else(|| "nobody".into());
        return Ok(Some(format!(
            "Swap #{id} is no longer valid — {requester} is no longer responsible for «{group_name}» \
             week {iso_week} (now: {holder_label}). It has been cancelled."
        )));
    }

    state.apply_event(DomainEvent::SwapApproved {
        swap_id: id,
        group_id: group_id.clone(),
        requester_id,
        replacement_id: replacement_id.clone(),
        iso_year,
        iso_week,
    })?;
    // The swap-request bookkeeping above records *that* a swap happened; this
    // is what actually makes the target the responsible person — the same
    // frozen `SlotAssignment` !assign and !takeover use, so !done, !status,
    // the pinned plan's ✅ reaction, PDF/iCal etc. all agree immediately,
    // even though the week was already materialized before the swap.
    state.apply_event(DomainEvent::SlotAssigned {
        group_id: group_id.clone(),
        slot_index: 0,
        iso_year,
        iso_week,
        person_id: Some(replacement_id),
        source: AssignmentSource::Swap,
        actor_id: Some(sender_mxid.to_owned()),
        previous_person_id: current_holder_id,
    })?;
    state.save(&ctx.state_path).await?;

    let group_name = state
        .group_by_id(&group_id)
        .map(|g| g.name.clone())
        .unwrap_or_default();
    Ok(Some(format!(
        "✅ Swap #{id} accepted. {sender_mxid} will clean «{group_name}» instead of {requester}."
    )))
}

// ── !rejectswap <id> ─────────────────────────────────────────────────────────

async fn cmd_rejectswap(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let id: u64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return Ok(Some("Usage: !rejectswap <id>".into())),
    };
    let sender_mxid = sender.as_str();
    let mut state = ctx.state.lock().await;

    let req = match state.swap_requests.iter().find(|r| r.id == id) {
        Some(r) => r,
        None => return Ok(Some(format!("Swap #{id} not found."))),
    };
    if req.target != sender_mxid {
        return Ok(Some("This swap is not addressed to you.".into()));
    }
    if req.status != SwapStatus::Pending {
        return Ok(Some(format!("Request #{id} is already {:?}.", req.status)));
    }
    let _ = req;

    state.apply_event(DomainEvent::SwapRejected { swap_id: id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("❌ Swap #{id} rejected.")))
}

// ── Admin: !assign <group> [<slot>] <person> [week <N>] ──────────────────────
//
// Directly sets who is responsible for one group/slot in one specific week
// (default: the current week), overriding the round-robin rotation for that
// week only.  Stored as a frozen `SlotAssignment` with `AssignmentSource::Manual`
// — the same record the resolver produces, so !cleanplan, !status, !remind,
// the PDF/iCal exports and the pinned weekly plan all pick it up for free.
//
// This does not touch group membership (`!cleaning add/remove`, `!addperson`,
// `!removeperson` own that) — it only edits who is on the hook for one week,
// which is why the target person does not need to already be a rotation
// member.

async fn cmd_assign(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let usage = "Usage: !assign <group> [<slot>] <person> [week <1-53>]";
    let Some(group_name) = args.first() else {
        return Ok(Some(usage.into()));
    };

    let (rest, (year, week)) = match extract_week_arg(&args[1..]) {
        Some(v) => v,
        None => return Ok(Some(usage.into())),
    };
    let (cur_y, cur_w) = current_iso_week();
    if (year, week) < (cur_y, cur_w) {
        return Ok(Some(format!(
            "Week {week} ({}) is in the past.",
            week_dates(year, week)
        )));
    }

    let mut state = ctx.state.lock().await;
    let (group_id, slot_index, rest) = match resolve_group_and_slot(&state, group_name, rest) {
        Ok(v) => v,
        Err(e) => return Ok(Some(e)),
    };
    let Some(person_query) = rest.first() else {
        return Ok(Some(usage.into()));
    };
    let person = match state.find_person(person_query) {
        Some(p) => p.clone(),
        None => {
            return Ok(Some(format!(
            "«{person_query}» is not registered. Use !adduser or !addperson to register them first."
        )))
        }
    };

    let group = state
        .group_by_id(&group_id)
        .expect("resolved group must exist")
        .clone();
    let interval = ctx.config.schedule.interval_weeks;
    let previous_id = if group.is_multi_slot() {
        state
            .slot_assignee(&group, slot_index, year, week, interval)
            .map(|p| p.id.clone())
    } else {
        state
            .responsible_person(&group, year, week, interval)
            .map(|p| p.id.clone())
    };

    state.apply_event(DomainEvent::SlotAssigned {
        group_id: group_id.clone(),
        slot_index,
        iso_year: year,
        iso_week: week,
        person_id: Some(person.id.clone()),
        source: AssignmentSource::Assign,
        actor_id: Some(sender.as_str().to_owned()),
        previous_person_id: previous_id.clone(),
    })?;
    state.save(&ctx.state_path).await?;

    let slot_suffix = group
        .slots
        .get(slot_index)
        .map(|s| format!(" / {}", s.name))
        .unwrap_or_default();
    let membership_note = if group.member_ids.contains(&person.id) {
        String::new()
    } else {
        format!(" (not a member of «{}» — one-off assignment)", group.name)
    };
    let changed_note = match previous_id {
        Some(prev_id) if prev_id != person.id => {
            let prev_label = state
                .person_by_id(&prev_id)
                .map(person_label)
                .unwrap_or_else(|| "nobody".into());
            format!(" · was {prev_label}")
        }
        _ => String::new(),
    };

    Ok(Some(format!(
        "✅ Assigned {} to «{}»{slot_suffix} for week {week} ({}){membership_note}{changed_note}.",
        person_label(&person),
        group.name,
        week_dates(year, week)
    )))
}

// ── Admin: !unassign <group> [<slot>] [week <N>] ─────────────────────────────
//
// Clears whoever is responsible for one group/slot in one specific week
// (default: the current week), leaving it unassigned until reassigned via
// !assign or the next materialization pass. Uses the same manual-override
// mechanism as !assign.

async fn cmd_unassign(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let usage = "Usage: !unassign <group> [<slot>] [week <1-53>]";
    let Some(group_name) = args.first() else {
        return Ok(Some(usage.into()));
    };

    let (rest, (year, week)) = match extract_week_arg(&args[1..]) {
        Some(v) => v,
        None => return Ok(Some(usage.into())),
    };
    let (cur_y, cur_w) = current_iso_week();
    if (year, week) < (cur_y, cur_w) {
        return Ok(Some(format!(
            "Week {week} ({}) is in the past.",
            week_dates(year, week)
        )));
    }

    let mut state = ctx.state.lock().await;
    let (group_id, slot_index, _rest) = match resolve_group_and_slot(&state, group_name, rest) {
        Ok(v) => v,
        Err(e) => return Ok(Some(e)),
    };
    let group = state
        .group_by_id(&group_id)
        .expect("resolved group must exist")
        .clone();
    let interval = ctx.config.schedule.interval_weeks;
    let previous_id = if group.is_multi_slot() {
        state
            .slot_assignee(&group, slot_index, year, week, interval)
            .map(|p| p.id.clone())
    } else {
        state
            .responsible_person(&group, year, week, interval)
            .map(|p| p.id.clone())
    };

    state.apply_event(DomainEvent::SlotAssigned {
        group_id: group_id.clone(),
        slot_index,
        iso_year: year,
        iso_week: week,
        person_id: None,
        source: AssignmentSource::Assign,
        actor_id: Some(sender.as_str().to_owned()),
        previous_person_id: previous_id,
    })?;
    state.save(&ctx.state_path).await?;

    let slot_suffix = group
        .slots
        .get(slot_index)
        .map(|s| format!(" / {}", s.name))
        .unwrap_or_default();
    Ok(Some(format!(
        "✅ Cleared «{}»{slot_suffix} for week {week} ({}) — left unassigned.",
        group.name,
        week_dates(year, week)
    )))
}

// ── Admin: !importplan [--replace] <entry>[ ; <entry>]* ──────────────────────
//
// One-time migration helper: freeze the remaining upcoming weeks of the old
// paper cleaning plan into the bot. Each entry names one already-decided
// assignment; entries are frozen via the exact same `SlotAssigned` event and
// upsert semantics as `!assign` (`AssignmentSource::Import` only for a
// clearer audit trail), so imported weeks behave exactly like a normal
// manual assignment and — crucially — never touch `rotation_queue`. Normal
// round-robin scheduling (`resolver::materialize`) simply skips every
// already-frozen (group, slot, week) it sees, so it picks up on its own,
// from wherever the queue already was, right after the last imported week —
// including a week this command *replaced*, since replacing just upserts
// the same `SlotAssignment` record and never touches the queue either.
//
// Entry syntax:   <ISO year>-W<week> <group>[/<slot>] <person>
// Multiple entries share one command line, separated by a standalone `;`:
//   !importplan 2025-W36 Kitchen @alice:example.org ; 2025-W36 Bathroom/Sink @bob:example.org
//
// Every entry is validated — ISO week format, future/current week, known
// group/slot, known person, no two entries fighting over the same slot, and
// (in default mode) no conflict with whatever is already frozen for that
// slot — before *any* entry is written; one bad line aborts the whole
// command with no state change, and every problem found is reported at once
// rather than stopping at the first. An entry that already matches
// persisted state exactly is treated as already-imported and quietly
// skipped, which is what makes re-running the same import safe.
//
// `--replace` (a flag token anywhere in the args) additionally allows
// overwriting an existing *different* assignment for the same (group, slot,
// week) — needed because most upcoming weeks are typically already
// materialized by the normal scheduler by the time a paper-plan migration
// happens. It never allows overwriting a week that's already completed or
// skipped, in either mode — that's historical record, not an open slot.
fn parse_iso_week_token(s: &str) -> Option<(i32, u32)> {
    let (y, w) = s.split_once("-W").or_else(|| s.split_once("-w"))?;
    let year: i32 = y.parse().ok()?;
    let week: u32 = w.parse().ok()?;
    (1..=53).contains(&week).then_some((year, week))
}

struct PlannedImport {
    raw: String,
    group_id: GroupId,
    group_name: String,
    slot_index: usize,
    slot_suffix: String,
    year: i32,
    week: u32,
    person: Person,
}

async fn cmd_importplan(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let usage = "Usage: !importplan [--replace] <YYYY-Www> <group>[/<slot>] <person> [; <YYYY-Www> <group>[/<slot>] <person> ...]";
    let replace_mode = args.contains(&"--replace");
    let args: Vec<&str> = args.iter().copied().filter(|&a| a != "--replace").collect();
    if args.is_empty() {
        return Ok(Some(usage.into()));
    }

    let entries: Vec<&[&str]> = args
        .split(|t| *t == ";")
        .filter(|c| !c.is_empty())
        .collect();
    if entries.is_empty() {
        return Ok(Some(usage.into()));
    }

    let (cur_y, cur_w) = current_iso_week();
    let mut state = ctx.state.lock().await;

    let mut planned: Vec<PlannedImport> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for chunk in &entries {
        let raw = chunk.join(" ");
        let Some((&week_token, rest)) = chunk.split_first() else {
            errors.push(format!("«{raw}» — {usage}"));
            continue;
        };
        let Some((year, week)) = parse_iso_week_token(week_token) else {
            errors.push(format!(
                "«{raw}»: «{week_token}» is not a valid ISO week (expected YYYY-Www)."
            ));
            continue;
        };
        if (year, week) < (cur_y, cur_w) {
            errors.push(format!(
                "«{raw}»: week {week} ({}) is in the past.",
                week_dates(year, week)
            ));
            continue;
        }
        let Some((&group_name, rest)) = rest.split_first() else {
            errors.push(format!("«{raw}» — {usage}"));
            continue;
        };
        let (group_id, slot_index, rest) = match resolve_group_and_slot(&state, group_name, rest) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("«{raw}»: {e}"));
                continue;
            }
        };
        let group = state
            .group_by_id(&group_id)
            .expect("resolved group must exist");
        let Some(&person_query) = rest.first() else {
            errors.push(format!("«{raw}» — {usage}"));
            continue;
        };
        if rest.len() > 1 {
            errors.push(format!("«{raw}»: unexpected extra text after the person."));
            continue;
        }
        let Some(person) = state.find_person(person_query) else {
            errors.push(format!(
                "«{raw}»: «{person_query}» is not registered. Use !adduser or !addperson first."
            ));
            continue;
        };
        let slot_suffix = group
            .slots
            .get(slot_index)
            .map(|s| format!("/{}", s.name))
            .unwrap_or_default();
        planned.push(PlannedImport {
            raw,
            group_id,
            group_name: group.name.clone(),
            slot_index,
            slot_suffix,
            year,
            week,
            person: person.clone(),
        });
    }

    // Two entries in this same batch claiming the same slot/week for different people.
    for i in 0..planned.len() {
        for j in (i + 1)..planned.len() {
            let (a, b) = (&planned[i], &planned[j]);
            if a.group_id == b.group_id
                && a.slot_index == b.slot_index
                && a.year == b.year
                && a.week == b.week
                && a.person.id != b.person.id
            {
                errors.push(format!(
                    "«{}» and «{}» both claim {}{} week {} — conflicting entries in this import.",
                    a.raw, b.raw, a.group_name, a.slot_suffix, a.week
                ));
            }
        }
    }

    // Conflicts against whatever is already persisted (round-robin, a prior
    // manual !assign, or an earlier import). A slot with no record at all is
    // always safe to fill; one that already matches this entry exactly is a
    // no-op either way; anything else is only writable under `--replace` —
    // and never at all if the slot/week is already completed or skipped,
    // since that's a historical record, not an open assignment to override.
    let mut to_add: Vec<&PlannedImport> = Vec::new();
    let mut to_replace: Vec<(&PlannedImport, String)> = Vec::new(); // (entry, previous holder label)
    let mut already_imported = 0usize;
    for p in &planned {
        let existing = state.slot_assignments.iter().find(|a| {
            a.group_id == p.group_id
                && a.slot_index == p.slot_index
                && a.iso_year == p.year
                && a.iso_week == p.week
        });
        if existing.is_some_and(|a| a.person_id.as_deref() == Some(p.person.id.as_str())) {
            already_imported += 1;
            continue;
        }

        let group = state
            .group_by_id(&p.group_id)
            .expect("resolved group must exist");
        let already_done = if group.is_multi_slot() {
            group
                .slots
                .get(p.slot_index)
                .is_some_and(|s| state.is_slot_completed(&p.group_id, &s.id, p.year, p.week))
        } else {
            state.is_completed(&p.group_id, p.year, p.week)
        };
        if already_done {
            errors.push(format!(
                "«{}»: {}{} for week {} ({}) is already completed/skipped — cannot import over a finished week.",
                p.raw, p.group_name, p.slot_suffix, p.week, week_dates(p.year, p.week)
            ));
            continue;
        }

        match existing {
            None => to_add.push(p),
            Some(a) if replace_mode => {
                let holder = a
                    .person_id
                    .as_ref()
                    .and_then(|id| state.person_by_id(id))
                    .map(person_label)
                    .unwrap_or_else(|| "nobody".into());
                to_replace.push((p, holder));
            }
            Some(a) => {
                let holder = a
                    .person_id
                    .as_ref()
                    .and_then(|id| state.person_by_id(id))
                    .map(person_label)
                    .unwrap_or_else(|| "nobody".into());
                errors.push(format!(
                    "«{}»: {}{} for week {} ({}) is already assigned to {holder} — use !importplan --replace to override, or !unassign it first.",
                    p.raw, p.group_name, p.slot_suffix, p.week, week_dates(p.year, p.week)
                ));
            }
        }
    }

    if !errors.is_empty() {
        return Ok(Some(format!(
            "❌ Import aborted, no changes made — {} problem(s):\n{}",
            errors.len(),
            errors.join("\n")
        )));
    }

    if to_add.is_empty() && to_replace.is_empty() {
        return Ok(Some(format!(
            "✅ Nothing to do — all {already_imported} entr{} already imported.",
            if already_imported == 1 { "y" } else { "ies" }
        )));
    }

    let make_event = |p: &PlannedImport| DomainEvent::SlotAssigned {
        group_id: p.group_id.clone(),
        slot_index: p.slot_index,
        iso_year: p.year,
        iso_week: p.week,
        person_id: Some(p.person.id.clone()),
        source: AssignmentSource::Import,
        actor_id: Some(sender.as_str().to_owned()),
        previous_person_id: None,
    };
    let events: Vec<DomainEvent> = to_add
        .iter()
        .map(|p| make_event(p))
        .chain(to_replace.iter().map(|(p, _)| make_event(p)))
        .collect();
    let mut lines: Vec<String> = to_add
        .iter()
        .map(|p| {
            format!(
                "• {}{} week {} ({}) → {} (added)",
                p.group_name,
                p.slot_suffix,
                p.week,
                week_dates(p.year, p.week),
                person_label(&p.person)
            )
        })
        .collect();
    lines.extend(to_replace.iter().map(|(p, prev)| {
        format!(
            "• {}{} week {} ({}) → {} (replaced {prev})",
            p.group_name,
            p.slot_suffix,
            p.week,
            week_dates(p.year, p.week),
            person_label(&p.person)
        )
    }));
    let (added_count, replaced_count) = (to_add.len(), to_replace.len());

    for event in events {
        state.apply_event(event)?;
    }
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "✅ Import complete — {added_count} added, {replaced_count} replaced, {already_imported} unchanged:\n{}",
        lines.join("\n")
    )))
}

// ── !takeover [<group>] [<slot>] [week <N>] ──────────────────────────────────
//
// Self-service handoff for the currently running (or a future) week: the
// sender claims responsibility away from whoever currently has it, whether
// that's the regular rotation pick or an earlier manual assignment. Uses the
// exact same manual-override mechanism as !assign (a frozen `SlotAssigned`
// with `source: Manual`), so it never touches `rotation_queue` or any other
// week — "frozen" only means the automatic rotation won't re-decide this
// week; an explicit handoff like this always may.
//
// Group, slot and week are all optional (see `resolve_takeover_target`):
// bare `!takeover` defaults to the sender's own group, current week, and
// auto-picks the slot when exactly one is takeable — never guessing between
// several groups or several open slots. The full explicit
// `<group> <slot> [week <N>]` syntax keeps working unchanged.
//
// Refuses to touch a week that's already completed or skipped (both are
// recorded as a `Completion`,
// checked via `is_completed`/`is_slot_completed`), so a finished task can't
// be silently reassigned out from under its record.

async fn cmd_takeover(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let usage = "Usage: !takeover [<group>] [<slot>] [week <1-53>]";
    let (rest, (year, week)) = match extract_week_arg(args) {
        Some(v) => v,
        None => return Ok(Some(usage.into())),
    };
    let (cur_y, cur_w) = current_iso_week();
    if (year, week) < (cur_y, cur_w) {
        return Ok(Some(format!(
            "Week {week} ({}) is in the past.",
            week_dates(year, week)
        )));
    }

    let sender_mxid = sender.as_str();
    let mut state = ctx.state.lock().await;

    // PersonCreated is idempotent — same self-registration as !done.
    let new_person_id = Uuid::new_v4().to_string();
    state.apply_event(DomainEvent::PersonCreated {
        person_id: new_person_id,
        display_name: sender_mxid.to_owned(),
        matrix_id: Some(sender_mxid.to_owned()),
    })?;
    let sender_person_id = state.person_by_matrix_id(sender_mxid).unwrap().id.clone();
    let interval = ctx.config.schedule.interval_weeks;

    let (group_id, slot_index) =
        match resolve_takeover_target(&state, &sender_person_id, rest, year, week, interval) {
            Ok(v) => v,
            Err(e) => return Ok(Some(e)),
        };
    let group = state
        .group_by_id(&group_id)
        .expect("resolved group must exist")
        .clone();

    let already_done = match group.slots.get(slot_index) {
        Some(slot) => state.is_slot_completed(&group_id, &slot.id, year, week),
        None => state.is_completed(&group_id, year, week),
    };
    if already_done {
        let slot_suffix = group
            .slots
            .get(slot_index)
            .map(|s| format!(" / {}", s.name))
            .unwrap_or_default();
        return Ok(Some(format!(
            "«{}»{slot_suffix} for week {week} ({}) is already completed or skipped — nothing to take over.",
            group.name, week_dates(year, week)
        )));
    }

    let previous_id = if group.is_multi_slot() {
        state
            .slot_assignee(&group, slot_index, year, week, interval)
            .map(|p| p.id.clone())
    } else {
        state
            .responsible_person(&group, year, week, interval)
            .map(|p| p.id.clone())
    };
    if previous_id.as_deref() == Some(sender_person_id.as_str()) {
        return Ok(Some(format!(
            "You are already responsible for «{}» this week.",
            group.name
        )));
    }

    state.apply_event(DomainEvent::SlotAssigned {
        group_id: group_id.clone(),
        slot_index,
        iso_year: year,
        iso_week: week,
        person_id: Some(sender_person_id.clone()),
        source: AssignmentSource::Takeover,
        actor_id: Some(sender_mxid.to_owned()),
        previous_person_id: previous_id.clone(),
    })?;
    state.save(&ctx.state_path).await?;

    let slot_suffix = group
        .slots
        .get(slot_index)
        .map(|s| format!(" / {}", s.name))
        .unwrap_or_default();
    let membership_note = if group.member_ids.contains(&sender_person_id) {
        String::new()
    } else {
        format!(" (not a member of «{}» — one-off takeover)", group.name)
    };
    let from_note = match previous_id {
        Some(prev_id) => {
            let prev_label = state
                .person_by_id(&prev_id)
                .map(person_label)
                .unwrap_or_else(|| "nobody".into());
            format!(" from {prev_label}")
        }
        None => String::new(),
    };
    Ok(Some(format!(
        "✅ You took over «{}»{slot_suffix} for week {week} ({}){from_note}{membership_note}.",
        group.name,
        week_dates(year, week)
    )))
}

// ── !undo [group] ─────────────────────────────────────────────────────────────

async fn cmd_undo(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let (year, week) = current_iso_week();
    let sender_mxid = sender.as_str();
    let is_admin = ctx.admin_users.contains(sender);
    let mut state = ctx.state.lock().await;

    let sender_pid = state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone());

    let target_group_ids: Vec<String> = if let Some(name) = args.first() {
        match state.group_by_name(name) {
            Some(g) => vec![g.id.clone()],
            None => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        match &sender_pid {
            Some(pid) => state
                .groups_for_person(pid)
                .iter()
                .map(|g| g.id.clone())
                .collect(),
            None => return Ok(Some("You are not assigned to any group.".into())),
        }
    };

    if target_group_ids.is_empty() {
        return Ok(Some("You are not assigned to any group.".into()));
    }

    let mut undone = vec![];
    let mut not_done = vec![];
    let mut no_perm = vec![];

    for group_id in &target_group_ids {
        let is_member = sender_pid
            .as_ref()
            .map(|pid| {
                state
                    .cleaning_groups
                    .iter()
                    .find(|g| &g.id == group_id)
                    .map(|g| g.member_ids.contains(pid))
                    .unwrap_or(false)
            })
            .unwrap_or(false);

        if !is_member && !is_admin {
            let name = state
                .group_by_id(group_id)
                .map(|g| g.name.clone())
                .unwrap_or_default();
            no_perm.push(name);
            continue;
        }

        let name = state
            .group_by_id(group_id)
            .map(|g| g.name.clone())
            .unwrap_or_default();
        let had_completion = state.is_completed(group_id, year, week);
        state.apply_event(DomainEvent::CleaningUndone {
            group_id: group_id.clone(),
            iso_year: year,
            iso_week: week,
        })?;
        if had_completion {
            // Infrastructure cleanup: remove matching reaction_done trackers.
            state.reaction_dones.retain(|_, rd| {
                !(rd.group_id == *group_id && rd.iso_year == year && rd.iso_week == week)
            });
            undone.push(name);
        } else {
            not_done.push(name);
        }
    }

    state.save(&ctx.state_path).await?;
    let mut lines = vec![];
    if !undone.is_empty() {
        lines.push(format!("↩️ Undone: {}", undone.join(", ")));
    }
    if !not_done.is_empty() {
        lines.push(format!("Not done this week: {}", not_done.join(", ")));
    }
    if !no_perm.is_empty() {
        lines.push(format!("❌ Not your group: {}", no_perm.join(", ")));
    }
    Ok(Some(lines.join("\n")))
}

// ── !next [@user] ─────────────────────────────────────────────────────────────

async fn cmd_next(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let (cur_y, cur_w) = current_iso_week();

    let query = args.first().copied().unwrap_or_else(|| sender.as_str());
    let person = match state.find_person(query) {
        Some(p) => p.clone(),
        None => return Ok(Some(format!("{query} is not registered."))),
    };
    let groups = state.groups_for_person(&person.id);
    if groups.is_empty() {
        return Ok(Some(format!(
            "{} is not in any cleaning group.",
            person.display_name
        )));
    }

    let (dy, dw) = crate::state::first_due_week(&state, interval);
    let away = weeks_between((cur_y, cur_w), (dy, dw));
    let when = match away {
        0 => "this week ⚠️".into(),
        1 => "next week".into(),
        n => format!("in {n} weeks"),
    };
    let group_names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();
    let done = groups.iter().any(|g| state.is_cleaned(&g.id, dy, dw));
    let suffix = if done { "  ✅ already done!" } else { "" };

    Ok(Some(format!(
        "📅 Next due for {name}: **Week {dw} ({dates})** ({when}) · {groups}{suffix}",
        name = person.display_name,
        dates = week_dates(dy, dw),
        groups = group_names.join(", "),
    )))
}

// ── Admin: !skip [group] ─────────────────────────────────────────────────────

async fn cmd_skip(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (year, week) = current_iso_week();
    let interval = ctx.config.schedule.interval_weeks;
    let mut state = ctx.state.lock().await;

    let target_ids: Vec<String> = if let Some(name) = args.first() {
        match state.group_by_name(name) {
            Some(g) => vec![g.id.clone()],
            None => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        state
            .cleaning_groups
            .iter()
            .filter(|g| g.is_active && state.is_due(&g.id, year, week, interval))
            .map(|g| g.id.clone())
            .collect()
    };

    let mut skipped = vec![];
    let mut already = vec![];
    let sender_mxid = sender.as_str();
    let sender_pid = state
        .person_by_matrix_id(sender_mxid)
        .map(|p| p.id.clone())
        .unwrap_or_else(|| sender_mxid.to_owned());

    for group_id in &target_ids {
        let name = state
            .group_by_id(group_id)
            .map(|g| g.name.clone())
            .unwrap_or_default();
        if state.is_completed(group_id, year, week) {
            already.push(name);
            continue;
        }
        state.apply_event(DomainEvent::CleaningSkipped {
            group_id: group_id.clone(),
            skipper_id: sender_pid.clone(),
            iso_year: year,
            iso_week: week,
        })?;
        skipped.push(name);
    }

    state.save(&ctx.state_path).await?;
    let mut lines = vec![];
    if !skipped.is_empty() {
        lines.push(format!("⏭️ Skipped: {}", skipped.join(", ")));
    }
    if !already.is_empty() {
        lines.push(format!("Already done: {}", already.join(", ")));
    }
    Ok(Some(lines.join("\n")))
}

// ── Admin: !remind [group] ────────────────────────────────────────────────────

async fn cmd_remind(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    require_admin(ctx, sender)?;

    let (year, week) = current_iso_week();
    let interval = ctx.config.schedule.interval_weeks;

    let (reminder_data, reply_to_plan): (
        Vec<(String, String, Option<String>, Vec<String>)>,
        Option<String>,
    ) = {
        let state = ctx.state.lock().await;
        let groups = if let Some(name) = args.first() {
            state
                .cleaning_groups
                .iter()
                .filter(|g| g.is_active && g.name.eq_ignore_ascii_case(name))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            state
                .cleaning_groups
                .iter()
                .filter(|g| {
                    g.is_active
                        && state.is_due(&g.id, year, week, interval)
                        && !state.is_completed(&g.id, year, week)
                })
                .cloned()
                .collect()
        };

        let week_key = format!("{year}-W{week:02}");
        let reply_to_plan = state.weekly_plan_canonical.get(&week_key).cloned();
        let reminder_data = groups
            .iter()
            .map(|g| {
                let resp = state.responsible_person(g, year, week, interval);
                let mxids = resp
                    .and_then(|p| p.matrix_id.as_ref().map(|m| vec![m.clone()]))
                    .unwrap_or_default();
                let _text = resp
                    .map(|p| person_key(p).to_owned())
                    .unwrap_or_else(|| "(nobody assigned)".into());
                (g.id.clone(), g.name.clone(), g.rooms_text(), mxids)
            })
            .collect();
        (reminder_data, reply_to_plan)
    };

    if reminder_data.is_empty() {
        return Ok(Some(format::mentionify(
            "✅ Nothing due and uncleaned right now.",
        )));
    }

    let mut sent = vec![];
    for (_group_id, group_name, rooms_text, mxids) in &reminder_data {
        let users_text = if mxids.is_empty() {
            "(nobody assigned)".into()
        } else {
            mxids.join(", ")
        };
        let rooms_line = rooms_text
            .as_ref()
            .map(|r| format!(" · {}", r.replace('\n', " · ")))
            .unwrap_or_default();
        let msg =
            format!("⏰ **Reminder · Week {week}**\n**{group_name}** · {users_text}{rooms_line}");

        let uid_refs: Vec<&str> = mxids.iter().map(String::as_str).collect();
        let names = format::fetch_names(room, &uid_refs).await;
        let parsed: Vec<matrix_sdk::ruma::OwnedUserId> =
            mxids.iter().filter_map(|s| s.parse().ok()).collect();
        let mut content = format::mentionify_with_names(&msg, &names)
            .add_mentions(matrix_sdk::ruma::events::Mentions::with_user_ids(parsed));
        if let Some(plan_eid) = &reply_to_plan {
            if let Ok(plan_eid) = plan_eid.parse::<OwnedEventId>() {
                content.relates_to = Some(Relation::Reply(Reply::with_event_id(plan_eid)));
            }
        }

        match room.send(content).await {
            Ok(_) => {
                sent.push(group_name.clone());
            }
            Err(e) => tracing::warn!("!remind send failed for {group_name}: {e}"),
        }
    }

    Ok(Some(format::mentionify(&format!(
        "✅ Reminder sent for: {}",
        sent.join(", ")
    ))))
}

// ── !leaderboard ─────────────────────────────────────────────────────────────

async fn cmd_leaderboard(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;

    let board = analytics::global_leaderboard(&state, interval);
    if board.is_empty() {
        return Ok(Some("No members assigned to any group yet.".into()));
    }

    let mut lines = vec!["🏆 Cleaning Leaderboard".to_owned(), String::new()];
    for (i, ps) in board.iter().enumerate() {
        let medal = match i {
            0 => "🥇",
            1 => "🥈",
            2 => "🥉",
            _ => "  ",
        };
        let streak = if ps.streak >= 2 {
            format!("  🔥{}", ps.streak)
        } else {
            String::new()
        };
        let skips = if ps.skipped > 0 {
            format!("  ⏭️{}", ps.skipped)
        } else {
            String::new()
        };
        let pct = (ps.completion_rate * 100.0).round() as u32;
        lines.push(format!(
            "{medal} {}  {}/{} ({}%){streak}{skips}",
            ps.display_name, ps.completed, ps.due_weeks, pct
        ));
    }
    Ok(Some(lines.join("\n")))
}

// ── !fairness [group] ─────────────────────────────────────────────────────────

async fn cmd_fairness(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let (_, start_w) = state.tracking_start();

    let groups: Vec<crate::domain::GroupId> = if let Some(name) = args.first() {
        match state.group_by_name(name) {
            Some(g) => vec![g.id.clone()],
            None => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        state
            .cleaning_groups
            .iter()
            .filter(|g| g.is_active)
            .map(|g| g.id.clone())
            .collect()
    };

    if groups.is_empty() {
        return Ok(Some("No cleaning groups configured yet.".into()));
    }

    let mut out = Vec::new();
    for (i, group_id) in groups.iter().enumerate() {
        let Some(report) = analytics::fairness_report(&state, group_id, interval) else {
            continue;
        };
        if i > 0 {
            out.push(String::new());
        }
        out.push(format!(
            "⚖️ **{}** · {} wks · since W{start_w}",
            report.group_name, report.due_weeks,
        ));
        for e in &report.entries {
            let (pct, _) = load_delta_pct(e.actual_load, e.expected_load);
            let icon = load_icon(e.actual_load, e.expected_load);
            out.push(format!(
                "{icon} **{}**  {}/{:.0} ({})",
                e.display_name, e.actual, e.expected as u32, pct,
            ));
        }
        out.push(format!("Score {}/100", report.fairness_score));
    }

    if out.is_empty() {
        return Ok(Some(
            "No history yet — run some cleaning cycles first.".into(),
        ));
    }
    Ok(Some(out.join("\n")))
}

// ── !workload ─────────────────────────────────────────────────────────────────

async fn cmd_workload(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let report = analytics::workload_report(&state, interval);

    if report.entries.is_empty() {
        return Ok(Some("No members assigned to any group yet.".into()));
    }

    let yrs = report.years_tracked;
    let has_history = report.due_weeks >= 4;
    let header = if has_history {
        format!("🏋️ **Load** · {} wks · {:.2} yr", report.due_weeks, yrs)
    } else {
        format!(
            "🏋️ **Expected load** (structural · {} wks tracked)",
            report.due_weeks
        )
    };
    let mut out = vec![header];

    // Normalize against house average so 1.0× = average resident.
    let avg_expected = {
        let sum: f64 = report.entries.iter().map(|e| e.expected_cli_per_year).sum();
        let n = report.entries.len() as f64;
        if n > 0.0 && sum > 0.0 {
            sum / n
        } else {
            1.0
        }
    };

    for e in &report.entries {
        let ratio = e.expected_cli_per_year / avg_expected;
        let ratio_str = format!("{:.2}×", ratio);

        let line = if has_history {
            let (pct, _) = load_delta_pct(e.actual_cli_per_year, e.expected_cli_per_year);
            let icon = load_icon(e.actual_cli_per_year, e.expected_cli_per_year);
            format!(
                "{icon} **{}**  {} · {} · {}",
                e.display_name,
                pct,
                ratio_str,
                e.group_names.join(", "),
            )
        } else {
            format!(
                "**{}**  {} · {}",
                e.display_name,
                ratio_str,
                e.group_names.join(", "),
            )
        };
        out.push(line);
    }

    if has_history {
        let most = report
            .most_loaded
            .first()
            .map(String::as_str)
            .unwrap_or("-");
        let least = report
            .least_loaded
            .first()
            .map(String::as_str)
            .unwrap_or("-");
        if most != least {
            out.push(format!("⬆ {most} · ⬇ {least}"));
        }
    }

    Ok(Some(out.join("\n")))
}

// ── !groupstats ───────────────────────────────────────────────────────────────

async fn cmd_groupstats(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;

    let active: Vec<_> = state
        .cleaning_groups
        .iter()
        .filter(|g| g.is_active)
        .collect();
    if active.is_empty() {
        return Ok(Some("No active cleaning groups configured.".into()));
    }

    let models: Vec<_> = active
        .iter()
        .map(|g| analytics::group_load_model(g, interval))
        .collect();

    let avg_cli = {
        let sum: f64 = models.iter().map(|m| m.cli_per_year).sum();
        let n = models.len() as f64;
        if n > 0.0 && sum > 0.0 {
            sum / n
        } else {
            1.0
        }
    };

    let mut out = vec!["📊 **Groups**  (1.0× = avg load/person/yr)".to_owned()];

    for (group, m) in active.iter().zip(models.iter()) {
        let ratio = m.cli_per_year / avg_cli;
        let weight = if (m.group_weight - 1.0).abs() > 0.01 {
            format!(" · ×{:.1}", m.group_weight)
        } else {
            String::new()
        };
        out.push(format!(
            "**{}**  {:.2}× · {}p/{}wks · {:.0}r{}",
            group.name, ratio, m.member_count, m.rotation_interval, m.rooms_per_assignment, weight,
        ));
        for slot in &group.slots {
            let sr = analytics::effective_rooms_pub(&slot.room_names, &slot.room_weights);
            let sw = if (slot.weight - 1.0).abs() > 0.01 {
                format!(" ×{:.1}", slot.weight)
            } else {
                String::new()
            };
            out.push(format!("  └ {}  {:.0}r{}", slot.name, sr, sw));
        }
        let mut room_weights: Vec<String> = if group.slots.is_empty() {
            group
                .room_weights
                .iter()
                .map(|(r, w)| format!("{r} ×{w:.1}"))
                .collect()
        } else {
            vec![]
        };
        room_weights.sort();
        if !room_weights.is_empty() {
            out.push(format!("  weights: {}", room_weights.join(", ")));
        }
    }

    Ok(Some(out.join("\n")))
}

// ── Admin: !setroomweight <group> <room> <weight> ────────────────────────────

async fn cmd_setroomweight(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (group_name, room_name, weight_str) = match (args.first(), args.get(1), args.get(2)) {
        (Some(g), Some(r), Some(w)) => (*g, *r, *w),
        _ => {
            return Ok(Some(
                "Usage: !setroomweight <group> <room> <weight>  (e.g. 2.0 for twice the load)"
                    .into(),
            ))
        }
    };
    let weight: f64 = match weight_str.parse() {
        Ok(w) if w > 0.0 => w,
        _ => {
            return Ok(Some(
                "Weight must be a positive number (e.g. 1.5 or 0.5).".into(),
            ))
        }
    };
    let mut state = ctx.state.lock().await;
    let (group_id, slot_id) = match state.group_by_name(group_name) {
        Some(g) => {
            // Check if the room exists in a slot or at group level.
            let in_group = g
                .room_names
                .iter()
                .any(|r| r.eq_ignore_ascii_case(room_name));
            let slot = g.slots.iter().find(|s| {
                s.room_names
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(room_name))
            });
            match (in_group, slot) {
                (true, _) => (g.id.clone(), None),
                (_, Some(s)) => (g.id.clone(), Some(s.id.clone())),
                _ => {
                    return Ok(Some(format!(
                        "Room «{room_name}» not found in «{group_name}»."
                    )))
                }
            }
        }
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    // Canonicalize room name from actual stored name.
    let canonical = {
        let g = state.group_by_name(group_name).unwrap();
        match &slot_id {
            None => g
                .room_names
                .iter()
                .find(|r| r.eq_ignore_ascii_case(room_name))
                .unwrap()
                .clone(),
            Some(s) => g
                .slot_by_id(s)
                .unwrap()
                .room_names
                .iter()
                .find(|r| r.eq_ignore_ascii_case(room_name))
                .unwrap()
                .clone(),
        }
    };
    state.apply_event(DomainEvent::RoomWeightSet {
        group_id,
        slot_id,
        room_name: canonical.clone(),
        weight,
    })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "✅ Room «{canonical}» in «{group_name}» weight set to {weight:.2}×."
    )))
}

// ── Admin: !setgroupweight <group> <weight> ───────────────────────────────────

async fn cmd_setgroupweight(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (group_name, weight_str) = match (args.first(), args.get(1)) {
        (Some(g), Some(w)) => (*g, *w),
        _ => {
            return Ok(Some(
                "Usage: !setgroupweight <group> <weight>  (e.g. 2.0 for twice the load)".into(),
            ))
        }
    };
    let weight: f64 = match weight_str.parse() {
        Ok(w) if w > 0.0 => w,
        _ => {
            return Ok(Some(
                "Weight must be a positive number (e.g. 1.5 or 0.5).".into(),
            ))
        }
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(group_name) {
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    state.apply_event(DomainEvent::GroupWeightSet { group_id, weight })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "✅ «{group_name}» workload weight set to {weight:.2}×."
    )))
}

// ── Admin: !absent <person> [group] [weeks] ───────────────────────────────────
//
// Records an `Absence`, which `resolver::materialize` reads: for any
// not-yet-frozen due week that falls in the absence range, the person is
// skipped when picking who's next — passed over in place, not removed from
// or requeued in `rotation_queue`, so they keep their turn and are simply
// due again once the absence ends (see `resolver::materialize`'s tests).
//
// Deliberately does NOT touch weeks that are already frozen (existing
// `SlotAssignment`s) — same "automatic rotation never rewrites an
// already-frozen week" rule as everywhere else. If the person is already
// assigned for the current week when this is called, that assignment
// stands; get someone else onto it with !takeover, !swap, or !assign.

async fn cmd_absent(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let person_query = match args.first() {
        Some(u) => u.to_string(),
        None => return Ok(Some("Usage: !absent <person> [weeks]  (default 4)".into())),
    };

    let mut state = ctx.state.lock().await;
    let person = match state.find_person(&person_query).cloned() {
        Some(p) => p,
        None => return Ok(Some(format!("{person_query} not found."))),
    };

    // Parse optional weeks (last numeric arg).
    let weeks: u32 = args.iter().rev().find_map(|s| s.parse().ok()).unwrap_or(4);

    let groups = state
        .groups_for_person(&person.id)
        .iter()
        .map(|g| g.id.clone())
        .collect::<Vec<_>>();
    if groups.is_empty() {
        return Ok(Some(format!(
            "{} is not in any group.",
            person.display_name
        )));
    }

    let (from_y, from_w) = current_iso_week();
    for group_id in &groups {
        state.apply_event(DomainEvent::AbsenceRecorded {
            person_id: person.id.clone(),
            group_id: group_id.clone(),
            from_year: from_y,
            from_week: from_w,
            duration_weeks: weeks,
        })?;
    }
    state.save(&ctx.state_path).await?;

    let end = add_weeks(from_y, from_w, weeks as i64);
    Ok(Some(format!(
        "🌴 {} away for {weeks} week{} · back week {} ({})",
        person.display_name,
        if weeks == 1 { "" } else { "s" },
        end.1,
        week_dates(end.0, end.1)
    )))
}

// ── Admin: !back <person> ─────────────────────────────────────────────────────

async fn cmd_back(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let query = match args.first() {
        Some(u) => u.to_string(),
        None => return Ok(Some("Usage: !back <person>".into())),
    };
    let mut state = ctx.state.lock().await;
    let person_id = match state.find_person(&query).map(|p| p.id.clone()) {
        Some(id) => id,
        None => return Ok(Some(format!("{query} not found."))),
    };
    if !state.absences.iter().any(|a| a.person_id == person_id) {
        return Ok(Some(format!("{query} has no active absence.")));
    }
    state.apply_event(DomainEvent::AbsenceCancelled { person_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ {query} is back.")))
}

// ── !blame [group / @user] ────────────────────────────────────────────────────

async fn cmd_blame(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let (cur_y, cur_w) = current_iso_week();
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;

    Ok(Some(match args.first() {
        None => blame_all(&state, cur_y, cur_w, interval),
        Some(arg) => {
            if let Some(group) = state.group_by_name(arg) {
                blame_group(&state, &group.clone(), cur_y, cur_w, interval)
            } else if let Some(person) = state.find_person(arg).cloned() {
                blame_person(&state, &person, interval)
            } else {
                format!("«{arg}» not found.")
            }
        }
    }))
}

fn blame_all(state: &crate::state::State, year: i32, week: u32, interval: u32) -> String {
    let uncleaned: Vec<_> = state
        .cleaning_groups
        .iter()
        .filter(|g| {
            g.is_active
                && state.is_due(&g.id, year, week, interval)
                && !state.is_completed(&g.id, year, week)
        })
        .collect();
    if uncleaned.is_empty() {
        return "✅ All due groups are cleaned this week!".into();
    }
    let mut lines = vec![format!(
        "😤 **Blame** · week {week} ({})",
        week_dates(year, week)
    )];
    for g in uncleaned {
        let members_text = state
            .members_of(g)
            .iter()
            .map(|p| p.display_name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let n_due = state.all_due_weeks(interval, (year, week)).len();
        let n_missed = state.missed_weeks_for(&g.id, interval).len();
        lines.push(String::new());
        lines.push(format!("❌ {}", g.name));
        lines.push(format!("Members: {members_text}"));
        lines.push(format!("Missed: {n_missed} of {n_due}"));
    }
    lines.join("\n")
}

fn blame_group(
    state: &crate::state::State,
    group: &CleaningGroup,
    year: i32,
    week: u32,
    interval: u32,
) -> String {
    let due = state.all_due_weeks(interval, (year, week));
    let closed: Vec<_> = due
        .iter()
        .filter(|&&(y, w)| (y, w) != (year, week))
        .collect();
    let n_due = closed.len();
    let n_done = closed
        .iter()
        .filter(|(y, w)| state.is_completed(&group.id, *y, *w))
        .count();
    let pct = if n_due > 0 { 100 * n_done / n_due } else { 100 };
    let streak = state.streak_for(&group.id, interval);
    let this = state.is_completed(&group.id, year, week);
    let members_text = state
        .members_of(group)
        .iter()
        .map(|p| p.display_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let mut lines = vec![format!("😤 **Blame** · {}", group.name), String::new()];
    lines.push(format!("Members: {members_text}"));
    lines.push(format!(
        "Completed: {n_done}/{n_due} ({pct}%) · Streak: {streak} · This week: {}",
        if this { "✅" } else { "❌" }
    ));
    if let Some(last) = state.last_completion(&group.id) {
        let by = state
            .person_by_id(&last.completed_by_id)
            .map(|p| p.display_name.as_str())
            .unwrap_or("?");
        lines.push(format!(
            "Last: week {} ({}) by {by}",
            last.iso_week,
            week_dates(last.iso_year, last.iso_week)
        ));
    }
    let missed = state.missed_weeks_for(&group.id, interval);
    if !missed.is_empty() {
        let shown: Vec<_> = missed
            .iter()
            .take(5)
            .map(|(y, w)| format!("w{w} ({})", week_dates(*y, *w)))
            .collect();
        lines.push(format!(
            "Missed: {}{}",
            shown.join(", "),
            if missed.len() > 5 {
                format!(" (+{})", missed.len() - 5)
            } else {
                String::new()
            }
        ));
    }
    lines.join("\n")
}

fn blame_person(state: &crate::state::State, person: &Person, interval: u32) -> String {
    let (cur_y, cur_w) = current_iso_week();
    let groups = state.groups_for_person(&person.id);
    let group_name = groups
        .first()
        .map(|g| g.name.as_str())
        .unwrap_or("(unassigned)");

    let due = state.all_due_weeks(interval, (cur_y, cur_w));
    let closed: Vec<_> = due
        .iter()
        .filter(|&&(y, w)| (y, w) != (cur_y, cur_w))
        .collect();
    let n_due = closed.len();
    let n_done_by_person = state
        .completions
        .iter()
        .filter(|c| c.completed_by_id == person.id)
        .count()
        .min(n_due);
    let pct = if n_due > 0 {
        100 * n_done_by_person / n_due
    } else {
        100
    };
    let streak = groups
        .first()
        .map(|g| state.streak_for(&g.id, interval))
        .unwrap_or(0);

    let mut lines = vec![
        format!("😤 **Blame** · {}", person.display_name),
        String::new(),
    ];
    lines.push(format!("Group: {group_name}"));
    lines.push(format!(
        "Personally cleaned: {n_done_by_person}/{n_due} ({pct}%) · Streak: {streak}"
    ));
    if let Some(last) = state
        .completions
        .iter()
        .filter(|c| c.completed_by_id == person.id)
        .max_by_key(|c| c.completed_at)
    {
        lines.push(format!(
            "Last: week {} ({})",
            last.iso_week,
            week_dates(last.iso_year, last.iso_week)
        ));
    }
    lines.join("\n")
}

// ── !cleanplan [N] ────────────────────────────────────────────────────────────

async fn cmd_cleanplan(
    ctx: &BotContext,
    _sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    let n: usize = args
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6)
        .clamp(1, 20);

    let (snapshot, is_empty) = {
        let state = ctx.state.lock().await;
        let interval = ctx.config.schedule.interval_weeks;
        let empty = state.cleaning_groups.is_empty();
        (build_schedule(&state, interval, n), empty)
    };

    if is_empty {
        return Ok(Some(format::mentionify(
            "No cleaning groups configured yet.",
        )));
    }

    // Pre-fetch Matrix display names for all assignees and completers.
    let all_mxids: Vec<String> = {
        let mut ids = Vec::new();
        for a in &snapshot.assignments {
            if let Some(mxid) = a.assignee_mxid() {
                if !ids.contains(&mxid.to_owned()) {
                    ids.push(mxid.to_owned());
                }
            }
            if let Some(by) = &a.completed_by {
                // completed_by is a display_name, look it up
                if !ids.contains(by) {
                    let state = ctx.state.lock().await;
                    if let Some(p) = state.find_person(by) {
                        if let Some(m) = &p.matrix_id {
                            if !ids.contains(m) {
                                ids.push(m.clone());
                            }
                        }
                    }
                }
            }
        }
        ids
    };
    let uid_refs: Vec<&str> = all_mxids.iter().map(String::as_str).collect();
    let names = format::fetch_names(room, &uid_refs).await;

    let interval = snapshot.interval_weeks;
    let (cur_y, cur_w) = current_iso_week();

    let mut lines = vec![format!(
        "📅 **Cleaning plan** · next {n} week{} · every {interval} week{}",
        if n == 1 { "" } else { "s" },
        if interval == 1 { "" } else { "s" }
    )];

    for (dy, dw) in snapshot.weeks() {
        let is_cur = (dy, dw) == (cur_y, cur_w);
        lines.push(String::new());
        if is_cur {
            lines.push(format!(
                "📆 **Week {dw} ({})** ← this week",
                week_dates(dy, dw)
            ));
        } else {
            lines.push(format!("📆 Week {dw} ({})", week_dates(dy, dw)));
        }
        for a in snapshot.for_group_in_week(dy, dw) {
            let icon = if a.is_completed {
                "✅"
            } else if is_cur {
                "🔲"
            } else {
                "🗓"
            };
            let detail = if a.is_completed {
                if a.is_skipped {
                    "skipped ⏭️".into()
                } else {
                    let by = a.completed_by.as_deref().unwrap_or("?");
                    format!("done by {by}")
                }
            } else {
                match &a.assignee {
                    None => "nobody assigned yet".into(),
                    Some(p) => {
                        let key = p.mxid.as_deref().unwrap_or(&p.name);
                        let state = ctx.state.lock().await;
                        let away = state.is_absent(&p.id, &a.group_id, dy, dw);
                        drop(state);
                        if away {
                            format!("{key} (away)")
                        } else {
                            key.to_owned()
                        }
                    }
                }
            };
            lines.push(format!("  {icon} {} : {detail}", a.group_name));
        }
    }

    Ok(Some(format::mentionify_with_names(
        &lines.join("\n"),
        &names,
    )))
}

// ── Admin: !pdf [N] ───────────────────────────────────────────────────────────

async fn cmd_pdf(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
    event_id: OwnedEventId,
    thread_root: OwnedEventId,
) -> Result<Option<RoomMessageEventContent>> {
    require_admin(ctx, sender)?;
    // !pdf [weeks] [group name]
    // First arg: either a number (weeks) or start of group name.
    let (n, group_filter) = {
        let weeks = args.first().and_then(|s| s.parse::<usize>().ok());
        if let Some(w) = weeks {
            let name = if args.len() > 1 {
                Some(args[1..].join(" "))
            } else {
                None
            };
            (w.clamp(1, 52), name)
        } else {
            let name = if !args.is_empty() {
                Some(args.join(" "))
            } else {
                None
            };
            (8, name)
        }
    };

    // Refresh Matrix display names so the PDF shows "Thomas" not "thomas99".
    refresh_display_names(ctx, room).await;

    let (tex, file_name) = {
        let state = ctx.state.lock().await;
        let interval = ctx.config.schedule.interval_weeks;
        let mut snapshot = build_schedule(&state, interval, n);
        if let Some(ref name) = group_filter {
            match state.group_by_name(name) {
                Some(g) => {
                    let gid = g.id.clone();
                    snapshot.assignments.retain(|a| a.group_id == gid);
                }
                None => {
                    return Ok(Some(RoomMessageEventContent::text_plain(format!(
                        "Group «{name}» not found."
                    ))))
                }
            }
        }
        let tex = crate::pdf::render_tex(&snapshot);
        // Build filename: cleaning-plan-KW{first}-KW{last}.pdf
        let weeks_range = {
            let first = snapshot.assignments.first();
            let last = snapshot.assignments.last();
            match (first, last) {
                (Some(f), Some(l)) if (f.iso_year, f.iso_week) != (l.iso_year, l.iso_week) => {
                    format!("KW{}-KW{}", f.iso_week, l.iso_week)
                }
                (Some(f), _) => format!("KW{}", f.iso_week),
                _ => format!("{n}w"),
            }
        };
        let file_name = match &group_filter {
            Some(name) => format!(
                "cleaning-plan-{}_{}.pdf",
                name.to_lowercase().replace(' ', "_"),
                weeks_range
            ),
            None => format!("cleaning-plan-{weeks_range}.pdf"),
        };
        (tex, file_name)
    };

    // Render .tex → PDF via tectonic.
    let pdf_bytes = match crate::pdf_renderer::tex_to_pdf(&tex).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("tectonic render failed: {e}");
            return Ok(Some(format::mentionify(&format!(
                "❌ PDF render failed: {e}"
            ))));
        }
    };

    let mime: mime::Mime = "application/pdf".parse().expect("valid mime");
    let pdf_size = pdf_bytes.len();
    match room.client().media().upload(&mime, pdf_bytes, None).await {
        Ok(upload) => {
            let mut file_info = FileInfo::new();
            file_info.mimetype = Some("application/pdf".to_owned());
            file_info.size = UInt::new(pdf_size as u64);
            let mut fc = FileMessageEventContent::plain(file_name, upload.content_uri);
            fc.info = Some(Box::new(file_info));
            let mut file_content = RoomMessageEventContent::new(MessageType::File(fc));
            file_content.relates_to =
                Some(matrix_sdk::ruma::events::room::message::Relation::Thread(
                    Thread::reply(thread_root.clone(), event_id.clone()),
                ));
            room.send(file_content).await.ok();
            Ok(Some(format::mentionify("📄 Schedule generated.")))
        }
        Err(e) => {
            tracing::warn!("PDF upload failed: {e}");
            Ok(Some(format::mentionify("❌ Upload failed.")))
        }
    }
}

// ── !ical [N] / !ical <person> [N] ────────────────────────────────────────────

async fn cmd_ical(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    let sender_mxid = sender.as_str();
    let is_admin = ctx.admin_users.contains(sender);

    // Parse target person and week count.
    let (person_id, weeks): (String, usize) = {
        let state = ctx.state.lock().await;
        match args.first() {
            None => {
                let pid =
                    match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
                        Some(id) => id,
                        None => return Ok(Some(format::mentionify(&format!(
                            "You ({sender_mxid}) are not registered. Ask an admin to use !adduser."
                        )))),
                    };
                (pid, 26)
            }
            Some(first) => {
                if let Ok(n) = first.parse::<usize>() {
                    let pid = match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
                        Some(id) => id,
                        None => {
                            return Ok(Some(format::mentionify(&format!(
                                "You ({sender_mxid}) are not registered."
                            ))))
                        }
                    };
                    (pid, n.clamp(1, 104))
                } else {
                    if !is_admin {
                        return Ok(Some(format::mentionify(
                            "❌ Admin permission required to generate iCal for others.",
                        )));
                    }
                    let person = match state.find_person(first).cloned() {
                        Some(p) => p,
                        None => {
                            return Ok(Some(format::mentionify(&format!(
                                "Person «{first}» not found."
                            ))))
                        }
                    };
                    let n = args
                        .get(1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(26usize)
                        .clamp(1, 104);
                    (person.id.clone(), n)
                }
            }
        }
    };

    // If HTTP server is configured, return URL (token-based feed).
    if let Some(ical_cfg) = &ctx.config.ical_server {
        let mut state = ctx.state.lock().await;

        // Check if a non-revoked token already exists for this person.
        let has_token = state
            .calendar_tokens
            .iter()
            .any(|ct| !ct.revoked && ct.person_id == person_id);

        if has_token {
            return Ok(Some(format::mentionify(
                "📅 You already have an active calendar feed.\n\
                 Use !icalreset to get a new URL (this invalidates the old subscription).",
            )));
        }

        let (raw_token, hash) = new_calendar_token();
        state.calendar_tokens.push(CalendarToken {
            id: Uuid::new_v4().to_string(),
            token_hash: hash,
            person_id: person_id.clone(),
            created_at: Utc::now(),
            revoked: false,
        });
        state.save(&ctx.state_path).await?;
        drop(state);

        let url = format!("{}/ical/{raw_token}.ics", ical_cfg.public_url);
        return Ok(Some(format::mentionify(&format!(
            "📅 Your calendar feed URL:\n{url}\n\n\
             Add this URL to your calendar app for automatic updates.\n\
             ⚠️ This URL is shown only once — save it!"
        ))));
    }

    // Fallback: generate and upload as a Matrix file attachment.
    let ical_data = {
        let state = ctx.state.lock().await;
        let interval = ctx.config.schedule.interval_weeks;
        let snapshot = build_schedule(&state, interval, weeks);
        crate::ical::render_ics(&snapshot, &person_id)
    };

    let safe_name = person_id
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or("user")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();

    let mime: mime::Mime = "text/calendar".parse().expect("valid mime");
    match room
        .client()
        .media()
        .upload(&mime, ical_data.into_bytes(), None)
        .await
    {
        Ok(upload) => {
            use matrix_sdk::ruma::events::room::message::{FileMessageEventContent, MessageType};
            let content =
                RoomMessageEventContent::new(MessageType::File(FileMessageEventContent::plain(
                    format!("putzplan_{safe_name}.ics"),
                    upload.content_uri,
                )));
            room.send(content).await.ok();
            Ok(Some(format::mentionify(&format!(
                "📅 iCal · {weeks} Wochen · Import .ics into your calendar app.\n\
                 Tip: configure [ical_server] in config.toml for live-updating feed URLs."
            ))))
        }
        Err(e) => {
            tracing::warn!("iCal upload failed: {e}");
            Ok(Some(format::mentionify("❌ Upload failed.")))
        }
    }
}

// ── !icalreset [person] ───────────────────────────────────────────────────────

async fn cmd_icalreset(
    ctx: &BotContext,
    sender: &OwnedUserId,
    _room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    let sender_mxid = sender.as_str();
    let is_admin = ctx.admin_users.contains(sender);

    let Some(ical_cfg) = &ctx.config.ical_server else {
        return Ok(Some(format::mentionify(
            "iCal HTTP server is not configured. Add [ical_server] to config.toml.",
        )));
    };

    let person_id: String = {
        let state = ctx.state.lock().await;
        match args.first() {
            None => match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
                Some(id) => id,
                None => {
                    return Ok(Some(format::mentionify(&format!(
                        "{sender_mxid} is not registered."
                    ))))
                }
            },
            Some(query) => {
                if !is_admin {
                    return Ok(Some(format::mentionify("❌ Admin permission required.")));
                }
                match state.find_person(query).map(|p| p.id.clone()) {
                    Some(id) => id,
                    None => {
                        return Ok(Some(format::mentionify(&format!(
                            "Person «{query}» not found."
                        ))))
                    }
                }
            }
        }
    };

    let mut state = ctx.state.lock().await;
    // Revoke all existing tokens for this person.
    for ct in state
        .calendar_tokens
        .iter_mut()
        .filter(|ct| ct.person_id == person_id)
    {
        ct.revoked = true;
    }

    // Issue new token.
    let (raw_token, hash) = new_calendar_token();
    state.calendar_tokens.push(CalendarToken {
        id: Uuid::new_v4().to_string(),
        token_hash: hash,
        person_id: person_id.clone(),
        created_at: Utc::now(),
        revoked: false,
    });
    state.save(&ctx.state_path).await?;
    drop(state);

    let url = format!("{}/ical/{raw_token}.ics", ical_cfg.public_url);
    Ok(Some(format::mentionify(&format!(
        "🔄 New calendar feed URL:\n{url}\n\n\
         ⚠️ Old URLs for this person are now invalid."
    ))))
}

// ── !testnotify ───────────────────────────────────────────────────────────────

async fn cmd_testnotify(room: &Room) -> Result<Option<RoomMessageEventContent>> {
    let room = room.clone();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(5 * 60)).await;
        use matrix_sdk::ruma::events::Mentions;
        let mut msg = RoomMessageEventContent::text_html(
            "🔔 @room · test notification",
            "🔔 @room · test notification",
        );
        let mut mentions = Mentions::new();
        mentions.room = true;
        msg = msg.add_mentions(mentions);
        room.send(msg).await.ok();
    });
    Ok(Some(RoomMessageEventContent::text_plain(
        "⏱ @room notification in 5 minutes.",
    )))
}

// ── Admin: !disablegroup <group> ─────────────────────────────────────────────

async fn cmd_disablegroup(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !disablegroup <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&name) {
        Some(g) if !g.is_active => return Ok(Some(format!("«{name}» is already disabled."))),
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{name}» not found."))),
    };
    state.apply_event(DomainEvent::GroupDisabled { group_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "🚫 «{name}» disabled — excluded from scheduling and statistics."
    )))
}

// ── Admin: !enablegroup <group> ──────────────────────────────────────────────

async fn cmd_enablegroup(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !enablegroup <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&name) {
        Some(g) if g.is_active => return Ok(Some(format!("«{name}» is already enabled."))),
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{name}» not found."))),
    };
    state.apply_event(DomainEvent::GroupEnabled { group_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "✅ «{name}» enabled — now included in scheduling and statistics."
    )))
}

// ── !listgroups ───────────────────────────────────────────────────────────────

async fn cmd_listgroups(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    if state.cleaning_groups.is_empty() {
        return Ok(Some("No cleaning groups configured.".into()));
    }
    let active_count = state.cleaning_groups.iter().filter(|g| g.is_active).count();
    let disabled_count = state.cleaning_groups.len() - active_count;
    let mut lines = vec![format!(
        "🏢 Groups ({active_count} active, {disabled_count} disabled):"
    )];
    for group in &state.cleaning_groups {
        let n = group.member_ids.len();
        if group.is_active {
            lines.push(format!("  ✅ **{}** ({n} members)", group.name));
        } else {
            lines.push(format!("  🚫 **{}** ({n} members) — disabled", group.name));
        }
    }
    Ok(Some(lines.join("\n")))
}

// ── Admin: !announceweek / !repostplan ───────────────────────────────────────
//
// Sends a fresh consolidated weekly plan for the current week.  Replaces the
// previously active plan in state so reactions on old messages no longer
// trigger completions.  Pins the new message and unpins old plan messages.

async fn cmd_announceweek(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
) -> Result<Option<RoomMessageEventContent>> {
    require_admin(ctx, sender)?;
    let (year, week) = current_iso_week();
    match crate::scheduler::announce_weekly_plan(ctx, room, year, week).await {
        Ok(Some(_)) => Ok(Some(format::mentionify(
            "📋 Weekly plan announced and pinned.",
        ))),
        Ok(None) => Ok(Some(format::mentionify(
            "Nothing is due this week — no plan to announce.",
        ))),
        Err(e) => {
            tracing::error!("!announceweek failed: {e}");
            Ok(Some(format::mentionify(&format!(
                "❌ Failed to announce weekly plan: {e}"
            ))))
        }
    }
}

// ── Admin: !validate ──────────────────────────────────────────────────────────

async fn cmd_validate(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let state = ctx.state.lock().await;
    let report = crate::validate::validate_state(&state);
    Ok(Some(report.summary()))
}

// ── help ─────────────────────────────────────────────────────────────────────

fn help_text() -> String {
    r#"🧹 Cleaning bot commands:

  !help                        · show this help
  !status                      · this week's cleaned / not-cleaned overview
  !areas                       · list all cleaning groups and their members
  !undo [group]                · undo this week's done mark
  !next [@user]                · when is your (or @user's) next due week?
  !stats [@user]               · completion statistics
  !leaderboard                 · overall cleaning leaderboard with streaks
  !fairness [group]            · fairness report — who's doing their share?
  !cleanplan [N]               · show the next N due cleaning weeks (default 6)
  !blame                       · all due but uncleaned groups this week
  !blame @user                 · cleaning record for one person
  !blame <group>               · cleaning record for a specific group
  !joingroup <group>            · add yourself to a cleaning group
  !leavegroup <group>           · remove yourself from a cleaning group
  !swap @user [group] [week N] · propose a swap; !acceptswap / !rejectswap to respond
  !acceptswap <id>             · accept a pending swap request
  !rejectswap <id>             · reject a pending swap request
  !takeover [group] [slot] [week N] · claim an already-assigned week for yourself right now (defaults to your own group)
  !ical [N]                    · get your cleaning schedule as iCal (default 26 weeks)
  !icalreset                   · revoke and regenerate your iCal feed URL

Admin commands:
  !skip [group]                         · excuse this week (won't count as missed)
  !announceweek                         · post (or repost) this week's cleaning plan and pin it
  !remind [group]                       · manually fire the cleaning reminder now
  !pdf [N]                              · generate printable HTML schedule (default 8 weeks)
  !absent <person> [weeks]              · skip person in new rotation picks (default 4 weeks; already-frozen weeks are unaffected)
  !back <person>                        · cancel an absence early
  !addgroup <name>                      · create a new cleaning group
  !removegroup <name>                   · delete a cleaning group
  !cleaning add @user:server <group>    · append a Matrix user after the active week
  !cleaning remove @user:server <group> · safely remove a Matrix user from future turns
  !cleaning people [group]              · show ordered cleaning rotations
  !adduser / !removeuser                · legacy aliases for the commands above
  !addperson <name> <group>             · add a non-Matrix person to a group
  !removeperson <name> <group>          · remove a non-Matrix person from a group
  !addroom <group> <room>               · add a room to clean in a group
  !removeroom <group> <room>            · remove a room from a group
  !ical <person> [N]                    · get iCal for any person (admin)
  !icalreset <person>                   · reset iCal token for any person (admin)
  !listgroups                           · list all groups with active/disabled status
  !disablegroup <name>                  · exclude group from scheduling and stats
  !enablegroup <name>                   · re-include a previously disabled group
  !assign <group> [slot] <person> [week N]   · directly assign/change who cleans one week
  !unassign <group> [slot] [week N]          · clear who cleans one week (leave unassigned)
  !importplan [--replace] <YYYY-Www> <group>[/slot] <person> [; ...]  · one-time import of upcoming weeks from the old paper plan (--replace overrides already-frozen weeks)
  !validate                             · check state for consistency issues"#.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        domain::{CleaningSlot, SlotAssignment},
        state::State,
    };
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
                person_id: Some(first_id.clone()),
                source: Default::default(),
            },
            SlotAssignment {
                group_id: group_id.clone(),
                slot_index: 0,
                iso_year: next_year,
                iso_week: next_week,
                person_id: Some(first_id.clone()),
                source: Default::default(),
            },
            SlotAssignment {
                group_id: group_id.clone(),
                slot_index: 1,
                iso_year: next_year,
                iso_week: next_week,
                person_id: Some(second_id.clone()),
                source: Default::default(),
            },
        ];

        let removed = remove_future_assignments_for_person(&mut state, &first_id, &group_id);

        assert_eq!(removed, 1);
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
            person_id: Some(first_id.clone()),
            source: Default::default(),
        });

        assert_eq!(
            current_open_assignments(&state, &group_id, &first_id, 1),
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
            })
            .unwrap();

        assert!(current_open_assignments(&state, &group_id, &first_id, 1).is_empty());
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
        for ev in resolver::materialize(state, 1, weeks_ahead) {
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
            .responsible_person(group, year, week, 1)
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
            person_id: Some(first_id.clone()),
            source: Default::default(),
        });
        let (ctx, path, admin) = test_context(state);
        let outsider = OwnedUserId::try_from("@outsider:example.org").unwrap();

        let error = add_matrix_participant(&ctx, &outsider, &["@charlie:example.org", "2nd Floor"])
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "__not_admin__");

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
        assert_eq!(error.to_string(), "__not_admin__");

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
        assert_eq!(error.to_string(), "__not_admin__");

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

        let (fy, fw) = {
            let state = ctx.state.lock().await;
            crate::state::first_due_week(&state, 1)
        };

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
            resolver::materialize(&state, 1, 3)
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
        for cmd in [
            "!done",
            "!skip",
            "!undo",
            "!assign",
            "!unassign",
            "!takeover",
            "!acceptswap",
            "!importplan",
        ] {
            assert!(
                command_may_change_current_plan(cmd),
                "{cmd} must trigger a pinned-plan refresh"
            );
        }
        for cmd in ["!status", "!help", "!stats", "!cleanplan", "!groups", ""] {
            assert!(
                !command_may_change_current_plan(cmd),
                "{cmd} must not trigger a pinned-plan refresh"
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
            scheduler::build_weekly_plan(&state, cy, cw, 1, &state.cleaning_groups).0
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
            scheduler::build_weekly_plan(&state, cy, cw, 1, &state.cleaning_groups).0
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
            scheduler::build_weekly_plan(&state, cy, cw, 1, &state.cleaning_groups).0
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
        let (raw, _mxids) = scheduler::build_weekly_plan(&state, cy, cw, 1, &state.cleaning_groups);
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
        let (raw_next, _mxids) =
            scheduler::build_weekly_plan(&state, ny, nw, 1, &state.cleaning_groups);
        drop(state);
        assert!(raw_next.contains("Flo3"), "{raw_next}");
        assert!(
            !raw_next.contains('@'),
            "a person without a Matrix ID must never leave an @mxid token in the text: {raw_next}"
        );
        // Must not panic and must not fabricate a mention for a plain name.
        let plain_content =
            format::mentionify_with_names(&raw_next, &std::collections::HashMap::new());
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
                })
                .unwrap();
        }

        let state = ctx.state.lock().await;
        let (raw, _mxids) = scheduler::build_weekly_plan(&state, cy, cw, 1, &state.cleaning_groups);
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
            resolver::materialize(&state, 1, 1)
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
        let (fy, fw) = {
            let state = ctx.state.lock().await;
            crate::state::first_due_week(&state, 1)
        };

        // Materialize freezes Anna in via round-robin for the first due week.
        let events = {
            let state = ctx.state.lock().await;
            resolver::materialize(&state, 1, 1)
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
            resolver::materialize(&state, 1, 1)
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
        assert_eq!(error.to_string(), "__not_admin__");

        let _ = tokio::fs::remove_file(path).await;
    }

    // ── Rotation-queue regression tests ──────────────────────────────────────
    //
    // These exercise the core promise of the queue-based rotation: a join or
    // leave must never change an already-frozen future week that isn't
    // actually theirs.

    #[tokio::test]
    async fn join_after_five_materialized_weeks_leaves_them_untouched_and_seats_the_newcomer_next()
    {
        let (mut state, group_id, aid, bid, cid) = three_person_state();
        seed_materialized_weeks(&mut state, 5);
        let (y, w) = current_iso_week();
        let weeks: Vec<(i32, u32)> = (0..5).map(|i| add_weeks(y, w, i)).collect();
        let before: Vec<Option<PersonId>> = weeks
            .iter()
            .map(|&(y, w)| assignee_for(&state, &group_id, y, w))
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

        // The 5 already-planned weeks are byte-for-byte unchanged.
        for (i, &(y, w)) in weeks.iter().enumerate() {
            assert_eq!(
                assignee_for(&state, &group_id, y, w),
                before[i],
                "week index {i} must not change"
            );
        }
        // David's first real turn is the very next open week — ahead of
        // Carla's would-be second lap, not after a full extra lap. The join
        // deliberately only extends the frozen horizon by one due-cycle (so
        // a single early joiner can't claim the whole configured horizon —
        // see `group_horizon_weeks_ahead`), so this is the one new frozen week.
        let (y5, w5) = add_weeks(y, w, 5);
        assert_eq!(assignee_for(&state, &group_id, y5, w5), Some(david_id));

        // Weeks 6 and 7 aren't frozen yet, but the *eventual* rotation still
        // continues the same queue correctly, previewed on demand.
        let (y6, w6) = add_weeks(y, w, 6);
        assert_eq!(
            assignee_for(&state, &group_id, y6, w6),
            None,
            "not yet frozen"
        );
        assert_eq!(preview_assignee_for(&state, &group_id, y6, w6), Some(cid));
        let (y7, w7) = add_weeks(y, w, 7);
        assert_eq!(
            assignee_for(&state, &group_id, y7, w7),
            None,
            "not yet frozen"
        );
        assert_eq!(preview_assignee_for(&state, &group_id, y7, w7), Some(aid));

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
        // Carla are already queued but have zero turns. David joining must
        // not cut in front of either of them — head-insertion would.
        let (mut state, group_id, aid, ..) = three_person_state();
        seed_materialized_weeks(&mut state, 1);
        let (ctx, path, admin) = test_context_with_horizon(state, 8);
        let bob_id = ctx
            .state
            .lock()
            .await
            .find_person("Bob")
            .unwrap()
            .id
            .clone();

        cmd_addperson(&ctx, &admin, &["David", "Floor"])
            .await
            .unwrap();

        let state = ctx.state.lock().await;
        let david_id = state.find_person("David").unwrap().id.clone();
        let carla_id = state.find_person("Carla").unwrap().id.clone();

        // The one newly-frozen week (the join only extends the horizon by
        // one due-cycle) goes to Bob, not to the newcomer.
        let (y, w) = current_iso_week();
        let (y1, w1) = add_weeks(y, w, 1);
        assert_eq!(
            assignee_for(&state, &group_id, y1, w1),
            Some(bob_id),
            "must not skip Bob's first turn"
        );

        // Beyond that, Carla (also still waiting for her first turn) goes
        // next, and only then David — never before either of them, even
        // though the join inserted him ahead of Anna, who already had hers.
        let (y2, w2) = add_weeks(y, w, 2);
        assert_eq!(
            preview_assignee_for(&state, &group_id, y2, w2),
            Some(carla_id),
            "Carla is also still waiting for her first turn"
        );
        let (y3, w3) = add_weeks(y, w, 3);
        assert_eq!(
            preview_assignee_for(&state, &group_id, y3, w3),
            Some(david_id),
            "David gets his first turn only after Bob and Carla have had theirs"
        );
        let (y4, w4) = add_weeks(y, w, 4);
        assert_eq!(
            preview_assignee_for(&state, &group_id, y4, w4),
            Some(aid),
            "Anna, who already had a turn, repeats after David"
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
    async fn removeperson_with_future_assignments_only_refills_their_own_gaps() {
        let (mut state, group_id, aid, bid, cid) = three_person_state();
        seed_materialized_weeks(&mut state, 5);
        let (ctx, path, admin) = test_context_with_horizon(state, 8);

        let reply = cmd_removeperson(&ctx, &admin, &["Carla", "Floor"])
            .await
            .unwrap()
            .unwrap();
        assert!(reply.contains("Removed Carla"), "{reply}");

        let state = ctx.state.lock().await;
        let (y, w) = current_iso_week();
        // Weeks that were never Carla's are byte-for-byte unchanged.
        assert_eq!(assignee_for(&state, &group_id, y, w), Some(aid.clone()));
        let (y1, w1) = add_weeks(y, w, 1);
        assert_eq!(assignee_for(&state, &group_id, y1, w1), Some(bid.clone()));
        let (y3, w3) = add_weeks(y, w, 3);
        assert_eq!(
            assignee_for(&state, &group_id, y3, w3),
            Some(aid),
            "week 3 was already Anna's, unrelated to Carla"
        );
        let (y4, w4) = add_weeks(y, w, 4);
        assert_eq!(
            assignee_for(&state, &group_id, y4, w4),
            Some(bid),
            "week 4 was already Bob's, unrelated to Carla"
        );
        // Carla's own vacated week (index 2) was refilled from the queue,
        // not left empty and not reassigned to whoever Carla displaced.
        let (y2, w2) = add_weeks(y, w, 2);
        assert!(
            assignee_for(&state, &group_id, y2, w2).is_some(),
            "Carla's gap must be refilled"
        );
        assert_ne!(
            assignee_for(&state, &group_id, y2, w2),
            Some(cid),
            "not Carla — she left"
        );
        drop(state);
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
        let interval = ctx.config.schedule.interval_weeks;
        let horizon = group_horizon_weeks_ahead(&after, &group_id, interval);
        assert!(horizon > 0, "the join must have frozen at least one week");
        let replay_events = resolver::materialize(&after, interval, horizon);
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
                a.group_id == group_id
                    && a.iso_year == year
                    && a.iso_week == week
                    && a.slot_index == 0
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
            person_id: Some(aid.clone()),
            source: Default::default(),
        });
        state.slot_assignments.push(SlotAssignment {
            group_id: group_id.clone(),
            slot_index: 1,
            iso_year: year,
            iso_week: week,
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
                a.group_id == group_id
                    && a.iso_year == year
                    && a.iso_week == week
                    && a.slot_index == 1
            })
            .unwrap();
        assert_eq!(assignment.person_id.as_deref(), Some(bid.as_str()));
        drop(state);
        let _ = tokio::fs::remove_file(path).await;
    }

    // ── Production-audit regression tests ────────────────────────────────

    #[tokio::test]
    async fn swap_is_refused_for_multi_slot_groups_instead_of_silently_targeting_slot_zero() {
        // !acceptswap always writes slot_index 0 (it predates slots). For a
        // multi-slot group that would silently overwrite whichever slot
        // happens to be first, not the one the requester actually holds —
        // so !swap must refuse up front rather than let that happen.
        let (state, group_id, ..) = multi_slot_group_matrix();
        let (ctx, path, _admin) = test_context(state);
        let alice = OwnedUserId::try_from("@alice:example.org").unwrap();

        let reply = cmd_swap(&ctx, &alice, &["@bob:example.org", "Floor"])
            .await
            .unwrap()
            .unwrap();
        assert!(reply.contains("multiple slots"), "{reply}");

        let state = ctx.state.lock().await;
        assert!(
            state.swap_requests.is_empty(),
            "no swap request should be created for a multi-slot group"
        );
        let _ = group_id;
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
                person_id: Some(aid.clone()),
                source: Default::default(),
            },
            SlotAssignment {
                group_id: gid.clone(),
                slot_index: 2,
                iso_year: year,
                iso_week: week,
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
}
