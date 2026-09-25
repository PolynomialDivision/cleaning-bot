//! Member commands: !done, !stats <person>, !join, !leave.

use super::*;

// ── !done [group] ─────────────────────────────────────────────────────────────
//
// Marks the sender's own open turns of this week — those that have started,
// or the next one if none has (see `markable_duties`). With a group named,
// only that group; a member of a group without slots may also mark its
// running turn for someone else (credit goes to whoever marks it).

pub(crate) async fn cmd_done(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let current = current_iso_week();
    let mut state = ctx.state.lock().await;

    let Some(sender_person_id) = state
        .person_by_matrix_id(sender.as_str())
        .map(|p| p.id.clone())
    else {
        return Ok(Some(
            "You are not registered. Join a group with !join <group>.".into(),
        ));
    };

    let group = if args.is_empty() {
        None
    } else {
        let name = args.join(" ");
        match state.group_by_name(&name) {
            Some(g) => Some(g.clone()),
            None => return Ok(Some(format!("Group «{name}» not found."))),
        }
    };

    let mut duties = markable_duties(
        &state,
        &sender_person_id,
        current,
        group.as_ref().map(|g| &g.id),
    );
    if duties.is_empty() {
        // Covering for someone: a member of a group without slots may mark
        // its running turn (or, bare, their only such group's).
        let covering: Vec<CleaningGroup> = match &group {
            Some(g) => vec![g.clone()],
            None => state
                .groups_for_person(&sender_person_id)
                .into_iter()
                .filter(|g| g.is_active && !g.is_multi_slot())
                .filter(|g| state.current_turn(g).is_some())
                .cloned()
                .collect(),
        };
        match covering.as_slice() {
            [g] if !g.is_multi_slot() && g.member_ids.contains(&sender_person_id) => {
                if let Some(turn) = state.current_turn(g) {
                    if state.is_turn_done(g, turn) {
                        return Ok(Some(format!(
                            "Already done: {}",
                            Duty {
                                group: g.clone(),
                                slot_index: 0,
                                turn
                            }
                            .label()
                        )));
                    }
                    duties.push(Duty {
                        group: g.clone(),
                        slot_index: 0,
                        turn,
                    });
                }
            }
            [g] if !g.member_ids.contains(&sender_person_id) => {
                return Ok(Some(format!("You are not a member of «{}».", g.name)))
            }
            [g] if g.is_multi_slot() => {
                return Ok(Some(format!(
                "You have no open slot in «{}» this week. (!takeover {} <slot> to take one over.)",
                g.name, g.name
            )))
            }
            _ if state.groups_for_person(&sender_person_id).is_empty() => {
                return Ok(Some("You are not in any cleaning group.".into()))
            }
            _ => {}
        }
    }
    if duties.is_empty() {
        return Ok(Some(
            "Nothing open for you this week. (!status shows who cleans what; \
             !done <group> marks a group you cleaned for someone else.)"
                .into(),
        ));
    }

    mark_duties_done(&mut state, &sender_person_id, &duties)?;
    state.save(&ctx.state_path).await?;
    let labels: Vec<String> = duties.iter().map(Duty::label).collect();
    Ok(Some(format!("✅ Cleaned: {}", labels.join(", "))))
}

// ── !stats <person> ────────────────────────────────────────────────────────────

pub(crate) async fn cmd_stats(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let (start_y, start_w) = state.tracking_start();

    // Per-person view.
    if !args.is_empty() {
        let query = args.join(" ");
        let query = query.as_str();
        let person_id = match state.find_person(query).map(|p| p.id.clone()) {
            Some(id) => id,
            None => return Ok(Some(format!("Person «{query}» not found."))),
        };
        let ps = match analytics::person_stats(&state, &person_id) {
            Some(s) => s,
            None => return Ok(Some(format!("{query} is not in any cleaning group."))),
        };
        let mut completions: Vec<_> = state
            .completions
            .iter()
            .filter(|c| c.completed_by_id == person_id && !c.skipped)
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
            format!("Groups: {}", ps.group_names),
            format!(
                "Own turns cleaned: {}/{} ({}%){streak_str}",
                ps.completed, ps.due_weeks, pct
            ),
            format!(
                "Missed: {} · Skipped: {} · Helped others: {}",
                ps.missed, ps.skipped, ps.helped
            ),
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

    Ok(Some(
        "Usage: !stats [person | group | fairness | load]".into(),
    ))
}

// ── !join <group> ────────────────────────────────────────────────────────

pub(crate) async fn cmd_joinfloor(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let group_name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !join <group>".into())),
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

// ── !leave <group> ───────────────────────────────────────────────────────

pub(crate) async fn cmd_leavefloor(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let group_name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !leave <group>".into())),
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
    let open = current_open_assignments(&state, &group_id, &person_id);
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
