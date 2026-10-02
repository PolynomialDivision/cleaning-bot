//! Adding and removing Matrix users (!member add/remove @user:server).

use super::*;

// ── Rotation management ───────────────────────────────────────────────────────

pub(crate) async fn add_matrix_participant(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (mxid, group_name) = match (args.first(), args.get(1)) {
        (Some(mxid), Some(group)) => (*mxid, *group),
        _ => return Ok(Some("Usage: !member add @user:server <group>".to_owned())),
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
    let replanned_from = apply_group_join(ctx, &mut state, &group_id, &person_id)?;
    let summary = join_summary(&state, &group_id, &person_id, replanned_from);
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "✅ Added {mxid} to «{group_name}».\n{summary}"
    )))
}

pub(crate) async fn remove_matrix_participant(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (mxid, group_name) = match (args.first(), args.get(1)) {
        (Some(mxid), Some(group)) => (*mxid, *group),
        _ => {
            return Ok(Some(
                "Usage: !member remove @user:server <group>".to_owned(),
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

    let open = current_open_assignments(&state, &group_id, &person_id);
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
    let departure = apply_group_departure(ctx, &mut state, &person_id, &group_id)?;
    let summary = departure_summary(&state, &group_id, &departure);
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "✅ Removed {mxid} from «{group_name}».\n{summary}"
    )))
}
