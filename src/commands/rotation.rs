//! Rotation management commands.

use super::*;

// ── Rotation management ───────────────────────────────────────────────────────

pub(crate) async fn cmd_cleaning(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    let subcommand = args.first().map(|value| value.to_ascii_lowercase());
    match subcommand.as_deref() {
        Some("add") => add_matrix_participant(ctx, sender, &args[1..]).await,
        Some("remove") => remove_matrix_participant(ctx, sender, &args[1..]).await,
        Some("people") => cmd_cleaning_people(ctx, &args[1..]).await,
        _ => Ok(Some(
            "Usage: !cleaning add @user:server <group> | remove @user:server <group> | people [group]"
                .to_owned(),
        )),
    }
}

pub(crate) async fn cmd_cleaning_people(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let groups: Vec<&CleaningGroup> = match args.first() {
        Some(group_name) => match state.group_by_name(group_name) {
            Some(group) => vec![group],
            None => return Ok(Some(format!("Group «{group_name}» not found."))),
        },
        None => state
            .cleaning_groups
            .iter()
            .filter(|group| group.is_active)
            .collect(),
    };

    if groups.is_empty() {
        return Ok(Some("No active cleaning groups.".to_owned()));
    }

    let mut sections = Vec::new();
    for group in groups {
        let mut lines = vec![format!("🔁 **{}**", group.name)];
        if group.member_ids.is_empty() {
            lines.push("Rotation is empty.".to_owned());
        } else {
            for (index, person_id) in group.member_ids.iter().enumerate() {
                let label = state
                    .person_by_id(person_id)
                    .map(person_label)
                    .unwrap_or_else(|| format!("unknown ({person_id})"));
                lines.push(format!("{}. {label}", index + 1));
            }
            lines.push(next_assignment_summary(&state, &group.id));
        }
        sections.push(lines.join("\n"));
    }
    Ok(Some(sections.join("\n\n")))
}

pub(crate) async fn add_matrix_participant(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (mxid, group_name) = match (args.first(), args.get(1)) {
        (Some(mxid), Some(group)) => (*mxid, *group),
        _ => return Ok(Some("Usage: !cleaning add @user:server <group>".to_owned())),
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
    apply_group_join(ctx, &mut state, &group_id, &person_id)?;
    let next = next_assignment_summary(&state, &group_id);
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "✅ Added {mxid} to «{group_name}».\n\
         Takes effect from the next open week; already-planned weeks are unchanged.\n{next}"
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
                "Usage: !cleaning remove @user:server <group>".to_owned(),
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

    let open = current_open_assignments(
        &state,
        &group_id,
        &person_id,
        ctx.config.schedule.interval_weeks,
    );
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
    let refilled = apply_group_departure(ctx, &mut state, &person_id, &group_id)?;
    let next = next_assignment_summary(&state, &group_id);
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "✅ Removed {mxid} from «{group_name}».\n\
         Current, completed, and other members' future assignments were preserved. \
         Refilled {refilled} vacated week(s).\n{next}"
    )))
}
