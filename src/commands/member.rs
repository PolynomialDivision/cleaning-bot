//! Member commands: !done, !status, !stats, !floors, !joinfloor, !leavefloor.

use super::*;

// ── !done [group] ─────────────────────────────────────────────────────────────

pub(crate) async fn cmd_done(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
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

pub(crate) async fn cmd_status(ctx: &BotContext) -> Result<Option<String>> {
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

pub(crate) async fn cmd_stats(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
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
        completions.sort_by_key(|c| std::cmp::Reverse(c.completed_at));
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

pub(crate) async fn cmd_floors(ctx: &BotContext) -> Result<Option<String>> {
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

pub(crate) async fn cmd_joinfloor(
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

pub(crate) async fn cmd_leavefloor(
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
