//! Swapping duties: !swap, !swap accept, !swap reject.

use super::*;

// ── !swap @target [group] [week N] ───────────────────────────────────────────

pub(crate) async fn cmd_swap(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
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

    // !swap/!swap accept only ever write slot_index 0 (see cmd_acceptswap) —
    // fine for single-slot groups, but silently wrong for multi-slot ones
    // (it would overwrite whichever slot happens to be first, not the one
    // the requester actually holds). Point at the slot-aware commands instead.
    if group.is_multi_slot() {
        return Ok(Some(format!(
            "«{}» has multiple slots — !swap doesn't support slot selection. \
             Use !takeover {} <slot> or ask an admin for !plan assign instead.",
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
        "🔄 Swap #{id} · «{}» week {week} ({})\n{target_mxid}: !swap accept {id} or !swap reject {id}",
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
    // frozen `SlotAssignment` !plan assign and !takeover use, so !done, !status,
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
