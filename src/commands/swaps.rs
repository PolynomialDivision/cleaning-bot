//! Swapping duties: !swap, !swap accept, !swap reject.

use super::*;

// ── !swap @target [group] [slot] [week N] ─────────────────────────────────────

pub(crate) async fn cmd_swap(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let target_mxid = match args.first() {
        Some(t) => *t,
        None => return Ok(Some("Usage: !swap @user [group] [slot] [week <N>]".into())),
    };
    if !target_mxid.starts_with('@') {
        return Ok(Some(
            "Swap targets must be Matrix users (@user:server).".into(),
        ));
    }
    let sender_mxid = sender.as_str();
    let (cur_y, cur_w) = current_iso_week();

    let Some(parsed) = extract_turn_args(&args[1..]) else {
        return Ok(Some(
            "Usage: !swap @user [group] [slot] [week <1-53>] [on <day>]".into(),
        ));
    };
    let group_args = parsed.rest.as_slice();
    let (year, week) = parsed.week;

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

    // Group: named, or else the sender's own group — not guessed among several.
    let (group, slot_token) = match group_args.first() {
        Some(&first) if state.group_by_name(first).is_some() => (
            state.group_by_name(first).unwrap().clone(),
            group_args.get(1).copied(),
        ),
        first => {
            let own: Vec<CleaningGroup> = state
                .groups_for_person(&sender_person_id)
                .into_iter()
                .cloned()
                .collect();
            match own.as_slice() {
                [] => {
                    return Ok(Some(
                        "You are not in any group. Specify: !swap @user <group>".into(),
                    ))
                }
                [g] => (g.clone(), first.copied()),
                many => {
                    return Ok(Some(format!(
                        "You are in several groups — name one: {}\nUsage: !swap @user <group> [<slot>] [week <N>]",
                        many.iter().map(|g| g.name.as_str()).collect::<Vec<_>>().join(", ")
                    )))
                }
            }
        }
    };
    let group_id = group.id.clone();

    // The sender's own open duties that week (optionally one slot / shift).
    let turns = match turns_for(&state, &group, (year, week), parsed.day) {
        Ok(t) => t,
        Err(e) => return Ok(Some(e)),
    };
    if slot_token.is_some() && !group.is_multi_slot() {
        return Ok(Some(format!("«{}» does not have slots.", group.name)));
    }
    let held: Vec<Duty> = turns
        .iter()
        .flat_map(|t| {
            state
                .held_slots(&group, &sender_person_id, *t)
                .into_iter()
                .map(|slot_index| Duty {
                    group: group.clone(),
                    slot_index,
                    turn: *t,
                })
        })
        .filter(|d| !state.is_turn_slot_done(&group, d.slot_index, d.turn))
        .collect();
    let chosen: Vec<Duty> = held
        .iter()
        .filter(|d| {
            slot_token.is_none_or(|name| {
                group
                    .slots
                    .get(d.slot_index)
                    .is_some_and(|s| s.name.eq_ignore_ascii_case(name))
            })
        })
        .cloned()
        .collect();
    if chosen.is_empty() && !held.is_empty() {
        return Ok(Some(format!(
            "«{}» isn't yours in week {week} — yours: {}",
            slot_token.unwrap_or_default(),
            held.iter().map(Duty::label).collect::<Vec<_>>().join(", ")
        )));
    }
    let duty = match chosen.as_slice() {
        [one] => one.clone(),
        [] => {
            return Ok(Some(format!(
                "You have no open turn in «{}» in week {week} — nothing to swap.",
                group.name
            )))
        }
        many => {
            return Ok(Some(format!(
                "Which one? Yours in week {week}: {}\nName the slot and/or add `on <day>`: !swap @user {} [<slot>] [week <N>] [on <day>]",
                many.iter().map(Duty::label).collect::<Vec<_>>().join(", "),
                group.name
            )))
        }
    };
    let (slot_index, turn) = (duty.slot_index, duty.turn);

    let dupe = state.swap_requests.iter().any(|s| {
        s.group_id == group_id
            && s.slot_index == slot_index
            && (s.iso_year, s.iso_week, s.shift) == (turn.year, turn.week, turn.shift)
            && s.status == SwapStatus::Pending
            && s.requester == sender_mxid
    });
    if dupe {
        return Ok(Some(format!(
            "You already have a pending swap for {}.",
            duty.label()
        )));
    }

    state.apply_event(DomainEvent::SwapRequested {
        group_id: group_id.clone(),
        requester_mxid: sender_mxid.to_owned(),
        target_mxid: target_mxid.to_owned(),
        iso_year: year,
        iso_week: week,
        slot_index,
        shift: turn.shift,
    })?;
    // The swap ID was allocated inside apply_event.
    let id = state.swap_requests.last().map(|s| s.id).unwrap_or(0);
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "🔄 Swap #{id} · {} · week {week} ({})\n{target_mxid}: !swap accept {id} or !swap reject {id}",
        duty.label(),
        turn.period_label(&group.rhythm)
    )))
}

// ── !swap accept <id> ──────────────────────────────────────────────────────────

pub(crate) async fn cmd_acceptswap(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let id: u64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return Ok(Some("Usage: !swap accept <id>".into())),
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
    let slot_index = req.slot_index;
    let turn = Turn::new(iso_year, iso_week, req.shift);
    let group = state.group_by_id(&group_id).cloned();
    let already_done = group
        .as_ref()
        .is_some_and(|g| state.is_turn_slot_done(g, slot_index, turn));
    if already_done {
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
    // PersonCreated is idempotent — the accepter must exist as a person to
    // be assigned (same self-registration as !takeover).
    state.apply_event(DomainEvent::PersonCreated {
        person_id: Uuid::new_v4().to_string(),
        display_name: sender_mxid.to_owned(),
        matrix_id: Some(sender_mxid.to_owned()),
    })?;
    let replacement_id = state
        .person_by_matrix_id(sender_mxid)
        .map(|p| p.id.clone())
        .expect("accepter was just registered");

    // The requester may no longer actually hold this week's assignment —
    // they could have left the group, or an admin/!takeover could have
    // reassigned it since the swap was requested. Accepting anyway would
    // silently hand the week to `sender` at the expense of whoever holds it
    // now, without their consent, so refuse and cancel the stale request
    // instead of blindly overwriting it.
    let current_holder_id = group
        .as_ref()
        .and_then(|g| state.slot_assignee(g, slot_index, turn))
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
        shift: turn.shift,
    })?;
    // The swap-request bookkeeping above records *that* a swap happened; this
    // is what actually makes the target the responsible person — the same
    // frozen `SlotAssignment` !plan assign and !takeover use, so !done, !status,
    // the pinned plan's ✅ reaction, PDF/iCal etc. all agree immediately,
    // even though the week was already materialized before the swap.
    state.apply_event(DomainEvent::SlotAssigned {
        group_id: group_id.clone(),
        slot_index,
        iso_year,
        iso_week,
        shift: turn.shift,
        person_id: Some(replacement_id),
        source: AssignmentSource::Swap,
        actor_id: Some(sender_mxid.to_owned()),
        previous_person_id: current_holder_id,
    })?;
    state.save(&ctx.state_path).await?;

    let what = group
        .map(|group| {
            Duty {
                group,
                slot_index,
                turn,
            }
            .label()
        })
        .unwrap_or_default();
    Ok(Some(format!(
        "✅ Swap #{id} accepted. {sender_mxid} will clean {what} in week {iso_week} instead of {requester}."
    )))
}

// ── !swap reject <id> ─────────────────────────────────────────────────────────

pub(crate) async fn cmd_rejectswap(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let id: u64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return Ok(Some("Usage: !swap reject <id>".into())),
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
