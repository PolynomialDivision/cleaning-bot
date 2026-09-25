//! Assignments: !plan assign, !plan unassign, !plan import, !takeover, !undo, !next, !plan skip, !plan remind.

use super::*;

// ── Admin: !plan assign <group> [<slot>] <person> [week <N>] [on <day>] ──────
//
// Directly sets who is responsible for one group/slot in one turn (default:
// the current week; `on <day>` picks the shift of a group split into
// shifts), overriding the rotation for that turn only. Stored as a frozen
// `SlotAssignment` — the same record the resolver produces, so !plan,
// !status, reminders, the PDF/iCal exports and the pinned weekly plan all
// pick it up for free.
//
// This does not touch group membership (`!member add/remove` own that) —
// it only edits who is on the hook for one turn, which is why the target
// person does not need to already be a rotation member.

/// Group, slot and turn named by `<group> [<slot>] … [week <N>] [on <day>]`;
/// returns the remaining arguments too.
fn resolve_admin_turn<'a>(
    state: &crate::state::State,
    args: &[&'a str],
) -> std::result::Result<(CleaningGroup, usize, Turn, Vec<&'a str>), Option<String>> {
    let parsed = extract_turn_args(args).ok_or(None)?;
    let Some((&group_name, rest)) = parsed.rest.split_first() else {
        return Err(None);
    };
    let (cur_y, cur_w) = current_iso_week();
    if parsed.week < (cur_y, cur_w) {
        return Err(Some(format!(
            "Week {} ({}) is in the past.",
            parsed.week.1,
            week_dates(parsed.week.0, parsed.week.1)
        )));
    }
    let (group_id, slot_index, rest) =
        resolve_group_and_slot(state, group_name, rest).map_err(Some)?;
    let group = state
        .group_by_id(&group_id)
        .expect("resolved group must exist")
        .clone();
    let turn = single_turn(state, &group, parsed.week, parsed.day).map_err(Some)?;
    Ok((group, slot_index, turn, rest.to_vec()))
}

/// "«Floor» / Kitchen · Thu–Sun 25 – 28 Sep (week 39)"
fn turn_title(group: &CleaningGroup, slot_index: usize, turn: Turn) -> String {
    let slot = group
        .slots
        .get(slot_index)
        .map(|s| format!(" / {}", s.name))
        .unwrap_or_default();
    format!(
        "«{}»{slot} · {} (week {})",
        group.name,
        turn.period_label(&group.rhythm),
        turn.week
    )
}

pub(crate) async fn cmd_assign(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let usage = "Usage: !plan assign <group> [<slot>] <person> [week <1-53>] [on <day>]";
    let mut state = ctx.state.lock().await;
    let (group, slot_index, turn, rest) = match resolve_admin_turn(&state, args) {
        Ok(v) => v,
        Err(e) => return Ok(Some(e.unwrap_or_else(|| usage.into()))),
    };
    if rest.is_empty() {
        return Ok(Some(usage.into()));
    }
    let person_query = rest.join(" ");
    let person = match state.find_person(&person_query) {
        Some(p) => p.clone(),
        None => {
            return Ok(Some(format!(
                "«{person_query}» is not registered. Add them with !member add first."
            )))
        }
    };
    let previous_id = state
        .slot_assignee(&group, slot_index, turn)
        .map(|p| p.id.clone());

    state.apply_event(DomainEvent::SlotAssigned {
        group_id: group.id.clone(),
        slot_index,
        iso_year: turn.year,
        iso_week: turn.week,
        shift: turn.shift,
        person_id: Some(person.id.clone()),
        source: AssignmentSource::Assign,
        actor_id: Some(sender.as_str().to_owned()),
        previous_person_id: previous_id.clone(),
    })?;
    state.save(&ctx.state_path).await?;

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
        "✅ Assigned {} to {}{membership_note}{changed_note}.",
        person_label(&person),
        turn_title(&group, slot_index, turn)
    )))
}

// ── Admin: !plan unassign <group> [<slot>] [week <N>] [on <day>] ─────────────
//
// Clears whoever is responsible for one group/slot in one turn (default: the
// current week), leaving it unassigned until reassigned via !plan assign.

pub(crate) async fn cmd_unassign(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let usage = "Usage: !plan unassign <group> [<slot>] [week <1-53>] [on <day>]";
    let mut state = ctx.state.lock().await;
    let (group, slot_index, turn, _rest) = match resolve_admin_turn(&state, args) {
        Ok(v) => v,
        Err(e) => return Ok(Some(e.unwrap_or_else(|| usage.into()))),
    };
    let previous_id = state
        .slot_assignee(&group, slot_index, turn)
        .map(|p| p.id.clone());
    state.apply_event(DomainEvent::SlotAssigned {
        group_id: group.id.clone(),
        slot_index,
        iso_year: turn.year,
        iso_week: turn.week,
        shift: turn.shift,
        person_id: None,
        source: AssignmentSource::Assign,
        actor_id: Some(sender.as_str().to_owned()),
        previous_person_id: previous_id,
    })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "✅ Cleared {} — left unassigned.",
        turn_title(&group, slot_index, turn)
    )))
}

// ── Admin: !plan import [--replace] <entry>[ ; <entry>]* ──────────────────────
//
// One-time migration helper: freeze the remaining upcoming weeks of the old
// paper cleaning plan into the bot. Each entry names one already-decided
// assignment; entries are frozen via the exact same `SlotAssigned` event and
// upsert semantics as `!plan assign` (`AssignmentSource::Import` only for a
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
//   !plan import 2025-W36 Kitchen @alice:example.org ; 2025-W36 Bathroom/Sink @bob:example.org
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
/// `YYYY-Www`, or `YYYY-Www:<day>` to name the shift containing that weekday.
pub(crate) fn parse_iso_week_token(s: &str) -> Option<(i32, u32, Option<u8>)> {
    let (week_part, day) = match s.split_once(':') {
        Some((w, d)) => (w, Some(parse_weekday(d)?)),
        None => (s, None),
    };
    let (y, w) = week_part
        .split_once("-W")
        .or_else(|| week_part.split_once("-w"))?;
    let year: i32 = y.parse().ok()?;
    let week: u32 = w.parse().ok()?;
    (1..=53).contains(&week).then_some((year, week, day))
}

pub(crate) struct PlannedImport {
    pub(crate) raw: String,
    pub(crate) group_id: GroupId,
    pub(crate) group_name: String,
    pub(crate) slot_index: usize,
    pub(crate) slot_suffix: String,
    pub(crate) year: i32,
    pub(crate) week: u32,
    pub(crate) shift: u8,
    /// "week 36 (1 – 7 Sep)" or "week 36 (Thu–Sun 4 – 7 Sep)".
    pub(crate) period: String,
    pub(crate) person: Person,
}

pub(crate) async fn cmd_importplan(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let usage = "Usage: !plan import [--replace] <YYYY-Www[:day]> <group>[/<slot>] <person> [; <YYYY-Www[:day]> <group>[/<slot>] <person> ...]";
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
        let Some((year, week, day)) = parse_iso_week_token(week_token) else {
            errors.push(format!(
                "«{raw}»: «{week_token}» is not a valid ISO week (expected YYYY-Www or YYYY-Www:<day>)."
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
                "«{raw}»: «{person_query}» is not registered. Use !member add first."
            ));
            continue;
        };
        let turn = match single_turn(&state, group, (year, week), day) {
            Ok(turn) => turn,
            Err(e) => {
                errors.push(format!("«{raw}»: {e} (as YYYY-Www:<day>)"));
                continue;
            }
        };
        let slot_suffix = group
            .slots
            .get(slot_index)
            .map(|s| format!("/{}", s.name))
            .unwrap_or_default();
        planned.push(PlannedImport {
            shift: turn.shift,
            period: format!("week {week} ({})", turn.period_label(&group.rhythm)),
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
                && (a.year, a.week, a.shift) == (b.year, b.week, b.shift)
                && a.person.id != b.person.id
            {
                errors.push(format!(
                    "«{}» and «{}» both claim {}{} {} — conflicting entries in this import.",
                    a.raw, b.raw, a.group_name, a.slot_suffix, a.period
                ));
            }
        }
    }

    // Conflicts against whatever is already persisted (round-robin, a prior
    // manual !plan assign, or an earlier import). A slot with no record at all is
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
                && (a.iso_year, a.iso_week, a.shift) == (p.year, p.week, p.shift)
        });
        if existing.is_some_and(|a| a.person_id.as_deref() == Some(p.person.id.as_str())) {
            already_imported += 1;
            continue;
        }

        let group = state
            .group_by_id(&p.group_id)
            .expect("resolved group must exist");
        let already_done =
            state.is_turn_slot_done(group, p.slot_index, Turn::new(p.year, p.week, p.shift));
        if already_done {
            errors.push(format!(
                "«{}»: {}{} for {} is already completed/skipped — cannot import over a finished turn.",
                p.raw, p.group_name, p.slot_suffix, p.period
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
                    "«{}»: {}{} for {} is already assigned to {holder} — use !plan import --replace to override, or !plan unassign it first.",
                    p.raw, p.group_name, p.slot_suffix, p.period
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
        shift: p.shift,
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
                "• {}{} {} → {} (added)",
                p.group_name,
                p.slot_suffix,
                p.period,
                person_label(&p.person)
            )
        })
        .collect();
    lines.extend(to_replace.iter().map(|(p, prev)| {
        format!(
            "• {}{} {} → {} (replaced {prev})",
            p.group_name,
            p.slot_suffix,
            p.period,
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

// ── !takeover [<group>] [<slot>] [week <N>] [on <day>] ──────────────────────
//
// Self-service handoff: the sender claims a turn away from whoever currently
// has it, whether that's the regular rotation pick or an earlier manual
// assignment — a frozen `SlotAssigned` with `source: Takeover`, so it never
// touches `rotation_queue` or any other turn.
//
// Group, slot, week and shift are all optional (see
// `resolve_takeover_target`): bare `!takeover` means the sender's own group,
// the running (or only open) turn, and auto-picks the slot when exactly one
// is takeable — never guessing between several.
//
// Refuses a turn that's already completed or skipped, so a finished task
// can't be silently reassigned out from under its record.

pub(crate) async fn cmd_takeover(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let usage = "Usage: !takeover [<group>] [<slot>] [week <1-53>] [on <day>]";
    let Some(parsed) = extract_turn_args(args) else {
        return Ok(Some(usage.into()));
    };
    let (cur_y, cur_w) = current_iso_week();
    if parsed.week < (cur_y, cur_w) {
        return Ok(Some(format!(
            "Week {} ({}) is in the past.",
            parsed.week.1,
            week_dates(parsed.week.0, parsed.week.1)
        )));
    }

    let sender_mxid = sender.as_str();
    let mut state = ctx.state.lock().await;

    // PersonCreated is idempotent — same self-registration as !join.
    state.apply_event(DomainEvent::PersonCreated {
        person_id: Uuid::new_v4().to_string(),
        display_name: sender_mxid.to_owned(),
        matrix_id: Some(sender_mxid.to_owned()),
    })?;
    let sender_person_id = state.person_by_matrix_id(sender_mxid).unwrap().id.clone();

    let (group, slot_index, turn) = match resolve_takeover_target(
        &state,
        &sender_person_id,
        &parsed.rest,
        parsed.week,
        parsed.day,
    ) {
        Ok(v) => v,
        Err(e) => return Ok(Some(e)),
    };
    let title = turn_title(&group, slot_index, turn);
    if state.is_turn_slot_done(&group, slot_index, turn) {
        return Ok(Some(format!(
            "{title} is already completed or skipped — nothing to take over."
        )));
    }
    let previous_id = state
        .slot_assignee(&group, slot_index, turn)
        .map(|p| p.id.clone());
    if previous_id.as_deref() == Some(sender_person_id.as_str()) {
        return Ok(Some(format!("You are already responsible for {title}.")));
    }

    state.apply_event(DomainEvent::SlotAssigned {
        group_id: group.id.clone(),
        slot_index,
        iso_year: turn.year,
        iso_week: turn.week,
        shift: turn.shift,
        person_id: Some(sender_person_id.clone()),
        source: AssignmentSource::Takeover,
        actor_id: Some(sender_mxid.to_owned()),
        previous_person_id: previous_id.clone(),
    })?;
    state.save(&ctx.state_path).await?;

    let membership_note = if group.member_ids.contains(&sender_person_id) {
        String::new()
    } else {
        format!(" (not a member of «{}» — one-off takeover)", group.name)
    };
    let from_note = previous_id
        .map(|prev_id| {
            let prev_label = state
                .person_by_id(&prev_id)
                .map(person_label)
                .unwrap_or_else(|| "nobody".into());
            format!(" from {prev_label}")
        })
        .unwrap_or_default();
    Ok(Some(format!(
        "✅ You took over {title}{from_note}{membership_note}."
    )))
}

// ── !undo [group] ─────────────────────────────────────────────────────────────
//
// Takes back this week's done marks. When several people share the week
// (slots, or shifts), a member only takes back *their own* marks — the
// slots/shifts they hold or marked — never someone else's. An admin naming
// the group explicitly clears the whole group's week (including skips).

pub(crate) async fn cmd_undo(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let (year, week) = current_iso_week();
    let is_admin = ctx.admin_users.contains(sender);
    let mut state = ctx.state.lock().await;
    let sender_pid = state
        .person_by_matrix_id(sender.as_str())
        .map(|p| p.id.clone());

    let explicit = !args.is_empty();
    let groups: Vec<CleaningGroup> = if explicit {
        let name = args.join(" ");
        match state.group_by_name(&name) {
            Some(g) => vec![g.clone()],
            None => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        match &sender_pid {
            Some(pid) => state
                .cleaning_groups
                .iter()
                .filter(|g| {
                    g.member_ids.contains(pid) || holds_any_turn(&state, g, pid, (year, week))
                })
                .cloned()
                .collect(),
            None => Vec::new(),
        }
    };
    if groups.is_empty() {
        return Ok(Some("You are not assigned to any group.".into()));
    }

    let mut undone = vec![];
    let mut no_perm = vec![];
    for group in &groups {
        let is_member = sender_pid
            .as_ref()
            .is_some_and(|pid| group.member_ids.contains(pid));
        let holds = sender_pid
            .as_ref()
            .is_some_and(|pid| holds_any_turn(&state, group, pid, (year, week)));
        if !is_member && !holds && !is_admin {
            no_perm.push(group.name.clone());
            continue;
        }

        let shared = group.is_multi_slot() || group.rhythm.is_split();
        if !shared || (is_admin && explicit) {
            // The whole week of the group.
            let marked = state
                .completions
                .iter()
                .any(|c| c.group_id == group.id && (c.iso_year, c.iso_week) == (year, week));
            if marked {
                state.apply_event(DomainEvent::CleaningUndone {
                    group_id: group.id.clone(),
                    iso_year: year,
                    iso_week: week,
                    slot_id: None,
                    shift: None,
                })?;
                undone.push(group.name.clone());
            }
            continue;
        }

        let Some(pid) = &sender_pid else { continue };
        for turn in state.turns_in_week(group, year, week) {
            for slot_index in crate::state::State::slot_indices(group) {
                let Some(c) = state.completion_for(group, slot_index, turn) else {
                    continue;
                };
                let own = &c.completed_by_id == pid
                    || state
                        .slot_assignee(group, slot_index, turn)
                        .is_some_and(|p| &p.id == pid);
                if !own {
                    continue;
                }
                state.apply_event(DomainEvent::CleaningUndone {
                    group_id: group.id.clone(),
                    iso_year: year,
                    iso_week: week,
                    slot_id: group.slots.get(slot_index).map(|s| s.id.clone()),
                    shift: Some(turn.shift),
                })?;
                undone.push(
                    Duty {
                        group: group.clone(),
                        slot_index,
                        turn,
                    }
                    .label(),
                );
            }
        }
    }
    // Forget ✅-reaction trackers of this week for the undone groups (for a
    // shared week: the sender's own trackers only).
    let undone_groups: Vec<GroupId> = groups.iter().map(|g| g.id.clone()).collect();
    state.reaction_dones.retain(|_, rd| {
        !(undone_groups.contains(&rd.group_id)
            && (rd.iso_year, rd.iso_week) == (year, week)
            && (is_admin && explicit || sender_pid.as_deref() == Some(rd.completed_by_id.as_str())))
    });

    state.save(&ctx.state_path).await?;
    let mut lines = vec![];
    if !undone.is_empty() {
        lines.push(format!("↩️ Undone: {}", undone.join(", ")));
    }
    if !no_perm.is_empty() {
        lines.push(format!("❌ Not your group: {}", no_perm.join(", ")));
    }
    if lines.is_empty() {
        lines.push("Nothing of yours to undo this week.".into());
    }
    Ok(Some(lines.join("\n")))
}

/// True when `person_id` holds any slot of any turn of `group` that week.
pub(crate) fn holds_any_turn(
    state: &crate::state::State,
    group: &CleaningGroup,
    person_id: &PersonId,
    (year, week): (i32, u32),
) -> bool {
    state
        .turns_in_week(group, year, week)
        .into_iter()
        .any(|t| !state.held_slots(group, person_id, t).is_empty())
}

// ── !next [person] ────────────────────────────────────────────────────────────
//
// The person's own next turn: the earliest turn (in any of their groups,
// each in its own rhythm) they hold that isn't done and isn't over yet —
// frozen plan first, rotation preview beyond it. Turns of this week that are
// already done are mentioned too.

pub(crate) async fn cmd_next(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let current = current_iso_week();

    let query = if args.is_empty() {
        sender.as_str().to_owned()
    } else {
        args.join(" ")
    };
    let person = match state.find_person(&query) {
        Some(p) => p.clone(),
        None => return Ok(Some(format!("{query} is not registered."))),
    };
    let groups: Vec<CleaningGroup> = state
        .groups_for_person(&person.id)
        .into_iter()
        .filter(|g| g.is_active)
        .cloned()
        .collect();
    if groups.is_empty() {
        return Ok(Some(format!(
            "{} is not in any cleaning group.",
            person.display_name
        )));
    }

    let horizon = add_weeks(current.0, current.1, 104);
    let mut done_now: Vec<String> = Vec::new();
    let mut next: Option<(chrono::NaiveDate, Vec<Duty>)> = None;
    for group in &groups {
        for turn in state.turns_between(group, current, horizon) {
            let start = turn.dates(&group.rhythm).0;
            if next.as_ref().is_some_and(|(d, _)| start > *d) {
                break;
            }
            for slot_index in state.held_slots(group, &person.id, turn) {
                let duty = Duty {
                    group: group.clone(),
                    slot_index,
                    turn,
                };
                if state.is_turn_slot_done(group, slot_index, turn) {
                    if turn.week() == current {
                        done_now.push(duty.label());
                    }
                } else if !state.turn_over(group, turn) {
                    match &mut next {
                        Some((d, duties)) if *d == start => duties.push(duty),
                        Some((d, _)) if *d < start => {}
                        _ => next = Some((start, vec![duty])),
                    }
                }
            }
        }
    }

    let Some((start, duties)) = next else {
        return Ok(Some(format!(
            "No upcoming turn found for {} in the next two years.",
            person.display_name
        )));
    };
    let first = &duties[0];
    let today = crate::state::today();
    let (_, end) = first.turn.dates(&first.group.rhythm);
    let weeks_away = crate::state::weeks_between(current, first.turn.week());
    let when = if start <= today {
        format!("now — this week, until {} ⚠️", end.format("%a %-d %b"))
    } else if weeks_away == 0 {
        format!("later this week, from {}", start.format("%a %-d %b"))
    } else if weeks_away == 1 {
        "next week".to_owned()
    } else {
        format!("in {weeks_away} weeks")
    };
    let mut reply = format!(
        "📅 Next turn for {}: **week {} · {}** ({when}) · {}",
        person.display_name,
        first.turn.week,
        first.turn.period_label(&first.group.rhythm),
        duties
            .iter()
            .map(Duty::label)
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !done_now.is_empty() {
        reply.push_str(&format!("\nAlready done: {} ✅", done_now.join(", ")));
    }
    Ok(Some(reply))
}

// ── Admin: !plan skip [group [slot]] [on <day>] ───────────────────────────────
//
// Excuses this week's open turns (not counted as missed): every due group,
// one group, one slot of it, and/or — with `on <day>` — one shift.

pub(crate) async fn cmd_skip(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let usage = "Usage: !plan skip [<group> [<slot>]] [on <day>]";
    let (year, week) = current_iso_week();
    let Some(parsed) = extract_turn_args(args) else {
        return Ok(Some(usage.into()));
    };
    let mut state = ctx.state.lock().await;

    // (group, only this slot)
    let targets: Vec<(CleaningGroup, Option<usize>)> = match parsed.rest.as_slice() {
        [] => state
            .cleaning_groups
            .iter()
            .filter(|g| state.belongs_in_weekly_plan(g, year, week))
            .map(|g| (g.clone(), None))
            .collect(),
        [name, slot @ ..] => {
            let Some(group) = state.group_by_name(name).cloned() else {
                return Ok(Some(format!("Group «{name}» not found.")));
            };
            if slot.is_empty() {
                vec![(group, None)]
            } else {
                let slot_name = slot.join(" ");
                match group
                    .slots
                    .iter()
                    .position(|s| s.name.eq_ignore_ascii_case(&slot_name))
                {
                    Some(i) => vec![(group, Some(i))],
                    None => {
                        return Ok(Some(format!(
                            "«{}» has no slot «{slot_name}».{}",
                            group.name,
                            if group.is_multi_slot() {
                                format!(
                                    " Slots: {}",
                                    group
                                        .slots
                                        .iter()
                                        .map(|s| s.name.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                            } else {
                                String::new()
                            }
                        )))
                    }
                }
            }
        }
    };

    let skipper = state
        .person_by_matrix_id(sender.as_str())
        .map(|p| p.id.clone())
        .unwrap_or_else(|| sender.as_str().to_owned());
    let mut skipped = vec![];
    let mut already = vec![];
    for (group, slot) in &targets {
        let turns = match turns_for(&state, group, (year, week), parsed.day) {
            Ok(t) => t,
            Err(e) => return Ok(Some(e)),
        };
        for turn in turns {
            let slots: Vec<usize> = match slot {
                Some(i) => vec![*i],
                None => crate::state::State::slot_indices(group).collect(),
            };
            let open = slots
                .iter()
                .any(|i| !state.is_turn_slot_done(group, *i, turn));
            let label = Duty {
                group: group.clone(),
                slot_index: slot.unwrap_or(usize::MAX),
                turn,
            }
            .label();
            if !open {
                already.push(label);
                continue;
            }
            state.apply_event(DomainEvent::CleaningSkipped {
                group_id: group.id.clone(),
                skipper_id: skipper.clone(),
                iso_year: turn.year,
                iso_week: turn.week,
                slot_id: slot.and_then(|i| group.slots.get(i)).map(|s| s.id.clone()),
                shift: Some(turn.shift),
            })?;
            skipped.push(label);
        }
    }

    state.save(&ctx.state_path).await?;
    let mut lines = vec![];
    if !skipped.is_empty() {
        lines.push(format!("⏭️ Skipped: {}", skipped.join(", ")));
    }
    if !already.is_empty() {
        lines.push(format!("Already done: {}", already.join(", ")));
    }
    if lines.is_empty() {
        lines.push("Nothing due this week.".into());
    }
    Ok(Some(lines.join("\n")))
}

// ── Admin: !plan remind [group] ───────────────────────────────────────────────
//
// Sends the reminder now: every open turn of this week that has started (or,
// if none has, the next one) — one line per open slot, mentioning whoever
// holds it.

pub(crate) async fn cmd_remind(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    require_admin(ctx, sender)?;
    let (year, week) = current_iso_week();

    let (msg, mxids, reply_to_plan, names) = {
        let state = ctx.state.lock().await;
        let groups: Vec<CleaningGroup> = match args.first() {
            Some(name) => match state.group_by_name(name) {
                Some(g) if g.is_active => vec![g.clone()],
                _ => {
                    return Ok(Some(format::mentionify(&format!(
                        "Group «{name}» not found or disabled."
                    ))))
                }
            },
            None => state
                .cleaning_groups
                .iter()
                .filter(|g| state.belongs_in_weekly_plan(g, year, week))
                .cloned()
                .collect(),
        };

        let mut lines = vec![format!("⏰ **Reminder · Week {week}**")];
        let mut mxids: Vec<String> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for group in &groups {
            let turns: Vec<Turn> = state
                .turns_in_week(group, year, week)
                .into_iter()
                .filter(|t| !state.is_turn_done(group, *t) && !state.turn_over(group, *t))
                .collect();
            let started: Vec<Turn> = turns
                .iter()
                .copied()
                .filter(|t| state.turn_started(group, *t))
                .collect();
            let chosen = if started.is_empty() {
                turns.into_iter().take(1).collect()
            } else {
                started
            };
            for turn in chosen {
                for (slot_index, assignee) in state.turn_assignees(group, turn) {
                    if state.is_turn_slot_done(group, slot_index, turn) {
                        continue;
                    }
                    let who = assignee
                        .map(|p| person_key(p).to_owned())
                        .unwrap_or_else(|| "(nobody assigned)".into());
                    if let Some(m) = assignee.and_then(|p| p.matrix_id.clone()) {
                        mxids.push(m);
                    }
                    let label = Duty {
                        group: group.clone(),
                        slot_index,
                        turn,
                    }
                    .label();
                    let rooms = match group.slots.get(slot_index) {
                        Some(slot) if !slot.room_names.is_empty() => {
                            format!(" · {}", slot.room_names.join(", "))
                        }
                        Some(_) => String::new(),
                        None => group
                            .rooms_text()
                            .map(|r| format!(" · {}", r.replace('\n', " · ")))
                            .unwrap_or_default(),
                    };
                    lines.push(format!("**{label}** · {who}{rooms}"));
                    names.push(label);
                }
            }
        }
        let week_key = format!("{year}-W{week:02}");
        (
            lines.join("\n"),
            mxids,
            state.weekly_plan_canonical.get(&week_key).cloned(),
            names,
        )
    };

    if names.is_empty() {
        return Ok(Some(format::mentionify(
            "✅ Nothing due and uncleaned right now.",
        )));
    }

    let uid_refs: Vec<&str> = mxids.iter().map(String::as_str).collect();
    let fetched = format::fetch_names(room, &uid_refs).await;
    let parsed: Vec<matrix_sdk::ruma::OwnedUserId> =
        mxids.iter().filter_map(|s| s.parse().ok()).collect();
    let mut content = format::mentionify_with_names(&msg, &fetched)
        .add_mentions(matrix_sdk::ruma::events::Mentions::with_user_ids(parsed));
    if let Some(plan_eid) = reply_to_plan.and_then(|e| e.parse::<OwnedEventId>().ok()) {
        content.relates_to = Some(Relation::Reply(Reply::with_event_id(plan_eid)));
    }
    if let Err(e) = room.send(content).await {
        tracing::warn!("!plan remind send failed: {e}");
        return Ok(Some(format::mentionify("❌ Sending the reminder failed.")));
    }
    Ok(Some(format::mentionify(&format!(
        "✅ Reminder sent for: {}",
        names.join(", ")
    ))))
}
