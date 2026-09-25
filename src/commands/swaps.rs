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

    let (group_args, (year, week)) = match extract_week_arg(&args[1..]) {
        Some(v) => v,
        None => {
            return Ok(Some(
                "Usage: !swap @user [group] [slot] [week <1-53>]".into(),
            ))
        }
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
    let interval = ctx.config.schedule.interval_weeks;

    // Slot: the sender's own slot that week (named, or the only one they hold).
    let slot_index = if group.is_multi_slot() {
        let held = held_tasks(&state, &group, &sender_person_id, year, week, interval);
        let chosen = match slot_token {
            Some(name) => held
                .iter()
                .find(|(_, slot)| slot.eq_ignore_ascii_case(name))
                .map(|(i, _)| i.unwrap_or(0)),
            None if held.len() == 1 => held[0].0,
            None => None,
        };
        match chosen {
            Some(i) => i,
            None if held.is_empty() => {
                return Ok(Some(format!(
                    "You have no slot in «{}» in week {week} — nothing to swap.",
                    group.name
                )))
            }
            None => {
                return Ok(Some(format!(
                    "Name the slot to swap — yours in «{}» week {week}: {}\nUsage: !swap @user {} <slot> [week <N>]",
                    group.name,
                    held.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join(", "),
                    group.name
                )))
            }
        }
    } else {
        if slot_token.is_some() {
            return Ok(Some(format!("«{}» does not have slots.", group.name)));
        }
        0
    };

    if !group.member_ids.contains(&sender_person_id)
        && !holds_any_slot(&state, &group, &sender_person_id, year, week, interval)
    {
        return Ok(Some(format!("You are not a member of «{}».", group.name)));
    }

    let dupe = state.swap_requests.iter().any(|s| {
        s.group_id == group_id
            && s.slot_index == slot_index
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
        slot_index,
    })?;
    // The swap ID was allocated inside apply_event.
    let id = state.swap_requests.last().map(|s| s.id).unwrap_or(0);
    state.save(&ctx.state_path).await?;

    let slot_suffix = group
        .slots
        .get(slot_index)
        .map(|s| format!(" / {}", s.name))
        .unwrap_or_default();
    Ok(Some(format!(
        "🔄 Swap #{id} · «{}»{slot_suffix} week {week} ({})\n{target_mxid}: !swap accept {id} or !swap reject {id}",
        group.name, week_dates(year, week)
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
    let interval = ctx.config.schedule.interval_weeks;
    let group = state.group_by_id(&group_id).cloned();
    let slot = group
        .as_ref()
        .and_then(|g| g.slots.get(slot_index))
        .cloned();

    let already_done = match &slot {
        Some(slot) => state.is_slot_completed(&group_id, &slot.id, iso_year, iso_week),
        None => state.is_completed(&group_id, iso_year, iso_week),
    };
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
        .and_then(|g| {
            if g.is_multi_slot() {
                state.slot_assignee(g, slot_index, iso_year, iso_week, interval)
            } else {
                state.responsible_person(g, iso_year, iso_week, interval)
            }
        })
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
    // frozen `SlotAssignment` !plan assign and !takeover use, so !done, !status,
    // the pinned plan's ✅ reaction, PDF/iCal etc. all agree immediately,
    // even though the week was already materialized before the swap.
    state.apply_event(DomainEvent::SlotAssigned {
        group_id: group_id.clone(),
        slot_index,
        iso_year,
        iso_week,
        person_id: Some(replacement_id),
        source: AssignmentSource::Swap,
        actor_id: Some(sender_mxid.to_owned()),
        previous_person_id: current_holder_id,
    })?;
    state.save(&ctx.state_path).await?;

    let group_name = group.map(|g| g.name).unwrap_or_default();
    let slot_suffix = slot.map(|s| format!(" / {}", s.name)).unwrap_or_default();
    Ok(Some(format!(
        "✅ Swap #{id} accepted. {sender_mxid} will clean «{group_name}»{slot_suffix} in week {iso_week} instead of {requester}."
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
