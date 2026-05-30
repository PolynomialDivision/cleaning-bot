use anyhow::Result;
use chrono::Utc;
use matrix_sdk::{
    Room,
    ruma::{
        OwnedEventId, OwnedUserId, UInt,
        events::{
            relation::Thread,
            room::message::{FileInfo, FileMessageEventContent, MessageType, RoomMessageEventContent},
        },
    },
};
use uuid::Uuid;

use crate::{
    BotContext, format, resolver,
    analytics::{self, DomainEvent},
    domain::{AssignmentSource, new_calendar_token, CalendarToken, CleaningGroup, Person},
    schedule::build_schedule,
    state::{
        SwapStatus,
        add_weeks, current_iso_week, week_dates, weeks_between,
    },
};

/// Shell-like tokenizer: splits on whitespace but keeps "quoted strings" together.
/// Quotes are stripped from the resulting tokens.
/// Example: `!addroom "2. Stock" "Scharni Toilette"` → ["!addroom", "2. Stock", "Scharni Toilette"]
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' | '\'' => quoted = !quoted,
            ' ' | '\t' if !quoted => {
                if !cur.is_empty() { tokens.push(std::mem::take(&mut cur)); }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() { tokens.push(cur); }
    tokens
}

pub async fn handle(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    body: &str,
    event_id: OwnedEventId,
    thread_root: OwnedEventId,
) -> Result<Option<RoomMessageEventContent>> {
    let mut tokens = tokenize(body);
    let cmd_owned  = if tokens.is_empty() { String::new() } else { tokens.remove(0) };
    let cmd        = cmd_owned.as_str();
    let arg_strings = tokens;
    let args: Vec<&str> = arg_strings.iter().map(String::as_str).collect();

    // Update the sender's display name from Matrix on every command (lightweight).
    {
        let sender_mxid = sender.as_str().to_owned();
        let refs = vec![sender_mxid.as_str()];
        let fetched = format::fetch_names(room, &refs).await;
        if let Some(name) = fetched.get(sender_mxid.as_str()) {
            if !name.is_empty() && name != &sender_mxid {
                let mut state = ctx.state.lock().await;
                if let Some(p) = state.persons.iter_mut().find(|p| p.matrix_id.as_deref() == Some(&sender_mxid)) {
                    p.display_name = name.clone();
                }
            }
        }
    }

    // Commands that need direct room access.
    match cmd {
        "!cleanplan"  => return cmd_cleanplan(ctx, sender, room, &args).await,
        "!remind"     => return cmd_remind(ctx, sender, room, &args).await,
        "!testnotify" => return cmd_testnotify(room).await,
        "!pdf"        => return cmd_pdf(ctx, sender, room, &args, event_id, thread_root).await,
        "!ical"       => return cmd_ical(ctx, sender, room, &args).await,
        "!icalreset"  => return cmd_icalreset(ctx, sender, room, &args).await,
        _ => {}
    }

    let reply: Option<String> = match cmd {
        "!done"         => cmd_done(ctx, sender, &args).await,
        "!status"       => cmd_status(ctx).await,
        "!stats"        => cmd_stats(ctx, &args).await,
        "!groups"       => cmd_floors(ctx).await,
        "!joingroup"    => cmd_joinfloor(ctx, sender, &args).await,
        "!leavegroup"   => cmd_leavefloor(ctx, sender, &args).await,
        "!swap"         => cmd_swap(ctx, sender, &args).await,
        "!acceptswap"   => cmd_acceptswap(ctx, sender, &args).await,
        "!rejectswap"   => cmd_rejectswap(ctx, sender, &args).await,
        "!adduser"      => cmd_adduser(ctx, sender, &args).await,
        "!removeuser"   => cmd_removeuser(ctx, sender, &args).await,
        "!addperson"    => cmd_addperson(ctx, sender, &args).await,
        "!removeperson" => cmd_removeperson(ctx, sender, &args).await,
        "!addgroup"     => cmd_addfloor(ctx, sender, &args).await,
        "!removegroup"  => cmd_removefloor(ctx, sender, &args).await,
        "!resetplan"    => cmd_resetplan(ctx, sender, &args).await,
        "!addslot"      => cmd_addslot(ctx, sender, &args).await,
        "!removeslot"   => cmd_removeslot(ctx, sender, &args).await,
        "!addroom"      => cmd_addroom(ctx, sender, &args).await,
        "!removeroom"   => cmd_removeroom(ctx, sender, &args).await,
        "!undo"         => cmd_undo(ctx, sender, &args).await,
        "!next"         => cmd_next(ctx, sender, &args).await,
        "!skip"         => cmd_skip(ctx, sender, &args).await,
        "!leaderboard"  => cmd_leaderboard(ctx).await,
        "!fairness"     => cmd_fairness(ctx, &args).await,
        "!validate"     => cmd_validate(ctx, sender).await,
        "!absent"       => cmd_absent(ctx, sender, &args).await,
        "!back"         => cmd_back(ctx, sender, &args).await,
        "!blame"        => cmd_blame(ctx, &args).await,
        "!help"         => Ok(Some(help_text())),
        _               => Ok(None),
    }?;

    match reply {
        None    => Ok(None),
        Some(s) => Ok(Some(format::mentionify_rich(&s, room).await)),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn require_admin(ctx: &BotContext, sender: &OwnedUserId) -> Result<()> {
    if ctx.admin_users.contains(sender) { Ok(()) }
    else { Err(anyhow::anyhow!("__not_admin__")) }
}

/// Returns the MXID or display_name depending on whether the person has Matrix.
fn person_key(p: &Person) -> &str {
    p.matrix_id.as_deref().unwrap_or(&p.display_name)
}

/// After removing `person_id` from `group_id`, unassign any of their future
/// stored slot assignments then rematerialize to fill the vacated weeks.
///
/// When someone joins a group, clear ALL future slot assignments for that group
/// and rematerialize.  This redistributes future weeks across the full member list.
/// Without this, any assignments frozen before the new person joined stay unchanged.
fn reset_and_rematerialize(
    ctx: &BotContext,
    state: &mut crate::state::State,
    group_id: &str,
) -> anyhow::Result<()> {
    let (cur_y, cur_w) = current_iso_week();
    // Drop all future assignments for this group — they'll be rebuilt with the full list.
    state.slot_assignments.retain(|a| {
        a.group_id != group_id
            || a.iso_year < cur_y
            || (a.iso_year == cur_y && a.iso_week < cur_w)
    });
    let interval = ctx.config.schedule.interval_weeks;
    let weeks    = ctx.config.schedule.materialize_weeks as usize;
    for ev in resolver::materialize(state, interval, weeks) {
        state.apply_event(ev)?;
    }
    Ok(())
}

/// Must be called with `state` already locked, AFTER `PersonLeftGroup` is applied.
async fn unassign_and_rematerialize(
    ctx: &BotContext,
    state: &mut crate::state::State,
    person_id: &str,
    group_id: &str,
) -> anyhow::Result<()> {
    let (cur_y, cur_w) = current_iso_week();
    let future: Vec<(usize, i32, u32)> = state.slot_assignments.iter()
        .filter(|a| {
            a.group_id == group_id
                && a.person_id.as_deref() == Some(person_id)
                && (a.iso_year > cur_y || (a.iso_year == cur_y && a.iso_week >= cur_w))
        })
        .map(|a| (a.slot_index, a.iso_year, a.iso_week))
        .collect();

    for (si, y, w) in future {
        state.apply_event(DomainEvent::SlotAssigned {
            group_id:   group_id.to_owned(),
            slot_index: si,
            iso_year:   y,
            iso_week:   w,
            person_id:  None,
            source:     AssignmentSource::RoundRobin,
        })?;
    }

    let interval = ctx.config.schedule.interval_weeks;
    let weeks = ctx.config.schedule.materialize_weeks as usize;
    let evs = resolver::materialize(state, interval, weeks);
    for ev in evs { state.apply_event(ev)?; }
    Ok(())
}

/// Fetch Matrix display names for all known Matrix users and update state.
/// Keeps Person.display_name in sync with the real Matrix profile name.
/// Called before PDF generation and whenever a command comes in.
async fn refresh_display_names(ctx: &BotContext, room: &Room) {
    let mxids: Vec<String> = ctx.state.lock().await.persons.iter()
        .filter_map(|p| p.matrix_id.clone()).collect();
    if mxids.is_empty() { return; }
    let refs: Vec<&str> = mxids.iter().map(String::as_str).collect();
    let fetched = format::fetch_names(room, &refs).await;
    if fetched.is_empty() { return; }
    let mut state = ctx.state.lock().await;
    for p in &mut state.persons {
        if let Some(mxid) = &p.matrix_id {
            if let Some(name) = fetched.get(mxid.as_str()) {
                if !name.is_empty() && name != mxid {
                    p.display_name = name.clone();
                }
            }
        }
    }
}

// ── !done [group] ─────────────────────────────────────────────────────────────

async fn cmd_done(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let (year, week)  = current_iso_week();
    let sender_mxid   = sender.as_str();
    let mut state     = ctx.state.lock().await;

    // Resolve sender to a Person.
    let sender_person_id = match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
        Some(id) => id,
        None => return Ok(Some(format!(
            "You are not registered. Ask an admin to run !adduser {sender_mxid} <group>."
        ))),
    };

    // Determine target group(s).
    let target_group_ids: Vec<String> = if let Some(name) = args.first() {
        match state.group_by_name(name) {
            Some(g) => vec![g.id.clone()],
            None    => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        state.groups_for_person(&sender_person_id).iter().map(|g| g.id.clone()).collect()
    };

    if target_group_ids.is_empty() {
        return Ok(Some("You are not assigned to any cleaning group.".into()));
    }

    let interval = ctx.config.schedule.interval_weeks;
    let mut marked = vec![];
    let mut already_done = vec![];

    for group_id in &target_group_ids {
        let is_member = state.cleaning_groups.iter()
            .find(|g| &g.id == group_id)
            .map(|g| g.member_ids.contains(&sender_person_id))
            .unwrap_or(false);
        let swapped_in = state.swap_requests.iter().any(|s| {
            s.group_id == *group_id && s.iso_year == year && s.iso_week == week
                && s.status == SwapStatus::Accepted && s.target == sender_mxid
        });
        if !is_member && !swapped_in {
            let name = state.group_by_id(group_id).map(|g| g.name.as_str()).unwrap_or("?");
            return Ok(Some(format!("You are not a member of «{name}».")));
        }

        let group = state.group_by_id(group_id).unwrap().clone();

        if group.is_multi_slot() {
            // Mark the slot(s) assigned to this person.
            let slot_assignments: Vec<(String, String)> = group.slots.iter().enumerate()
                .filter_map(|(slot_idx, slot)| {
                    let assignee = state.slot_assignee(&group, slot_idx, year, week, interval)?;
                    if assignee.id == sender_person_id { Some((slot.id.clone(), slot.name.clone())) } else { None }
                })
                .collect();

            if slot_assignments.is_empty() {
                let name = group.name.clone();
                return Ok(Some(format!("You are not assigned to any slot in «{name}» this week.")));
            }

            for (slot_id, slot_name) in slot_assignments {
                if state.is_slot_completed(group_id, &slot_id, year, week) {
                    already_done.push(format!("{} / {slot_name}", group.name));
                    continue;
                }
                let responsible_ids = vec![sender_person_id.clone()];
                state.apply_event(DomainEvent::CleaningCompleted {
                    group_id:               group_id.clone(),
                    slot_id:                Some(slot_id),
                    person_id:              sender_person_id.clone(),
                    responsible_person_ids: responsible_ids,
                    iso_year: year, iso_week: week,
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
            let responsible_ids: Vec<String> = state.responsible_person(&group, year, week, interval)
                .map(|p| vec![p.id.clone()]).unwrap_or_default();
            state.apply_event(DomainEvent::CleaningCompleted {
                group_id:               group_id.clone(),
                slot_id:                None,
                person_id:              sender_person_id.clone(),
                responsible_person_ids: responsible_ids,
                iso_year: year, iso_week: week,
            })?;
            marked.push(group.name.clone());
        }
    }

    state.save(&ctx.state_path).await?;
    drop(state);

    let mut lines = vec![];
    if !marked.is_empty()       { lines.push(format!("✅ Cleaned: {}", marked.join(", "))); }
    if !already_done.is_empty() { lines.push(format!("Already done: {}", already_done.join(", "))); }
    Ok(Some(lines.join("\n")))
}

// ── !status ───────────────────────────────────────────────────────────────────

async fn cmd_status(ctx: &BotContext) -> Result<Option<String>> {
    let (year, week) = current_iso_week();
    let state    = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;

    if state.cleaning_groups.is_empty() {
        return Ok(Some("No cleaning groups configured yet.".into()));
    }

    let mut lines = vec![format!("📋 **Cleaning status** · week {week} ({})", week_dates(year, week))];
    for group in &state.cleaning_groups {
        let done = state.is_completed(&group.id, year, week);
        let icon = if done { "✅" } else { "❌" };
        let who = if done {
            state.completions.iter()
                .find(|c| c.group_id == group.id && c.iso_year == year && c.iso_week == week)
                .and_then(|c| state.person_by_id(&c.completed_by_id))
                .map(|p| format!(" · {}", p.display_name))
                .unwrap_or_default()
        } else {
            match state.responsible_person(group, year, week, interval) {
                Some(p) => format!(" · {}", person_key(p)),
                None    => " · (nobody assigned)".into(),
            }
        };
        let rooms_str = group.rooms_text().map(|r| format!("\n  {r}")).unwrap_or_default();
        lines.push(format!("{icon} **{}**{who}{rooms_str}", group.name));
    }
    Ok(Some(lines.join("\n")))
}

// ── !stats [@user] ────────────────────────────────────────────────────────────

async fn cmd_stats(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let state    = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let (start_y, start_w) = state.tracking_start();

    // Per-person view.
    if let Some(query) = args.first().copied() {
        let person_id = match state.find_person(query).map(|p| p.id.clone()) {
            Some(id) => id,
            None     => return Ok(Some(format!("Person «{query}» not found."))),
        };
        let ps = match analytics::person_stats(&state, &person_id, interval) {
            Some(s) => s,
            None    => return Ok(Some(format!("{query} is not in any cleaning group."))),
        };
        let mut completions: Vec<_> = state.completions.iter()
            .filter(|c| c.completed_by_id == person_id)
            .collect();
        completions.sort_by(|a,b| b.completed_at.cmp(&a.completed_at));
        let pct = (ps.completion_rate * 100.0).round() as u32;
        let streak_str = if ps.streak >= 2 { format!(" 🔥{}", ps.streak) } else { String::new() };
        let swaps_str  = if ps.swaps_given > 0 || ps.swaps_taken > 0 {
            format!(" · Swaps given: {} taken: {}", ps.swaps_given, ps.swaps_taken)
        } else { String::new() };
        let mut lines = vec![
            format!("📊 **Stats** · {} · since week {start_w} ({})", ps.display_name, week_dates(start_y, start_w)),
            format!("Group: {} · {}/{} ({pct}%){streak_str}", ps.group_names, ps.completed, ps.due_weeks),
            format!("Missed: {} · Skipped: {}{swaps_str}", ps.missed, ps.skipped),
        ];
        if let Some(last) = completions.first() {
            lines.push(format!("Last: week {} ({})", last.iso_week, week_dates(last.iso_year, last.iso_week)));
        }
        if completions.len() > 1 {
            lines.push("Recent:".into());
            for c in completions.iter().take(5) {
                let gname = state.group_by_id(&c.group_id).map(|g| g.name.as_str()).unwrap_or("?");
                lines.push(format!("  • {gname} · week {} ({})", c.iso_week, week_dates(c.iso_year, c.iso_week)));
            }
        }
        return Ok(Some(lines.join("\n")));
    }

    // Group summary view.
    let mut lines = vec![format!("📊 **Cleaning stats** · since week {start_w} ({})", week_dates(start_y, start_w))];
    if state.cleaning_groups.is_empty() {
        lines.push("  No cleaning groups configured yet.".into());
        return Ok(Some(lines.join("\n")));
    }

    let (cur_y, cur_w) = current_iso_week();
    for group in &state.cleaning_groups {
        let gs = match analytics::group_stats(&state, &group.id, interval) {
            Some(s) => s, None => continue,
        };
        let pct = (gs.completion_rate * 100.0).round() as u32;
        let this_week = state.is_completed(&group.id, cur_y, cur_w);
        lines.push(String::new());
        lines.push(format!("🏢 {} ({} members)", group.name, gs.member_count));
        lines.push(format!("Completed: {}/{} ({pct}%) · Missed: {}", gs.completed, gs.due_weeks, gs.missed));
        lines.push(format!("Streak: {} · This week: {}", gs.current_streak, if this_week { "✅" } else { "❌" }));
        if let Some(last) = state.last_completion(&group.id) {
            let by = state.person_by_id(&last.completed_by_id).map(|p| p.display_name.as_str()).unwrap_or("?");
            lines.push(format!("Last: week {} ({}) by {by}", last.iso_week, week_dates(last.iso_year, last.iso_week)));
        }
        // Per-member counts
        for pid in &group.member_ids {
            if let Some(p) = state.person_by_id(pid) {
                let cnt = state.completions.iter()
                    .filter(|c| c.group_id == group.id && c.completed_by_id == *pid).count();
                lines.push(format!("  {}: {cnt}", p.display_name));
            }
        }
    }
    Ok(Some(lines.join("\n")))
}

// ── !floors / !areas ─────────────────────────────────────────────────────────

async fn cmd_floors(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    if state.cleaning_groups.is_empty() {
        return Ok(Some("No cleaning groups configured.".into()));
    }
    let mut lines = vec!["🏢 Cleaning areas:".to_owned()];
    for group in &state.cleaning_groups {
        let members_text = if group.member_ids.is_empty() {
            "(no members)".to_owned()
        } else {
            group.member_ids.iter()
                .filter_map(|id| state.person_by_id(id))
                .map(|p| {
                    if p.matrix_id.is_some() { p.display_name.clone() }
                    else { format!("{} (no Matrix)", p.display_name) }
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

async fn cmd_joinfloor(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let group_name = match args.first() {
        Some(n) => n.to_string(),
        None    => return Ok(Some("Usage: !joingroup <group>".into())),
    };
    let mxid = sender.as_str();
    let mut state = ctx.state.lock().await;

    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None    => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    // PersonCreated is idempotent — safe even if this Matrix user already exists.
    let new_person_id = uuid::Uuid::new_v4().to_string();
    state.apply_event(DomainEvent::PersonCreated {
        person_id: new_person_id, display_name: mxid.to_owned(), matrix_id: Some(mxid.to_owned()),
    })?;
    let person_id = state.person_by_matrix_id(mxid).unwrap().id.clone();
    if state.group_by_id(&group_id).map(|g| g.member_ids.contains(&person_id)).unwrap_or(false) {
        return Ok(Some(format!("You are already in «{group_name}».")));
    }
    state.apply_event(DomainEvent::PersonJoinedGroup { person_id, group_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Joined «{group_name}».")))
}

// ── !leavefloor <group> ───────────────────────────────────────────────────────

async fn cmd_leavefloor(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let group_name = match args.first() {
        Some(n) => n.to_string(),
        None    => return Ok(Some("Usage: !leavegroup <group>".into())),
    };
    let mxid = sender.as_str();
    let mut state = ctx.state.lock().await;

    let person_id = match state.person_by_matrix_id(mxid).map(|p| p.id.clone()) {
        Some(id) => id,
        None     => return Ok(Some("You are not registered in any group.".into())),
    };
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None    => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    if !state.group_by_id(&group_id).map(|g| g.member_ids.contains(&person_id)).unwrap_or(false) {
        return Ok(Some(format!("You are not in «{group_name}».")));
    }
    state.apply_event(DomainEvent::PersonLeftGroup { person_id: person_id.clone(), group_id: group_id.clone() })?;
    unassign_and_rematerialize(ctx, &mut state, &person_id, &group_id).await?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Left «{group_name}».")))
}

// ── Admin: !adduser @mxid <group> ────────────────────────────────────────────

async fn cmd_adduser(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (mxid, group_name) = match (args.first(), args.get(1)) {
        (Some(u), Some(f)) => (*u, f.to_string()),
        _ => return Ok(Some("Usage: !adduser @user <group>".into())),
    };

    let mut state = ctx.state.lock().await;

    // Ensure group exists before checking membership.
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None    => return Ok(Some(format!("Group «{group_name}» not found."))),
    };

    // PersonCreated is idempotent — safe to call even if person exists.
    let new_person_id = uuid::Uuid::new_v4().to_string();
    state.apply_event(DomainEvent::PersonCreated {
        person_id: new_person_id, display_name: mxid.to_owned(), matrix_id: Some(mxid.to_owned()),
    })?;
    let person_id = state.person_by_matrix_id(mxid).unwrap().id.clone();

    if state.group_by_id(&group_id).map(|g| g.member_ids.contains(&person_id)).unwrap_or(false) {
        return Ok(Some(format!("{mxid} is already in «{group_name}».")));
    }
    state.apply_event(DomainEvent::PersonJoinedGroup { person_id, group_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Added {mxid} to «{group_name}».")))
}

// ── Admin: !removeuser @mxid <group> ─────────────────────────────────────────

async fn cmd_removeuser(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (query, group_name) = match (args.first(), args.get(1)) {
        (Some(u), Some(f)) => (u.to_string(), f.to_string()),
        _ => return Ok(Some("Usage: !removeuser @user <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let person_id = match state.find_person(&query).map(|p| p.id.clone()) {
        Some(id) => id,
        None     => return Ok(Some(format!("Person «{query}» not found."))),
    };
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None    => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    if !state.group_by_id(&group_id).map(|g| g.member_ids.contains(&person_id)).unwrap_or(false) {
        return Ok(Some(format!("{query} is not in «{group_name}».")));
    }
    state.apply_event(DomainEvent::PersonLeftGroup { person_id: person_id.clone(), group_id: group_id.clone() })?;
    unassign_and_rematerialize(ctx, &mut state, &person_id, &group_id).await?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Removed {query} from «{group_name}».")))
}

// ── Admin: !addperson <name> <group> ─────────────────────────────────────────

async fn cmd_addperson(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (name, group_name) = match (args.first(), args.get(1)) {
        (Some(n), Some(f)) => (n.to_string(), f.to_string()),
        _ => return Ok(Some("Usage: !addperson <display_name> <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None    => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    let new_person_id = uuid::Uuid::new_v4().to_string();
    state.apply_event(DomainEvent::PersonCreated {
        person_id: new_person_id, display_name: name.clone(), matrix_id: None,
    })?;
    let person_id = state.find_person(&name).unwrap().id.clone();
    if state.group_by_id(&group_id).map(|g| g.member_ids.contains(&person_id)).unwrap_or(false) {
        return Ok(Some(format!("{name} is already in «{group_name}».")));
    }
    state.apply_event(DomainEvent::PersonJoinedGroup { person_id, group_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Added {name} (no Matrix) to «{group_name}».")))
}

// ── Admin: !removeperson <name> <group> ──────────────────────────────────────

async fn cmd_removeperson(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (query, group_name) = match (args.first(), args.get(1)) {
        (Some(n), Some(f)) => (n.to_string(), f.to_string()),
        _ => return Ok(Some("Usage: !removeperson <name> <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let person_id = match state.find_person(&query).map(|p| p.id.clone()) {
        Some(id) => id,
        None     => return Ok(Some(format!("Person «{query}» not found."))),
    };
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None    => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    if !state.group_by_id(&group_id).map(|g| g.member_ids.contains(&person_id)).unwrap_or(false) {
        return Ok(Some(format!("{query} is not in «{group_name}».")));
    }
    state.apply_event(DomainEvent::PersonLeftGroup { person_id: person_id.clone(), group_id: group_id.clone() })?;
    unassign_and_rematerialize(ctx, &mut state, &person_id, &group_id).await?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Removed {query} from «{group_name}».")))
}

// ── Admin: !addfloor <name> ───────────────────────────────────────────────────

async fn cmd_addfloor(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None    => return Ok(Some("Usage: !addgroup <name>".into())),
    };
    let mut state = ctx.state.lock().await;
    if state.group_by_name(&name).is_some() {
        return Ok(Some(format!("Group «{name}» already exists.")));
    }
    let group_id = uuid::Uuid::new_v4().to_string();
    state.apply_event(DomainEvent::GroupCreated { group_id, name: name.clone() })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Created cleaning group «{name}».")))
}

// ── Admin: !removefloor <name> ────────────────────────────────────────────────

async fn cmd_removefloor(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None    => return Ok(Some("Usage: !removegroup <name>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&name) {
        Some(g) => g.id.clone(),
        None    => return Ok(Some(format!("Group «{name}» not found."))),
    };
    state.apply_event(DomainEvent::GroupDeleted { group_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Removed group «{name}».")))
}

// ── Admin: !addslot <group> <slot_name> ──────────────────────────────────────

// ── Admin: !resetplan <group> ─────────────────────────────────────────────────
// Clears all future (>= today) slot assignments for a group and rematerializes.
// Use this after the initial setup when you've added all members and want the
// rotation to distribute fairly from now on.  Safe to run at any time — past
// completed weeks are never touched.

async fn cmd_resetplan(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let group_name = match args.first() {
        Some(n) => n.to_string(),
        None    => return Ok(Some("Usage: !resetplan <group>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None    => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    reset_and_rematerialize(ctx, &mut state, &group_id)?;
    let assignee = {
        let interval = ctx.config.schedule.interval_weeks;
        let (cur_y, cur_w) = current_iso_week();
        let g = state.group_by_id(&group_id).unwrap().clone();
        state.responsible_person(&g, cur_y, cur_w, interval)
            .map(|p| p.display_name.clone())
            .unwrap_or_else(|| "(nobody)".into())
    };
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Plan reset for «{group_name}». This week: {assignee}.")))
}

async fn cmd_addslot(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (group_name, slot_name) = match (args.first(), args.get(1..).map(|s| s.join(" "))) {
        (Some(g), Some(s)) if !s.is_empty() => (g.to_string(), s),
        _ => return Ok(Some("Usage: !addslot <group> <slot_name>".into())),
    };
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None    => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    if state.group_by_id(&group_id).and_then(|g| g.slot_by_name(&slot_name)).is_some() {
        return Ok(Some(format!("Slot «{slot_name}» already exists in «{group_name}».")));
    }
    let slot_id = uuid::Uuid::new_v4().to_string();
    state.apply_event(DomainEvent::SlotAdded { group_id, slot_id, slot_name: slot_name.clone() })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Added slot «{slot_name}» to «{group_name}».")))
}

// ── Admin: !removeslot <group> <slot_name> ───────────────────────────────────

async fn cmd_removeslot(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (group_name, slot_name) = match (args.first(), args.get(1..).map(|s| s.join(" "))) {
        (Some(g), Some(s)) if !s.is_empty() => (g.to_string(), s),
        _ => return Ok(Some("Usage: !removeslot <group> <slot_name>".into())),
    };
    let mut state = ctx.state.lock().await;
    let (group_id, slot_id) = match state.group_by_name(&group_name) {
        Some(g) => match g.slot_by_name(&slot_name) {
            Some(s) => (g.id.clone(), s.id.clone()),
            None    => return Ok(Some(format!("Slot «{slot_name}» not found in «{group_name}»."))),
        },
        None => return Ok(Some(format!("Group «{group_name}» not found."))),
    };
    state.apply_event(DomainEvent::SlotRemoved { group_id, slot_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Removed slot «{slot_name}» from «{group_name}».")))
}

// ── Admin: !addroom <group> [<slot>] <room> ──────────────────────────────────
//
// If the group has slots and the second argument matches a slot name, the room
// is added to that slot.  Otherwise the room is added to the group directly
// (single-slot mode, or a group-level room for backwards compatibility).

async fn cmd_addroom(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    if args.len() < 2 {
        return Ok(Some("Usage: !addroom <group> [<slot>] <room name>".into()));
    }
    let group_name = args[0].to_string();
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None    => return Ok(Some(format!("Group «{group_name}» not found."))),
    };

    // Detect slot targeting: if args[1] matches a slot name and there are more args, route to slot.
    let (slot_id, room_name) = {
        let g = state.group_by_id(&group_id).unwrap();
        if args.len() >= 3 {
            if let Some(slot) = g.slot_by_name(args[1]) {
                (Some(slot.id.clone()), args[2..].join(" "))
            } else {
                (None, args[1..].join(" "))
            }
        } else if g.is_multi_slot() {
            return Ok(Some(format!(
                "«{group_name}» has slots. Usage: !addroom \"{group_name}\" <slot_name> <room>.\nSlots: {}",
                g.slots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
            )));
        } else {
            (None, args[1..].join(" "))
        }
    };

    state.apply_event(DomainEvent::RoomAdded { group_id, slot_id, room_name: room_name.clone() })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Added room «{room_name}».")))
}

// ── Admin: !removeroom <group> [<slot>] <room> ───────────────────────────────

async fn cmd_removeroom(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    if args.len() < 2 {
        return Ok(Some("Usage: !removeroom <group> [<slot>] <room name>".into()));
    }
    let group_name = args[0].to_string();
    let mut state = ctx.state.lock().await;
    let group_id = match state.group_by_name(&group_name) {
        Some(g) => g.id.clone(),
        None    => return Ok(Some(format!("Group «{group_name}» not found."))),
    };

    let (slot_id, room_name) = {
        let g = state.group_by_id(&group_id).unwrap();
        if args.len() >= 3 {
            if let Some(slot) = g.slot_by_name(args[1]) {
                (Some(slot.id.clone()), args[2..].join(" "))
            } else {
                (None, args[1..].join(" "))
            }
        } else {
            (None, args[1..].join(" "))
        }
    };

    state.apply_event(DomainEvent::RoomRemoved { group_id, slot_id, room_name: room_name.clone() })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Removed room «{room_name}» from «{group_name}».")))
}

// ── !swap @target [group] [week N] ───────────────────────────────────────────

async fn cmd_swap(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let target_mxid = match args.first() {
        Some(t) => *t,
        None    => return Ok(Some("Usage: !swap @user [group] [week <N>]".into())),
    };
    if !target_mxid.starts_with('@') {
        return Ok(Some("Swap targets must be Matrix users (@user:server).".into()));
    }
    let sender_mxid  = sender.as_str();
    let (cur_y, cur_w) = current_iso_week();

    let remaining = &args[1..];
    let (group_args, (year, week)) = if let Some(pos) = remaining.iter().position(|a| a.eq_ignore_ascii_case("week")) {
        let n: u32 = match remaining.get(pos+1).and_then(|s| s.parse().ok()) {
            Some(n) if (1..=53).contains(&n) => n,
            _ => return Ok(Some("Usage: !swap @user [group] [week <1-53>]".into())),
        };
        let y = if n < cur_w { cur_y + 1 } else { cur_y };
        (&remaining[..pos], (y, n))
    } else {
        (remaining, (cur_y, cur_w))
    };

    if (year, week) < (cur_y, cur_w) {
        return Ok(Some(format!("Week {week} ({}) is in the past.", week_dates(year, week))));
    }
    if sender_mxid == target_mxid {
        return Ok(Some("You cannot swap with yourself.".into()));
    }

    let mut state = ctx.state.lock().await;

    let sender_person_id = match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
        Some(id) => id,
        None     => return Ok(Some(format!("You ({sender_mxid}) are not registered."))),
    };

    let group_id = if let Some(name) = group_args.first() {
        match state.group_by_name(name) {
            Some(g) => g.id.clone(),
            None    => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        match state.groups_for_person(&sender_person_id).first().map(|g| g.id.clone()) {
            Some(id) => id,
            None     => return Ok(Some("You are not in any group. Specify: !swap @user <group>".into())),
        }
    };

    let group = match state.group_by_id(&group_id) {
        Some(g) => g.clone(),
        None    => return Ok(Some("Group not found.".into())),
    };

    if !group.member_ids.contains(&sender_person_id) {
        return Ok(Some(format!("You are not a member of «{}».", group.name)));
    }

    let dupe = state.swap_requests.iter().any(|s| {
        s.group_id == group_id && s.iso_year == year && s.iso_week == week
            && s.status == SwapStatus::Pending && s.requester == sender_mxid
    });
    if dupe {
        return Ok(Some(format!("You already have a pending swap for «{}» week {week}.", group.name)));
    }

    state.apply_event(DomainEvent::SwapRequested {
        group_id:       group_id.clone(),
        requester_mxid: sender_mxid.to_owned(),
        target_mxid:    target_mxid.to_owned(),
        iso_year:       year,
        iso_week:       week,
    })?;
    // The swap ID was allocated inside apply_event.
    let id = state.swap_requests.last().map(|s| s.id).unwrap_or(0);
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "🔄 Swap #{id} · «{}» week {week} ({})\n{target_mxid}: !acceptswap {id} or !rejectswap {id}",
        group.name, week_dates(year, week)
    )))
}

// ── !acceptswap <id> ──────────────────────────────────────────────────────────

async fn cmd_acceptswap(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let id: u64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None    => return Ok(Some("Usage: !acceptswap <id>".into())),
    };
    let sender_mxid = sender.as_str();
    let mut state   = ctx.state.lock().await;

    let req = match state.swap_requests.iter().find(|r| r.id == id) {
        Some(r) => r,
        None    => return Ok(Some(format!("Swap request #{id} not found."))),
    };
    if req.target != sender_mxid { return Ok(Some("This swap is not addressed to you.".into())); }
    if req.status != SwapStatus::Pending { return Ok(Some(format!("Request #{id} is already {:?}.", req.status))); }
    let requester  = req.requester.clone();
    let group_id   = req.group_id.clone();
    let iso_year   = req.iso_year;
    let iso_week   = req.iso_week;

    let requester_id   = state.person_by_matrix_id(&requester).map(|p| p.id.clone())
        .unwrap_or_else(|| requester.clone());
    let replacement_id = state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone())
        .unwrap_or_else(|| sender_mxid.to_owned());

    state.apply_event(DomainEvent::SwapApproved {
        swap_id:        id,
        group_id:       group_id.clone(),
        requester_id,
        replacement_id,
        iso_year,
        iso_week,
    })?;
    state.save(&ctx.state_path).await?;

    let group_name = state.group_by_id(&group_id).map(|g| g.name.clone()).unwrap_or_default();
    Ok(Some(format!("✅ Swap #{id} accepted. {sender_mxid} will clean «{group_name}» instead of {requester}.")))
}

// ── !rejectswap <id> ─────────────────────────────────────────────────────────

async fn cmd_rejectswap(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let id: u64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None    => return Ok(Some("Usage: !rejectswap <id>".into())),
    };
    let sender_mxid = sender.as_str();
    let mut state   = ctx.state.lock().await;

    let req = match state.swap_requests.iter().find(|r| r.id == id) {
        Some(r) => r,
        None    => return Ok(Some(format!("Swap #{id} not found."))),
    };
    if req.target != sender_mxid { return Ok(Some("This swap is not addressed to you.".into())); }
    if req.status != SwapStatus::Pending { return Ok(Some(format!("Request #{id} is already {:?}.", req.status))); }
    let _ = req;

    state.apply_event(DomainEvent::SwapRejected { swap_id: id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("❌ Swap #{id} rejected.")))
}

// ── !undo [group] ─────────────────────────────────────────────────────────────

async fn cmd_undo(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let (year, week) = current_iso_week();
    let sender_mxid  = sender.as_str();
    let is_admin     = ctx.admin_users.contains(sender);
    let mut state    = ctx.state.lock().await;

    let sender_pid = state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone());

    let target_group_ids: Vec<String> = if let Some(name) = args.first() {
        match state.group_by_name(name) {
            Some(g) => vec![g.id.clone()],
            None    => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        match &sender_pid {
            Some(pid) => state.groups_for_person(pid).iter().map(|g| g.id.clone()).collect(),
            None      => return Ok(Some("You are not assigned to any group.".into())),
        }
    };

    if target_group_ids.is_empty() {
        return Ok(Some("You are not assigned to any group.".into()));
    }

    let mut undone   = vec![];
    let mut not_done = vec![];
    let mut no_perm  = vec![];

    for group_id in &target_group_ids {
        let is_member = sender_pid.as_ref().map(|pid| {
            state.cleaning_groups.iter()
                .find(|g| &g.id == group_id)
                .map(|g| g.member_ids.contains(pid))
                .unwrap_or(false)
        }).unwrap_or(false);

        if !is_member && !is_admin {
            let name = state.group_by_id(group_id).map(|g| g.name.clone()).unwrap_or_default();
            no_perm.push(name);
            continue;
        }

        let name = state.group_by_id(group_id).map(|g| g.name.clone()).unwrap_or_default();
        let had_completion = state.is_completed(group_id, year, week);
        state.apply_event(DomainEvent::CleaningUndone {
            group_id: group_id.clone(), iso_year: year, iso_week: week,
        })?;
        if had_completion {
            // Infrastructure cleanup: remove matching reaction_done trackers.
            state.reaction_dones.retain(|_, rd| {
                !(rd.group_id == *group_id && rd.iso_year == year && rd.iso_week == week)
            });
            undone.push(name);
        } else {
            not_done.push(name);
        }
    }

    state.save(&ctx.state_path).await?;
    let mut lines = vec![];
    if !undone.is_empty()   { lines.push(format!("↩️ Undone: {}", undone.join(", "))); }
    if !not_done.is_empty() { lines.push(format!("Not done this week: {}", not_done.join(", "))); }
    if !no_perm.is_empty()  { lines.push(format!("❌ Not your group: {}", no_perm.join(", "))); }
    Ok(Some(lines.join("\n")))
}

// ── !next [@user] ─────────────────────────────────────────────────────────────

async fn cmd_next(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let state    = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let (cur_y, cur_w)     = current_iso_week();
    let (start_y, start_w) = state.tracking_start();
    let iv = interval as i64;

    let query = args.first().copied().unwrap_or_else(|| sender.as_str());
    let person = match state.find_person(query) {
        Some(p) => p.clone(),
        None    => return Ok(Some(format!("{query} is not registered."))),
    };
    let groups = state.groups_for_person(&person.id);
    if groups.is_empty() {
        return Ok(Some(format!("{} is not in any cleaning group.", person.display_name)));
    }

    let elapsed = weeks_between((start_y, start_w), (cur_y, cur_w));
    let first_due = if elapsed < 0 { (start_y, start_w) }
        else {
            let past = elapsed / iv;
            if elapsed % iv == 0 { (cur_y, cur_w) } else { add_weeks(start_y, start_w, (past+1)*iv) }
        };

    let (dy, dw) = first_due;
    let away = weeks_between((cur_y, cur_w), (dy, dw));
    let when = match away { 0 => "this week ⚠️".into(), 1 => "next week".into(), n => format!("in {n} weeks") };
    let group_names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();
    let done = groups.iter().any(|g| state.is_cleaned(&g.id, dy, dw));
    let suffix = if done { "  ✅ already done!" } else { "" };

    Ok(Some(format!(
        "📅 Next due for {name}: **Week {dw} ({dates})** ({when}) · {groups}{suffix}",
        name   = person.display_name,
        dates  = week_dates(dy, dw),
        groups = group_names.join(", "),
    )))
}

// ── Admin: !skip [group] ─────────────────────────────────────────────────────

async fn cmd_skip(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (year, week) = current_iso_week();
    let interval     = ctx.config.schedule.interval_weeks;
    let mut state    = ctx.state.lock().await;

    let target_ids: Vec<String> = if let Some(name) = args.first() {
        match state.group_by_name(name) {
            Some(g) => vec![g.id.clone()],
            None    => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        state.cleaning_groups.iter()
            .filter(|g| state.is_due(&g.id, year, week, interval))
            .map(|g| g.id.clone())
            .collect()
    };

    let mut skipped = vec![];
    let mut already = vec![];
    let sender_mxid = sender.as_str();
    let sender_pid  = state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone())
        .unwrap_or_else(|| sender_mxid.to_owned());

    for group_id in &target_ids {
        let name = state.group_by_id(group_id).map(|g| g.name.clone()).unwrap_or_default();
        if state.is_completed(group_id, year, week) {
            already.push(name);
            continue;
        }
        state.apply_event(DomainEvent::CleaningSkipped {
            group_id:  group_id.clone(),
            skipper_id: sender_pid.clone(),
            iso_year:  year,
            iso_week:  week,
        })?;
        skipped.push(name);
    }

    state.save(&ctx.state_path).await?;
    let mut lines = vec![];
    if !skipped.is_empty() { lines.push(format!("⏭️ Skipped: {}", skipped.join(", "))); }
    if !already.is_empty() { lines.push(format!("Already done: {}", already.join(", "))); }
    Ok(Some(lines.join("\n")))
}

// ── Admin: !remind [group] ────────────────────────────────────────────────────

async fn cmd_remind(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    require_admin(ctx, sender)?;

    let (year, week) = current_iso_week();
    let interval     = ctx.config.schedule.interval_weeks;

    let reminder_data: Vec<(String, String, Option<String>, Vec<String>)> = {
        let state = ctx.state.lock().await;
        let groups = if let Some(name) = args.first() {
            state.cleaning_groups.iter()
                .filter(|g| g.name.eq_ignore_ascii_case(name))
                .cloned().collect::<Vec<_>>()
        } else {
            state.cleaning_groups.iter()
                .filter(|g| state.is_due(&g.id, year, week, interval) && !state.is_completed(&g.id, year, week))
                .cloned().collect()
        };

        groups.iter().map(|g| {
            let resp = state.responsible_person(g, year, week, interval);
            let mxids = resp.and_then(|p| p.matrix_id.as_ref().map(|m| vec![m.clone()])).unwrap_or_default();
            let _text = resp.map(|p| person_key(p).to_owned()).unwrap_or_else(|| "(nobody assigned)".into());
            (g.id.clone(), g.name.clone(), g.rooms_text(), mxids)
        }).collect()
    };

    if reminder_data.is_empty() {
        return Ok(Some(format::mentionify("✅ Nothing due and uncleaned right now.")));
    }

    let mut sent = vec![];
    for (group_id, group_name, rooms_text, mxids) in &reminder_data {
        let users_text = if mxids.is_empty() { "(nobody assigned)".into() } else { mxids.join(", ") };
        let rooms_line = rooms_text.as_ref().map(|r| format!("\n{r}")).unwrap_or_default();
        let msg = format!(
            "🧹 **{group_name}** · week {week} ({})\n{users_text}{rooms_line}\n✅ React or type !done",
            week_dates(year, week)
        );

        let uid_refs: Vec<&str> = mxids.iter().map(String::as_str).collect();
        let names   = format::fetch_names(room, &uid_refs).await;
        let parsed: Vec<matrix_sdk::ruma::OwnedUserId> = mxids.iter().filter_map(|s| s.parse().ok()).collect();
        let content = format::mentionify_with_names(&msg, &names)
            .add_mentions(matrix_sdk::ruma::events::Mentions::with_user_ids(parsed));

        match room.send(content).await {
            Ok(resp) => {
                let mut state = ctx.state.lock().await;
                state.reminder_event_ids.insert(resp.response.event_id.to_string(), group_id.clone());
                state.save(&ctx.state_path).await?;
                sent.push(group_name.clone());
            }
            Err(e) => tracing::warn!("!remind send failed for {group_name}: {e}"),
        }
    }

    Ok(Some(format::mentionify(&format!("✅ Reminder sent for: {}", sent.join(", ")))))
}

// ── !leaderboard ─────────────────────────────────────────────────────────────

async fn cmd_leaderboard(ctx: &BotContext) -> Result<Option<String>> {
    let state    = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;

    let board = analytics::global_leaderboard(&state, interval);
    if board.is_empty() {
        return Ok(Some("No members assigned to any group yet.".into()));
    }

    let mut lines = vec!["🏆 Cleaning Leaderboard".to_owned(), String::new()];
    for (i, ps) in board.iter().enumerate() {
        let medal   = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " };
        let streak  = if ps.streak >= 2 { format!("  🔥{}", ps.streak) } else { String::new() };
        let skips   = if ps.skipped > 0 { format!("  ⏭️{}", ps.skipped) } else { String::new() };
        let pct     = (ps.completion_rate * 100.0).round() as u32;
        lines.push(format!("{medal} {}: {}/{} ({pct:>3}%){streak}{skips}",
            ps.display_name, ps.completed, ps.due_weeks));
    }
    Ok(Some(lines.join("\n")))
}

// ── !fairness [group] ─────────────────────────────────────────────────────────

async fn cmd_fairness(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let state    = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let (start_y, start_w) = state.tracking_start();

    // Collect target groups (named group or all groups).
    let groups: Vec<crate::domain::GroupId> = if let Some(name) = args.first() {
        match state.group_by_name(name) {
            Some(g) => vec![g.id.clone()],
            None    => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        state.cleaning_groups.iter().map(|g| g.id.clone()).collect()
    };

    if groups.is_empty() {
        return Ok(Some("No cleaning groups configured yet.".into()));
    }

    let mut out = Vec::new();
    for group_id in &groups {
        let Some(report) = analytics::fairness_report(&state, group_id, interval) else { continue };
        out.push(format!(
            "⚖️ **Fairness · {}** · {} due weeks · since week {start_w} ({})",
            report.group_name, report.due_weeks, week_dates(start_y, start_w)
        ));
        out.push(String::new());
        out.push(format!("Expected per person: {:.1} cleanings", report.entries.first().map(|e| e.expected).unwrap_or(0.0)));
        out.push(String::new());

        for e in &report.entries {
            let icon = if e.deviation > 0.5 { "🟢" }
                       else if e.deviation < -0.5 { "🔴" }
                       else { "🟡" };
            let sign = if e.deviation >= 0.0 { "+" } else { "" };
            out.push(format!("  {icon} {}  done: {}  ({sign}{:.1})", e.display_name, e.actual, e.deviation));
        }

        out.push(String::new());
        out.push(format!("Fairness score: {}/100  (Gini: {:.2})", report.fairness_score, report.gini));
        out.push(String::new());
    }

    if out.is_empty() {
        return Ok(Some("No history yet — run some cleaning cycles first.".into()));
    }
    out.pop(); // remove trailing blank line
    Ok(Some(out.join("\n")))
}

// ── Admin: !absent <person> [group] [weeks] ───────────────────────────────────

async fn cmd_absent(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let person_query = match args.first() {
        Some(u) => u.to_string(),
        None    => return Ok(Some("Usage: !absent <person> [weeks]  (default 4)".into())),
    };

    let mut state = ctx.state.lock().await;
    let person = match state.find_person(&person_query).cloned() {
        Some(p) => p,
        None    => return Ok(Some(format!("{person_query} not found."))),
    };

    // Parse optional weeks (last numeric arg).
    let weeks: u32 = args.iter().rev().find_map(|s| s.parse().ok()).unwrap_or(4);

    let groups = state.groups_for_person(&person.id).iter().map(|g| g.id.clone()).collect::<Vec<_>>();
    if groups.is_empty() {
        return Ok(Some(format!("{} is not in any group.", person.display_name)));
    }

    let (from_y, from_w) = current_iso_week();
    for group_id in &groups {
        state.apply_event(DomainEvent::AbsenceRecorded {
            person_id:      person.id.clone(),
            group_id:       group_id.clone(),
            from_year:      from_y,
            from_week:      from_w,
            duration_weeks: weeks,
        })?;
    }
    state.save(&ctx.state_path).await?;

    let end = add_weeks(from_y, from_w, weeks as i64);
    Ok(Some(format!(
        "🌴 {} away for {weeks} week{} · back week {} ({})",
        person.display_name,
        if weeks == 1 { "" } else { "s" },
        end.1, week_dates(end.0, end.1)
    )))
}

// ── Admin: !back <person> ─────────────────────────────────────────────────────

async fn cmd_back(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let query = match args.first() {
        Some(u) => u.to_string(),
        None    => return Ok(Some("Usage: !back <person>".into())),
    };
    let mut state = ctx.state.lock().await;
    let person_id = match state.find_person(&query).map(|p| p.id.clone()) {
        Some(id) => id,
        None     => return Ok(Some(format!("{query} not found."))),
    };
    if !state.absences.iter().any(|a| a.person_id == person_id) {
        return Ok(Some(format!("{query} has no active absence.")));
    }
    state.apply_event(DomainEvent::AbsenceCancelled { person_id })?;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ {query} is back.")))
}

// ── !blame [group / @user] ────────────────────────────────────────────────────

async fn cmd_blame(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let (cur_y, cur_w) = current_iso_week();
    let state    = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;

    Ok(Some(match args.first() {
        None      => blame_all(&state, cur_y, cur_w, interval),
        Some(arg) => {
            if let Some(group) = state.group_by_name(arg) {
                blame_group(&state, &group.clone(), cur_y, cur_w, interval)
            } else if let Some(person) = state.find_person(arg).cloned() {
                blame_person(&state, &person, interval)
            } else {
                format!("«{arg}» not found.")
            }
        }
    }))
}

fn blame_all(state: &crate::state::State, year: i32, week: u32, interval: u32) -> String {
    let uncleaned: Vec<_> = state.cleaning_groups.iter()
        .filter(|g| state.is_due(&g.id, year, week, interval) && !state.is_completed(&g.id, year, week))
        .collect();
    if uncleaned.is_empty() {
        return "✅ All due groups are cleaned this week!".into();
    }
    let mut lines = vec![format!("😤 **Blame** · week {week} ({})", week_dates(year, week))];
    for g in uncleaned {
        let members_text = state.members_of(g).iter().map(|p| p.display_name.as_str()).collect::<Vec<_>>().join(", ");
        let n_due    = state.all_due_weeks(interval, (year, week)).len();
        let n_missed = state.missed_weeks_for(&g.id, interval).len();
        lines.push(String::new());
        lines.push(format!("❌ {}", g.name));
        lines.push(format!("Members: {members_text}"));
        lines.push(format!("Missed: {n_missed} of {n_due}"));
    }
    lines.join("\n")
}

fn blame_group(state: &crate::state::State, group: &CleaningGroup, year: i32, week: u32, interval: u32) -> String {
    let due    = state.all_due_weeks(interval, (year, week));
    let closed: Vec<_> = due.iter().filter(|&&(y,w)| (y,w) != (year,week)).collect();
    let n_due  = closed.len();
    let n_done = closed.iter().filter(|(y,w)| state.is_completed(&group.id, *y, *w)).count();
    let pct    = if n_due > 0 { 100 * n_done / n_due } else { 100 };
    let streak = state.streak_for(&group.id, interval);
    let this   = state.is_completed(&group.id, year, week);
    let members_text = state.members_of(group).iter().map(|p| p.display_name.as_str()).collect::<Vec<_>>().join(", ");

    let mut lines = vec![format!("😤 **Blame** · {}", group.name), String::new()];
    lines.push(format!("Members: {members_text}"));
    lines.push(format!("Completed: {n_done}/{n_due} ({pct}%) · Streak: {streak} · This week: {}", if this { "✅" } else { "❌" }));
    if let Some(last) = state.last_completion(&group.id) {
        let by = state.person_by_id(&last.completed_by_id).map(|p| p.display_name.as_str()).unwrap_or("?");
        lines.push(format!("Last: week {} ({}) by {by}", last.iso_week, week_dates(last.iso_year, last.iso_week)));
    }
    let missed = state.missed_weeks_for(&group.id, interval);
    if !missed.is_empty() {
        let shown: Vec<_> = missed.iter().take(5).map(|(y,w)| format!("w{w} ({})", week_dates(*y,*w))).collect();
        lines.push(format!("Missed: {}{}", shown.join(", "), if missed.len() > 5 { format!(" (+{})", missed.len()-5) } else { String::new() }));
    }
    lines.join("\n")
}

fn blame_person(state: &crate::state::State, person: &Person, interval: u32) -> String {
    let (cur_y, cur_w) = current_iso_week();
    let groups = state.groups_for_person(&person.id);
    let group_name = groups.first().map(|g| g.name.as_str()).unwrap_or("(unassigned)");

    let due    = state.all_due_weeks(interval, (cur_y, cur_w));
    let closed: Vec<_> = due.iter().filter(|&&(y,w)| (y,w) != (cur_y,cur_w)).collect();
    let n_due  = closed.len();
    let n_done_by_person = state.completions.iter().filter(|c| c.completed_by_id == person.id).count().min(n_due);
    let pct    = if n_due > 0 { 100 * n_done_by_person / n_due } else { 100 };
    let streak = groups.first().map(|g| state.streak_for(&g.id, interval)).unwrap_or(0);

    let mut lines = vec![format!("😤 **Blame** · {}", person.display_name), String::new()];
    lines.push(format!("Group: {group_name}"));
    lines.push(format!("Personally cleaned: {n_done_by_person}/{n_due} ({pct}%) · Streak: {streak}"));
    if let Some(last) = state.completions.iter()
        .filter(|c| c.completed_by_id == person.id)
        .max_by_key(|c| c.completed_at)
    {
        lines.push(format!("Last: week {} ({})", last.iso_week, week_dates(last.iso_year, last.iso_week)));
    }
    lines.join("\n")
}

// ── !cleanplan [N] ────────────────────────────────────────────────────────────

async fn cmd_cleanplan(
    ctx: &BotContext,
    _sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(6).clamp(1, 20);

    let (snapshot, is_empty) = {
        let state    = ctx.state.lock().await;
        let interval = ctx.config.schedule.interval_weeks;
        let empty    = state.cleaning_groups.is_empty();
        (build_schedule(&state, interval, n), empty)
    };

    if is_empty {
        return Ok(Some(format::mentionify("No cleaning groups configured yet.")));
    }

    // Pre-fetch Matrix display names for all assignees and completers.
    let all_mxids: Vec<String> = {
        let mut ids = Vec::new();
        for a in &snapshot.assignments {
            if let Some(mxid) = a.assignee_mxid() {
                if !ids.contains(&mxid.to_owned()) { ids.push(mxid.to_owned()); }
            }
            if let Some(by) = &a.completed_by {
                // completed_by is a display_name, look it up
                if !ids.contains(by) {
                    let state = ctx.state.lock().await;
                    if let Some(p) = state.find_person(by) {
                        if let Some(m) = &p.matrix_id { if !ids.contains(m) { ids.push(m.clone()); } }
                    }
                }
            }
        }
        ids
    };
    let uid_refs: Vec<&str> = all_mxids.iter().map(String::as_str).collect();
    let names = format::fetch_names(room, &uid_refs).await;

    let interval = snapshot.interval_weeks;
    let (cur_y, cur_w) = current_iso_week();

    let mut lines = vec![format!(
        "📅 **Cleaning plan** · next {n} week{} · every {interval} week{}",
        if n == 1 { "" } else { "s" }, if interval == 1 { "" } else { "s" }
    )];

    for (dy, dw) in snapshot.weeks() {
        let is_cur = (dy, dw) == (cur_y, cur_w);
        lines.push(String::new());
        if is_cur {
            lines.push(format!("📆 **Week {dw} ({})** ← this week", week_dates(dy, dw)));
        } else {
            lines.push(format!("📆 Week {dw} ({})", week_dates(dy, dw)));
        }
        for a in snapshot.for_group_in_week(dy, dw) {
            let icon   = if a.is_completed { "✅" } else if is_cur { "🔲" } else { "🗓" };
            let detail = if a.is_completed {
                if a.is_skipped {
                    "skipped ⏭️".into()
                } else {
                    let by = a.completed_by.as_deref().unwrap_or("?");
                    format!("done by {by}")
                }
            } else {
                match &a.assignee {
                    None    => "nobody assigned yet".into(),
                    Some(p) => {
                        let key = p.mxid.as_deref().unwrap_or(&p.name);
                        let state = ctx.state.lock().await;
                        let away = state.is_absent(&p.id, &a.group_id, dy, dw);
                        drop(state);
                        if away { format!("{key} (away)") } else { key.to_owned() }
                    }
                }
            };
            lines.push(format!("  {icon} {} : {detail}", a.group_name));
        }
    }

    Ok(Some(format::mentionify_with_names(&lines.join("\n"), &names)))
}

// ── Admin: !pdf [N] ───────────────────────────────────────────────────────────

async fn cmd_pdf(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
    event_id: OwnedEventId,
    thread_root: OwnedEventId,
) -> Result<Option<RoomMessageEventContent>> {
    require_admin(ctx, sender)?;
    // !pdf [weeks] [group name]
    // First arg: either a number (weeks) or start of group name.
    let (n, group_filter) = {
        let weeks = args.first().and_then(|s| s.parse::<usize>().ok());
        if let Some(w) = weeks {
            let name = if args.len() > 1 { Some(args[1..].join(" ")) } else { None };
            (w.clamp(1, 52), name)
        } else {
            let name = if !args.is_empty() { Some(args.join(" ")) } else { None };
            (8, name)
        }
    };

    // Refresh Matrix display names so the PDF shows "Thomas" not "thomas99".
    refresh_display_names(ctx, room).await;

    let (html, file_name) = {
        let state    = ctx.state.lock().await;
        let interval = ctx.config.schedule.interval_weeks;
        let mut snapshot = build_schedule(&state, interval, n);
        if let Some(ref name) = group_filter {
            match state.group_by_name(name) {
                Some(g) => {
                    let gid = g.id.clone();
                    snapshot.assignments.retain(|a| a.group_id == gid);
                }
                None => return Ok(Some(
                    RoomMessageEventContent::text_plain(format!("Group «{name}» not found."))
                )),
            }
        }
        let html = crate::pdf::render_pdf(&snapshot);
        // Build filename: cleaning-plan-KW{first}-KW{last}.pdf
        let weeks_range = {
            let first = snapshot.assignments.first();
            let last  = snapshot.assignments.last();
            match (first, last) {
                (Some(f), Some(l)) if (f.iso_year, f.iso_week) != (l.iso_year, l.iso_week) =>
                    format!("KW{}-KW{}", f.iso_week, l.iso_week),
                (Some(f), _) =>
                    format!("KW{}", f.iso_week),
                _ => format!("{n}w"),
            }
        };
        let file_name = match &group_filter {
            Some(name) => format!("cleaning-plan-{}_{}.pdf", name.to_lowercase().replace(' ', "_"), weeks_range),
            None       => format!("cleaning-plan-{weeks_range}.pdf"),
        };
        (html, file_name)
    };

    // Render HTML → PDF via headless Chromium.
    let pdf_bytes = match crate::pdf_renderer::html_to_pdf(&html).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("Chromium PDF render failed: {e}");
            return Ok(Some(format::mentionify(&format!("❌ PDF render failed: {e}"))));
        }
    };

    let mime: mime::Mime = "application/pdf".parse().expect("valid mime");
    let pdf_size = pdf_bytes.len();
    match room.client().media().upload(&mime, pdf_bytes, None).await {
        Ok(upload) => {
            let mut file_info = FileInfo::new();
            file_info.mimetype = Some("application/pdf".to_owned());
            file_info.size     = UInt::new(pdf_size as u64);
            let mut fc = FileMessageEventContent::plain(file_name, upload.content_uri);
            fc.info = Some(Box::new(file_info));
            let mut file_content = RoomMessageEventContent::new(MessageType::File(fc));
            file_content.relates_to = Some(matrix_sdk::ruma::events::room::message::Relation::Thread(
                Thread::reply(thread_root.clone(), event_id.clone())
            ));
            room.send(file_content).await.ok();
            Ok(Some(format::mentionify("📄 Schedule generated.")))
        }
        Err(e) => {
            tracing::warn!("PDF upload failed: {e}");
            Ok(Some(format::mentionify("❌ Upload failed.")))
        }
    }
}

// ── !ical [N] / !ical <person> [N] ────────────────────────────────────────────

async fn cmd_ical(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    let sender_mxid = sender.as_str();
    let is_admin    = ctx.admin_users.contains(sender);

    // Parse target person and week count.
    let (person_id, weeks): (String, usize) = {
        let state = ctx.state.lock().await;
        match args.first() {
            None => {
                let pid = match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
                    Some(id) => id,
                    None     => return Ok(Some(format::mentionify(
                        &format!("You ({sender_mxid}) are not registered. Ask an admin to use !adduser.")
                    ))),
                };
                (pid, 26)
            }
            Some(first) => {
                if let Ok(n) = first.parse::<usize>() {
                    let pid = match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
                        Some(id) => id,
                        None     => return Ok(Some(format::mentionify(
                            &format!("You ({sender_mxid}) are not registered.")
                        ))),
                    };
                    (pid, n.clamp(1, 104))
                } else {
                    if !is_admin {
                        return Ok(Some(format::mentionify("❌ Admin permission required to generate iCal for others.")));
                    }
                    let person = match state.find_person(first).cloned() {
                        Some(p) => p,
                        None    => return Ok(Some(format::mentionify(&format!("Person «{first}» not found.")))),
                    };
                    let n = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(26usize).clamp(1, 104);
                    (person.id.clone(), n)
                }
            }
        }
    };

    // If HTTP server is configured, return URL (token-based feed).
    if let Some(ical_cfg) = &ctx.config.ical_server {
        let mut state = ctx.state.lock().await;

        // Check if a non-revoked token already exists for this person.
        let has_token = state.calendar_tokens.iter()
            .any(|ct| !ct.revoked && ct.person_id == person_id);

        if has_token {
            return Ok(Some(format::mentionify(
                "📅 You already have an active calendar feed.\n\
                 Use !icalreset to get a new URL (this invalidates the old subscription)."
            )));
        }

        let (raw_token, hash) = new_calendar_token();
        state.calendar_tokens.push(CalendarToken {
            id:         Uuid::new_v4().to_string(),
            token_hash: hash,
            person_id:  person_id.clone(),
            created_at: Utc::now(),
            revoked:    false,
        });
        state.save(&ctx.state_path).await?;
        drop(state);

        let url = format!("{}/ical/{raw_token}.ics", ical_cfg.public_url);
        return Ok(Some(format::mentionify(&format!(
            "📅 Your calendar feed URL:\n{url}\n\n\
             Add this URL to your calendar app for automatic updates.\n\
             ⚠️ This URL is shown only once — save it!"
        ))));
    }

    // Fallback: generate and upload as a Matrix file attachment.
    let ical_data = {
        let state    = ctx.state.lock().await;
        let interval = ctx.config.schedule.interval_weeks;
        let snapshot = build_schedule(&state, interval, weeks);
        crate::ical::render_ics(&snapshot, &person_id)
    };

    let safe_name = person_id.trim_start_matches('@')
        .split(':').next().unwrap_or("user")
        .chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>();

    let mime: mime::Mime = "text/calendar".parse().expect("valid mime");
    match room.client().media().upload(&mime, ical_data.into_bytes(), None).await {
        Ok(upload) => {
            use matrix_sdk::ruma::events::room::message::{FileMessageEventContent, MessageType};
            let content = RoomMessageEventContent::new(MessageType::File(
                FileMessageEventContent::plain(format!("putzplan_{safe_name}.ics"), upload.content_uri)
            ));
            room.send(content).await.ok();
            Ok(Some(format::mentionify(&format!(
                "📅 iCal · {weeks} Wochen · Import .ics into your calendar app.\n\
                 Tip: configure [ical_server] in config.toml for live-updating feed URLs."
            ))))
        }
        Err(e) => {
            tracing::warn!("iCal upload failed: {e}");
            Ok(Some(format::mentionify("❌ Upload failed.")))
        }
    }
}

// ── !icalreset [person] ───────────────────────────────────────────────────────

async fn cmd_icalreset(
    ctx: &BotContext,
    sender: &OwnedUserId,
    _room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    let sender_mxid = sender.as_str();
    let is_admin    = ctx.admin_users.contains(sender);

    let Some(ical_cfg) = &ctx.config.ical_server else {
        return Ok(Some(format::mentionify(
            "iCal HTTP server is not configured. Add [ical_server] to config.toml."
        )));
    };

    let person_id: String = {
        let state = ctx.state.lock().await;
        match args.first() {
            None => {
                match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
                    Some(id) => id,
                    None     => return Ok(Some(format::mentionify(&format!(
                        "{sender_mxid} is not registered."
                    )))),
                }
            }
            Some(query) => {
                if !is_admin {
                    return Ok(Some(format::mentionify("❌ Admin permission required.")));
                }
                match state.find_person(query).map(|p| p.id.clone()) {
                    Some(id) => id,
                    None     => return Ok(Some(format::mentionify(&format!("Person «{query}» not found.")))),
                }
            }
        }
    };

    let mut state = ctx.state.lock().await;
    // Revoke all existing tokens for this person.
    for ct in state.calendar_tokens.iter_mut().filter(|ct| ct.person_id == person_id) {
        ct.revoked = true;
    }

    // Issue new token.
    let (raw_token, hash) = new_calendar_token();
    state.calendar_tokens.push(CalendarToken {
        id:         Uuid::new_v4().to_string(),
        token_hash: hash,
        person_id:  person_id.clone(),
        created_at: Utc::now(),
        revoked:    false,
    });
    state.save(&ctx.state_path).await?;
    drop(state);

    let url = format!("{}/ical/{raw_token}.ics", ical_cfg.public_url);
    Ok(Some(format::mentionify(&format!(
        "🔄 New calendar feed URL:\n{url}\n\n\
         ⚠️ Old URLs for this person are now invalid."
    ))))
}

// ── !testnotify ───────────────────────────────────────────────────────────────

async fn cmd_testnotify(room: &Room) -> Result<Option<RoomMessageEventContent>> {
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
    Ok(Some(RoomMessageEventContent::text_plain("⏱ @room notification in 5 minutes.")))
}

// ── Admin: !validate ──────────────────────────────────────────────────────────

async fn cmd_validate(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let state = ctx.state.lock().await;
    let report = crate::validate::validate_state(&state);
    Ok(Some(report.summary()))
}

// ── help ─────────────────────────────────────────────────────────────────────

fn help_text() -> String {
    r#"🧹 Cleaning bot commands:

  !help                        · show this help
  !status                      · this week's cleaned / not-cleaned overview
  !areas                       · list all cleaning groups and their members
  !done [group]                · mark your cleaning done for this week
  !undo [group]                · undo this week's done mark
  !next [@user]                · when is your (or @user's) next due week?
  !stats [@user]               · completion statistics
  !leaderboard                 · overall cleaning leaderboard with streaks
  !fairness [group]            · fairness report — who's doing their share?
  !cleanplan [N]               · show the next N due cleaning weeks (default 6)
  !blame                       · all due but uncleaned groups this week
  !blame @user                 · cleaning record for one person
  !blame <group>               · cleaning record for a specific group
  !joingroup <group>            · add yourself to a cleaning group
  !leavegroup <group>           · remove yourself from a cleaning group
  !swap @user [group] [week N] · propose a swap; !acceptswap / !rejectswap to respond
  !acceptswap <id>             · accept a pending swap request
  !rejectswap <id>             · reject a pending swap request
  !ical [N]                    · get your cleaning schedule as iCal (default 26 weeks)
  !icalreset                   · revoke and regenerate your iCal feed URL

Admin commands:
  !skip [group]                         · excuse this week (won't count as missed)
  !remind [group]                       · manually fire the cleaning reminder now
  !pdf [N]                              · generate printable HTML schedule (default 8 weeks)
  !absent <person> [weeks]              · mark person away (default 4 weeks)
  !back <person>                        · cancel an absence early
  !addgroup <name>                      · create a new cleaning group
  !removegroup <name>                   · delete a cleaning group
  !adduser @user <group>                · assign a Matrix user to a group
  !removeuser @user <group>             · unassign a Matrix user from a group
  !addperson <name> <group>             · add a non-Matrix person to a group
  !removeperson <name> <group>          · remove a non-Matrix person from a group
  !addroom <group> <room>               · add a room to clean in a group
  !removeroom <group> <room>            · remove a room from a group
  !ical <person> [N]                    · get iCal for any person (admin)
  !icalreset <person>                   · reset iCal token for any person (admin)
  !validate                             · check state for consistency issues"#.to_owned()
}
