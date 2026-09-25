//! Maintenance: !testnotify, group enable/disable, !listgroups, plan reposting, !validate.

use super::*;

// ── !testnotify ───────────────────────────────────────────────────────────────

pub(crate) async fn cmd_testnotify(room: &Room) -> Result<Option<RoomMessageEventContent>> {
    let room = room.clone();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(5 * 60)).await;
        use matrix_sdk::ruma::events::Mentions;
        let mut msg = RoomMessageEventContent::text_html(
            "🔔 @room · test notification",
            "🔔 @room · test notification",
        );
        let mut mentions = Mentions::new();
        mentions.room = true;
        msg = msg.add_mentions(mentions);
        room.send(msg).await.ok();
    });
    Ok(Some(RoomMessageEventContent::text_plain(
        "⏱ @room notification in 5 minutes.",
    )))
}

// ── Admin: !disablegroup <group> ─────────────────────────────────────────────

pub(crate) async fn cmd_disablegroup(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !disablegroup <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&name) {
        Some(g) if !g.is_active => return Ok(Some(format!("«{name}» is already disabled."))),
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{name}» not found."))),
    };
    state.apply_event(DomainEvent::GroupDisabled { group_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "🚫 «{name}» disabled — excluded from scheduling and statistics."
    )))
}

// ── Admin: !enablegroup <group> ──────────────────────────────────────────────

pub(crate) async fn cmd_enablegroup(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !enablegroup <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&name) {
        Some(g) if g.is_active => return Ok(Some(format!("«{name}» is already enabled."))),
        Some(g) => g.id.clone(),
        None => return Ok(Some(format!("Group «{name}» not found."))),
    };
    state.apply_event(DomainEvent::GroupEnabled { group_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!(
        "✅ «{name}» enabled — now included in scheduling and statistics."
    )))
}

// ── !listgroups ───────────────────────────────────────────────────────────────

pub(crate) async fn cmd_listgroups(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    if state.cleaning_groups.is_empty() {
        return Ok(Some("No cleaning groups configured.".into()));
    }
    let active_count = state.cleaning_groups.iter().filter(|g| g.is_active).count();
    let disabled_count = state.cleaning_groups.len() - active_count;
    let mut lines = vec![format!(
        "🏢 Groups ({active_count} active, {disabled_count} disabled):"
    )];
    for group in &state.cleaning_groups {
        let n = group.member_ids.len();
        if group.is_active {
            lines.push(format!("  ✅ **{}** ({n} members)", group.name));
        } else {
            lines.push(format!("  🚫 **{}** ({n} members) — disabled", group.name));
        }
    }
    Ok(Some(lines.join("\n")))
}

// ── Admin: !announceweek / !repostplan ───────────────────────────────────────
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
            "📋 Weekly plan announced and pinned.",
        ))),
        Ok(None) => Ok(Some(format::mentionify(
            "Nothing is due this week — no plan to announce.",
        ))),
        Err(e) => {
            tracing::error!("!announceweek failed: {e}");
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
