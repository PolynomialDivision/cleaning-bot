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
        let person_id = match lookup_person(&state, query) {
            Ok(Some(p)) => p.id.clone(),
            Ok(None) => return Ok(Some(format!("Person «{query}» not found."))),
            Err(ambiguous) => return Ok(Some(ambiguous)),
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

// ── !mygroups ────────────────────────────────────────────────────────────

/// `!mygroups` — the group selector again (the welcome, the first time).
pub(crate) async fn cmd_mygroups(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
) -> Result<Option<RoomMessageEventContent>> {
    if room.room_id() != ctx.room_id {
        return Ok(Some(format::mentionify(
            "!mygroups works in the cleaning room.",
        )));
    }
    let welcome = {
        let mut state = ctx.state.lock().await;
        let first = crate::onboarding::claim_welcome(&mut state, sender.as_str());
        state.save(&ctx.state_path).await?;
        first
    };
    crate::onboarding::post_selector(ctx, room, sender.as_str(), welcome).await?;
    Ok(None)
}

// ── !join <group> / !leave <group> ──────────────────────────────────────

pub(crate) async fn cmd_joinfloor(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let Some(group_name) = args.first() else {
        return Ok(Some("Usage: !join <group>".into()));
    };
    let mut state = ctx.state.lock().await;
    let Some(group_id) = state.group_by_name(group_name).map(|g| g.id.clone()) else {
        return Ok(Some(format!("Group «{group_name}» not found.")));
    };
    let reply = match join_group(ctx, &mut state, sender.as_str(), &group_id)? {
        Ok(summary) => format!(
            "✅ You joined {}\n{summary}",
            group_name_of(&state, &group_id)
        ),
        Err(why) => return Ok(Some(why)),
    };
    state.save(&ctx.state_path).await?;
    Ok(Some(reply))
}

pub(crate) async fn cmd_leavefloor(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let Some(group_name) = args.first() else {
        return Ok(Some("Usage: !leave <group>".into()));
    };
    let mut state = ctx.state.lock().await;
    if state.person_by_matrix_id(sender.as_str()).is_none() {
        return Ok(Some("You are not registered in any group.".into()));
    }
    let Some(group_id) = state.group_by_name(group_name).map(|g| g.id.clone()) else {
        return Ok(Some(format!("Group «{group_name}» not found.")));
    };
    let reply = match leave_group(ctx, &mut state, sender.as_str(), &group_id)? {
        Ok(summary) => format!(
            "✅ You left {}\n{summary}",
            group_name_of(&state, &group_id)
        ),
        Err(why) => return Ok(Some(why)),
    };
    state.save(&ctx.state_path).await?;
    Ok(Some(reply))
}

/// A Matrix user joins a group on their own (`!join`, the group selector):
/// their person is created if it doesn't exist yet — never a second one —
/// and the rotation re-planned from the next cycle (`apply_group_join`).
/// `Ok(summary)` for the reply, `Err(why)` when nothing changed.
pub(crate) fn join_group(
    ctx: &BotContext,
    state: &mut crate::state::State,
    mxid: &str,
    group_id: &GroupId,
) -> Result<std::result::Result<String, String>> {
    // PersonCreated is idempotent — safe even if this Matrix user already exists.
    state.apply_event(DomainEvent::PersonCreated {
        person_id: uuid::Uuid::new_v4().to_string(),
        display_name: mxid.to_owned(),
        matrix_id: Some(mxid.to_owned()),
    })?;
    let person_id = state
        .person_by_matrix_id(mxid)
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow::anyhow!("no person for {mxid} after PersonCreated"))?;
    if state.is_member(group_id, &person_id) {
        return Ok(Err(format!(
            "You are already in «{}».",
            group_name_of(state, group_id)
        )));
    }
    let replanned_from = apply_group_join(ctx, state, group_id, &person_id)?;
    Ok(Ok(join_summary(
        state,
        group_id,
        &person_id,
        replanned_from,
    )))
}

/// A Matrix user leaves a group on their own (`!leave`, the group
/// selector) — refused while their turn of this week is still open; their
/// upcoming turns are handed on (`apply_group_departure`). `Ok(summary)`
/// for the reply, `Err(why)` when nothing changed.
pub(crate) fn leave_group(
    ctx: &BotContext,
    state: &mut crate::state::State,
    mxid: &str,
    group_id: &GroupId,
) -> Result<std::result::Result<String, String>> {
    let group_name = group_name_of(state, group_id);
    let Some(person_id) = state
        .person_by_matrix_id(mxid)
        .map(|p| p.id.clone())
        .filter(|pid| state.is_member(group_id, pid))
    else {
        return Ok(Err(format!("You are not in «{group_name}».")));
    };
    let open = current_open_assignments(state, group_id, &person_id);
    if !open.is_empty() {
        return Ok(Err(format!(
            "You cannot leave «{group_name}» while your current assignment is open ({}). \
             Complete or skip it first.",
            open.join(", ")
        )));
    }
    state.apply_event(DomainEvent::PersonLeftGroup {
        person_id: person_id.clone(),
        group_id: group_id.clone(),
    })?;
    let departure = apply_group_departure(ctx, state, &person_id, group_id)?;
    Ok(Ok(departure_summary(state, group_id, &departure)))
}
