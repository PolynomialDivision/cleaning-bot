//! Admin setup: persons, Matrix links, floors, slots, rooms, weights, absences.

use super::*;

// ── Admin: !member add|remove <@user:server | name> <group> ──────────────────
//
// One command for both kinds of people: a Matrix ID adds/removes that Matrix
// user, anything else a person without Matrix (by name).

pub(crate) async fn cmd_member_add(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    match args.first() {
        Some(who) if who.starts_with('@') => add_matrix_participant(ctx, sender, args).await,
        _ => cmd_addperson(ctx, sender, args).await,
    }
}

pub(crate) async fn cmd_member_remove(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    match args.first() {
        Some(who) if who.starts_with('@') => remove_matrix_participant(ctx, sender, args).await,
        _ => cmd_removeperson(ctx, sender, args).await,
    }
}

// ── Admin: !groups slot|room|weight … ─────────────────────────────────────────

pub(crate) async fn cmd_groups_slot(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    match args.first().map(|a| a.to_ascii_lowercase()).as_deref() {
        Some("add") => cmd_addslot(ctx, sender, &args[1..]).await,
        Some("remove") => cmd_removeslot(ctx, sender, &args[1..]).await,
        _ => Ok(Some("Usage: !groups slot add|remove <group> <slot>".into())),
    }
}

pub(crate) async fn cmd_groups_room(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    match args.first().map(|a| a.to_ascii_lowercase()).as_deref() {
        Some("add") => cmd_addroom(ctx, sender, &args[1..]).await,
        Some("remove") => cmd_removeroom(ctx, sender, &args[1..]).await,
        _ => Ok(Some(
            "Usage: !groups room add|remove <group> [<slot>] <room>".into(),
        )),
    }
}

/// `!groups weight <group> <factor>` or `!groups weight <group> <room> <factor>`.
pub(crate) async fn cmd_weight(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    match args.len() {
        2 => cmd_setgroupweight(ctx, sender, args).await,
        3 => cmd_setroomweight(ctx, sender, args).await,
        _ => Ok(Some(
            "Usage: !groups weight <group> [<room>] <factor>  (e.g. 2.0 = twice the work; quote names with spaces)".into(),
        )),
    }
}

// ── Admin: !member add <name> <group> ─────────────────────────────────────────

pub(crate) async fn cmd_addperson(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (name, group_name) = match (args.first(), args.get(1)) {
        (Some(n), Some(f)) => (n.to_string(), f.to_string()),
        _ => {
            return Ok(Some(
                "Usage: !member add <@user:server | name> <group>".into(),
            ))
        }
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

// ── Admin: !member remove <name> <group> ──────────────────────────────────────

pub(crate) async fn cmd_removeperson(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (query, group_name) = match (args.first(), args.get(1)) {
        (Some(n), Some(f)) => (n.to_string(), f.to_string()),
        _ => {
            return Ok(Some(
                "Usage: !member remove <@user:server | name> <group>".into(),
            ))
        }
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

// ── Admin: !member link <name> <@user:server> ─────────────────────────────────
//
// Normally refuses when `name` already has a Matrix ID linked — but if that
// *existing* matrix_id doesn't even parse as a valid Matrix user ID (data
// corruption: a manual edit, an old bug, ...), there is nothing legitimate to
// protect, so this repairs it in place instead of refusing. A person whose
// existing matrix_id already parses correctly is never touched this way —
// only an invalid one may be replaced, never a valid one overwritten by
// another.

pub(crate) async fn cmd_linkmatrix(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (name, mxid) = match (args.first(), args.get(1)) {
        (Some(n), Some(m)) => (n.to_string(), m.to_string()),
        _ => return Ok(Some("Usage: !member link <name> <@user:server>".into())),
    };
    if !mxid.starts_with('@') || !mxid.contains(':') {
        return Ok(Some(format!(
            "«{mxid}» does not look like a Matrix ID (@user:server)."
        )));
    }

    // Fetch the Matrix display name immediately so the record looks the same
    // as one created via !member add from the start. This is the only step that
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
pub(crate) fn person_has_activity(state: &crate::state::State, person_id: &str) -> bool {
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

/// Room-independent core of `!member link`: resolves `name`, decides whether
/// to link fresh, repair an invalid existing `matrix_id`, or refuse, then
/// performs the stub auto-merge and the actual `PersonMatrixLinked` event —
/// everything `cmd_linkmatrix` does except the live display-name fetch
/// (passed in as `fetched_display_name` so this stays testable without a
/// `Room`).
pub(crate) async fn apply_linkmatrix(
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

// ── Admin: !groups add <name> ───────────────────────────────────────────────────

pub(crate) async fn cmd_addfloor(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !groups add <name>".into())),
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

// ── Admin: !groups remove <name> ────────────────────────────────────────────────

pub(crate) async fn cmd_removefloor(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !groups remove <name>".into())),
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

// ── Admin: !groups slot add <group> <slot_name> ──────────────────────────────────────

// ── Admin: !plan reset <group> ─────────────────────────────────────────────────
// Clears all future (>= today) slot assignments for a group and rematerializes.
// Use this after the initial setup when you've added all members and want the
// rotation to distribute fairly from now on.  Safe to run at any time — past
// completed weeks are never touched.

pub(crate) async fn cmd_resetplan(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let group_name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !plan reset <group>".into())),
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

pub(crate) async fn cmd_addslot(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (group_name, slot_name) = match (args.first(), args.get(1..).map(|s| s.join(" "))) {
        (Some(g), Some(s)) if !s.is_empty() => (g.to_string(), s),
        _ => return Ok(Some("Usage: !groups slot add <group> <slot>".into())),
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

// ── Admin: !groups slot remove <group> <slot_name> ───────────────────────────────────

pub(crate) async fn cmd_removeslot(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (group_name, slot_name) = match (args.first(), args.get(1..).map(|s| s.join(" "))) {
        (Some(g), Some(s)) if !s.is_empty() => (g.to_string(), s),
        _ => return Ok(Some("Usage: !groups slot remove <group> <slot>".into())),
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

// ── Admin: !groups room add <group> [<slot>] <room> ──────────────────────────────────
//
// If the group has slots and the second argument matches a slot name, the room
// is added to that slot.  Otherwise the room is added to the group directly
// (single-slot mode, or a group-level room for backwards compatibility).

pub(crate) async fn cmd_addroom(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    if args.len() < 2 {
        return Ok(Some(
            "Usage: !groups room add <group> [<slot>] <room>".into(),
        ));
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
                "«{group_name}» has slots. Usage: !groups room add \"{group_name}\" <slot> <room>.\nSlots: {}",
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

// ── Admin: !groups room remove <group> [<slot>] <room> ───────────────────────────────

pub(crate) async fn cmd_removeroom(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    if args.len() < 2 {
        return Ok(Some(
            "Usage: !groups room remove <group> [<slot>] <room>".into(),
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

// ── Admin: !groups weight <group> <room> <weight> ────────────────────────────

pub(crate) async fn cmd_setroomweight(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (group_name, room_name, weight_str) = match (args.first(), args.get(1), args.get(2)) {
        (Some(g), Some(r), Some(w)) => (*g, *r, *w),
        _ => {
            return Ok(Some(
                "Usage: !groups weight <group> <room> <factor>  (e.g. 2.0 for twice the load)"
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

// ── Admin: !groups weight <group> <weight> ───────────────────────────────────

pub(crate) async fn cmd_setgroupweight(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (group_name, weight_str) = match (args.first(), args.get(1)) {
        (Some(g), Some(w)) => (*g, *w),
        _ => {
            return Ok(Some(
                "Usage: !groups weight <group> <factor>  (e.g. 2.0 for twice the load)".into(),
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

// ── Admin: !member away <person> [group] [weeks] ───────────────────────────────────
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
// stands; get someone else onto it with !takeover, !swap, or !plan assign.

pub(crate) async fn cmd_absent(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let person_query = match args.first() {
        Some(u) => u.to_string(),
        None => {
            return Ok(Some(
                "Usage: !member away <person> [weeks]  (default 4)".into(),
            ))
        }
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

// ── Admin: !member back <person> ─────────────────────────────────────────────────────

pub(crate) async fn cmd_back(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let query = match args.first() {
        Some(u) => u.to_string(),
        None => return Ok(Some("Usage: !member back <person>".into())),
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
