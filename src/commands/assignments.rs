//! Assignments: !assign, !unassign, !importplan, !takeover, !undo, !next, !skip, !remind.

use super::*;

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

pub(crate) async fn cmd_assign(
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

pub(crate) async fn cmd_unassign(
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
pub(crate) fn parse_iso_week_token(s: &str) -> Option<(i32, u32)> {
    let (y, w) = s.split_once("-W").or_else(|| s.split_once("-w"))?;
    let year: i32 = y.parse().ok()?;
    let week: u32 = w.parse().ok()?;
    (1..=53).contains(&week).then_some((year, week))
}

pub(crate) struct PlannedImport {
    pub(crate) raw: String,
    pub(crate) group_id: GroupId,
    pub(crate) group_name: String,
    pub(crate) slot_index: usize,
    pub(crate) slot_suffix: String,
    pub(crate) year: i32,
    pub(crate) week: u32,
    pub(crate) person: Person,
}

pub(crate) async fn cmd_importplan(
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

pub(crate) async fn cmd_takeover(
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

pub(crate) async fn cmd_undo(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
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

pub(crate) async fn cmd_next(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
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

pub(crate) async fn cmd_skip(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
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

/// One group to remind: (group id, group name, rooms text, mentioned MXIDs).
type ReminderRow = (String, String, Option<String>, Vec<String>);

pub(crate) async fn cmd_remind(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    require_admin(ctx, sender)?;

    let (year, week) = current_iso_week();
    let interval = ctx.config.schedule.interval_weeks;

    let (reminder_data, reply_to_plan): (Vec<ReminderRow>, Option<String>) = {
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
