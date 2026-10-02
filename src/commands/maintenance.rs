//! Maintenance: group enable/disable, plan announcing, !validate.

use super::*;

// ── Admin: !groups disable <group> ─────────────────────────────────────────────

pub(crate) async fn cmd_disablegroup(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !groups disable <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&name) {
        Some(g) if !g.is_active => return Ok(Some(format!("«{name}» is already disabled."))),
        Some(g) => g.id.clone(),
        None => return Ok(Some(group_not_found(&name))),
    };
    // Its rotation stands still while disabled: planned rotation weeks go
    // back to the queue, so enabling it later continues where it stopped.
    let returned = return_future_rotation_turns(&mut state, &group_id)?;
    state.apply_event(DomainEvent::GroupDisabled { group_id })?;
    state.save(&ctx.state_path).await?;
    let mut reply =
        format!("🚫 {name} disabled — no turns, reminders or stats until it's enabled again.");
    if !returned.is_empty() {
        reply.push_str(&format!(
            "\n{} planned week(s) cleared; the rotation picks up there when enabled.",
            returned.len()
        ));
    }
    Ok(Some(reply))
}

// ── Admin: !groups enable <group> ──────────────────────────────────────────────

pub(crate) async fn cmd_enablegroup(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !groups enable <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&name) {
        Some(g) if g.is_active => return Ok(Some(format!("«{name}» is already enabled."))),
        Some(g) => g.id.clone(),
        None => return Ok(Some(group_not_found(&name))),
    };
    state.apply_event(DomainEvent::GroupEnabled {
        group_id: group_id.clone(),
    })?;
    // Weeks planned while it was disabled (older versions kept rotating
    // disabled groups) are handed back first, then the plan is filled anew.
    return_future_rotation_turns(&mut state, &group_id)?;
    materialize_group_and_apply(
        &mut state,
        &group_id,
        ctx.config.schedule.materialize_weeks as usize,
    )?;
    let next = next_assignment_summary(&state, &group_id);
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ {name} enabled.\n{next}")))
}

// ── Admin: !plan announce ───────────────────────────────────────
//
// Sends a fresh consolidated weekly plan for the current week.  Replaces the
// previously active plan in state so reactions on old messages no longer
// trigger completions.  Pins the new message and unpins old plan messages.

pub(crate) async fn cmd_announceweek(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
) -> Result<Option<RoomMessageEventContent>> {
    require_admin(ctx, sender)?;
    let (year, week) = current_iso_week();
    match crate::scheduler::announce_weekly_plan(ctx, room, year, week).await {
        Ok(Some(_)) => Ok(Some(format::mentionify(
            "🧹 Weekly plan posted and pinned.",
        ))),
        Ok(None) => Ok(Some(format::mentionify(
            "Nothing is due this week — no plan to announce.",
        ))),
        Err(e) => {
            tracing::error!("!plan announce failed: {e}");
            Ok(Some(format::mentionify(&format!(
                "❌ Failed to announce weekly plan: {e}"
            ))))
        }
    }
}

// ── Admin: !validate ──────────────────────────────────────────────────────────

pub(crate) async fn cmd_validate(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let state = ctx.state.lock().await;
    let report = crate::validate::validate_state(&state);
    Ok(Some(report.summary()))
}
