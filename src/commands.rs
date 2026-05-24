use anyhow::Result;
use chrono::Utc;
use matrix_sdk::{Room, ruma::{events::room::message::RoomMessageEventContent, OwnedUserId}};

use crate::{
    BotContext, format,
    state::{
        Absence, Completion, Floor, FloorGroup, SwapRequest, SwapStatus,
        add_weeks, current_iso_week, week_dates, weeks_between,
    },
};

pub async fn handle(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    body: &str,
) -> Result<Option<RoomMessageEventContent>> {
    let mut parts = body.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let args: Vec<&str> = parts.collect();

    // Commands that need direct room access return RoomMessageEventContent.
    match cmd {
        "!cleanplan" => return cmd_cleanplan(ctx, sender, room, &args).await,
        "!remind"    => return cmd_remind(ctx, sender, room, &args).await,
        _ => {}
    }

    // All other commands return plain strings; wrap them in display-name pills.
    let reply: Option<String> = match cmd {
        "!done"         => cmd_done(ctx, sender, &args).await,
        "!status"       => cmd_status(ctx).await,
        "!stats"        => cmd_stats(ctx, &args).await,
        "!floors"       => cmd_floors(ctx).await,
        "!joinfloor"    => cmd_joinfloor(ctx, sender, &args).await,
        "!leavefloor"   => cmd_leavefloor(ctx, sender, &args).await,
        "!swap"         => cmd_swap(ctx, sender, &args).await,
        "!acceptswap"   => cmd_acceptswap(ctx, sender, &args).await,
        "!rejectswap"   => cmd_rejectswap(ctx, sender, &args).await,
        "!adduser"      => cmd_adduser(ctx, sender, &args).await,
        "!removeuser"   => cmd_removeuser(ctx, sender, &args).await,
        "!addfloor"     => cmd_addfloor(ctx, sender, &args).await,
        "!removefloor"  => cmd_removefloor(ctx, sender, &args).await,
        "!addroom"      => cmd_addroom(ctx, sender, &args).await,
        "!removeroom"   => cmd_removeroom(ctx, sender, &args).await,
        "!linkfloors"   => cmd_linkfloors(ctx, sender, &args).await,
        "!unlinkfloors" => cmd_unlinkfloors(ctx, sender, &args).await,
        "!undo"         => cmd_undo(ctx, sender, &args).await,
        "!next"         => cmd_next(ctx, sender, &args).await,
        "!skip"         => cmd_skip(ctx, sender, &args).await,
        "!leaderboard"  => cmd_leaderboard(ctx).await,
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

// ── !done [floor_or_group] ────────────────────────────────────────────────────

async fn cmd_done(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let (year, week) = current_iso_week();
    let sender_str = sender.as_str();
    let mut state = ctx.state.lock().await;

    // Determine target unit(s): explicit arg, or all units this user belongs to.
    let target_units: Vec<String> = if let Some(name) = args.first() {
        vec![name.to_string()]
    } else {
        state.units()
            .iter()
            .filter(|u| u.users.iter().any(|u2| u2 == sender_str))
            .map(|u| u.name.clone())
            .collect()
    };

    if target_units.is_empty() {
        return Ok(Some("You are not assigned to any floor or group.".into()));
    }

    // Also accept if the user was swapped in
    let mut marked = vec![];
    let mut already_done = vec![];
    let now = Utc::now();

    for unit_name in &target_units {
        // Check if this user is allowed: assigned or swap-accepted
        let units = state.units();
        let unit = units.iter().find(|u| &u.name == unit_name);
        let assigned = unit.map(|u| u.users.iter().any(|u2| u2 == sender_str)).unwrap_or(false);
        let swapped_in = state.swap_requests.iter().any(|s| {
            s.unit_name == *unit_name
                && s.iso_year == year
                && s.iso_week == week
                && s.status == SwapStatus::Accepted
                && s.target == sender_str
        });

        if !assigned && !swapped_in {
            return Ok(Some(format!(
                "You are not responsible for «{}» this week.",
                unit_name
            )));
        }

        if state.is_completed(unit_name, year, week) {
            already_done.push(unit_name.clone());
            continue;
        }

        let responsible_users = {
            let units = state.units();
            let unit = units.iter().find(|u| u.name == *unit_name);
            let interval = ctx.config.schedule.interval_weeks;
            unit.and_then(|u| state.responsible_user(u, year, week, interval))
                .map(|u| vec![u])
                .unwrap_or_default()
        };
        state.completions.push(Completion {
            unit_name: unit_name.clone(),
            iso_year: year,
            iso_week: week,
            completed_at: now,
            completed_by: sender_str.to_owned(),
            responsible_users,
            skipped: false,
        });
        marked.push(unit_name.clone());
    }

    state.save(&ctx.state_path).await?;
    drop(state);

    let mut lines = vec![];
    if !marked.is_empty() {
        lines.push(format!("✅ Marked as cleaned: {}", marked.join(", ")));
    }
    if !already_done.is_empty() {
        lines.push(format!("Already marked done this week: {}", already_done.join(", ")));
    }
    Ok(Some(lines.join("\n")))
}

// ── !status ───────────────────────────────────────────────────────────────────

async fn cmd_status(ctx: &BotContext) -> Result<Option<String>> {
    let (year, week) = current_iso_week();
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let units = state.units();

    if units.is_empty() {
        return Ok(Some("No floors configured yet.".into()));
    }

    let mut lines = vec![format!("📋 Cleaning status — week {week} ({}):", week_dates(year, week))];
    for unit in &units {
        let done = state.is_completed(&unit.name, year, week);
        let icon = if done { "✅" } else { "❌" };
        let who = if done {
            state.completions.iter()
                .find(|c| c.unit_name == unit.name && c.iso_year == year && c.iso_week == week)
                .map(|c| format!(" (by {})", c.completed_by))
                .unwrap_or_default()
        } else {
            match state.responsible_user(&unit, year, week, interval) {
                Some(u) => format!(" — responsible: {u}"),
                None    => " — (nobody assigned)".into(),
            }
        };
        let rooms_str = unit.rooms_text()
            .map(|r| format!("\n      {r}"))
            .unwrap_or_default();
        lines.push(format!("  {icon} **{}**{who}{rooms_str}", unit.name));
    }
    Ok(Some(lines.join("\n")))
}

// ── !stats [@user] ────────────────────────────────────────────────────────────

async fn cmd_stats(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let (start_y, start_w) = state.tracking_start();
    let (cur_y, cur_w) = current_iso_week();

    if let Some(user) = args.first().copied() {
        return Ok(Some(stats_for_user(&state, user, interval)));
    }

    // ── Global stats ──────────────────────────────────────────────────────────
    let mut lines = vec![
        format!("📊 Cleaning statistics — tracking since week {start_w} ({}):", week_dates(start_y, start_w)),
    ];

    let units = state.units();
    if units.is_empty() {
        lines.push("  No floors configured yet.".into());
        return Ok(Some(lines.join("\n")));
    }

    for unit in &units {
        let due_weeks = state.all_due_weeks(interval, (cur_y, cur_w));
        // Exclude the current (ongoing) week from "due so far" for fairness
        let closed_due: Vec<_> = due_weeks.iter()
            .filter(|&&(y, w)| (y, w) != (cur_y, cur_w))
            .collect();
        let n_due = closed_due.len();
        let n_done = closed_due.iter()
            .filter(|(y, w)| state.is_completed(&unit.name, *y, *w))
            .count();
        let n_missed = n_due - n_done;
        let pct = if n_due > 0 { 100 * n_done / n_due } else { 100 };
        let streak = state.streak_for(&unit.name, interval);
        let this_week = state.is_completed(&unit.name, cur_y, cur_w);

        lines.push(String::new());
        lines.push(format!("🏢 {}", unit.name));
        lines.push(format!(
            "   Completed: {n_done}/{n_due} ({pct}%)  │  Missed: {n_missed}"
        ));
        lines.push(format!(
            "   Streak: {streak}  │  This week: {}",
            if this_week { "✅" } else { "❌" }
        ));

        if let Some(last) = state.last_completed(&unit.name) {
            lines.push(format!(
                "   Last cleaned: week {week} ({dates}) by {by}",
                week = last.iso_week,
                dates = week_dates(last.iso_year, last.iso_week),
                by = last.completed_by,
            ));
        } else {
            lines.push("   Never cleaned.".into());
        }

        // Per-user completion count
        let mut per_user: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for c in state.completions.iter().filter(|c| c.unit_name == unit.name) {
            *per_user.entry(c.completed_by.as_str()).or_default() += 1;
        }
        for u in &unit.users {
            let cnt = per_user.get(u.as_str()).copied().unwrap_or(0);
            let miss_u = if n_due > 0 { n_due - cnt.min(n_due) } else { 0 };
            lines.push(format!("   {u}: {cnt} cleanings  ({miss_u} missed)"));
        }

        // List missed weeks (cap at 10 to avoid wall of text)
        let missed = state.missed_weeks_for(&unit.name, interval);
        if !missed.is_empty() {
            let shown: Vec<String> = missed.iter().take(10)
                .map(|(y, w)| format!("w{w} ({})", week_dates(*y, *w)))
                .collect();
            let suffix = if missed.len() > 10 {
                format!(" (+{})", missed.len() - 10)
            } else {
                String::new()
            };
            lines.push(format!("   Missed weeks: {}{suffix}", shown.join(", ")));
        }
    }

    Ok(Some(lines.join("\n")))
}

fn stats_for_user(state: &crate::state::State, user: &str, interval: u32) -> String {
    let (cur_y, cur_w) = current_iso_week();
    let (start_y, start_w) = state.tracking_start();

    let unit_opt = state.unit_for_user(user);
    let unit_name = unit_opt.as_ref().map(|u| u.name.as_str()).unwrap_or("(unassigned)");

    let due_weeks = state.all_due_weeks(interval, (cur_y, cur_w));
    let closed_due: Vec<_> = due_weeks.iter()
        .filter(|&&(y, w)| (y, w) != (cur_y, cur_w))
        .collect();
    let n_due = closed_due.len();

    let user_completions: Vec<_> = state.completions.iter()
        .filter(|c| c.completed_by == user)
        .collect();
    let n_done = user_completions.len();
    let n_missed = n_due.saturating_sub(n_done);
    let pct = if n_due > 0 { 100 * n_done / n_due } else { 100 };

    let streak = unit_opt.as_ref()
        .map(|u| state.streak_for(&u.name, interval))
        .unwrap_or(0);

    let mut lines = vec![
        format!("📊 Stats for {user} — tracking since week {start_w} ({}):", week_dates(start_y, start_w)),
        format!("   Unit:             {unit_name}"),
        format!("   Total cleanings:  {n_done}"),
        format!("   Due weeks so far: {n_due}"),
        format!("   Missed:           {n_missed}  ({pct}% completion rate)"),
        format!("   Current streak:   {streak} consecutive weeks"),
    ];

    if let Some(last) = user_completions.iter().max_by_key(|c| c.completed_at) {
        lines.push(format!(
            "   Last cleaned:     week {w} ({dates})",
            w = last.iso_week,
            dates = week_dates(last.iso_year, last.iso_week),
        ));
    }

    // Most recent 5 completions
    let mut sorted = user_completions.clone();
    sorted.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));
    if !sorted.is_empty() {
        lines.push("   Recent:".into());
        for c in sorted.iter().take(5) {
            lines.push(format!(
                "     • {} — week {w} ({})",
                c.unit_name, week_dates(c.iso_year, c.iso_week), w = c.iso_week,
            ));
        }
    }

    lines.join("\n")
}

// ── !floors ───────────────────────────────────────────────────────────────────

async fn cmd_floors(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    if state.floors.is_empty() {
        return Ok(Some("No floors configured.".into()));
    }

    let mut lines = vec!["🏢 Floors:".to_owned()];
    for floor in &state.floors {
        let users = if floor.users.is_empty() {
            "(no users)".to_owned()
        } else {
            floor.users.join(", ")
        };
        let rooms_str = if floor.rooms.is_empty() {
            String::new()
        } else {
            format!(" — Rooms: {}", floor.rooms.join(", "))
        };
        lines.push(format!("  • **{}**: {users}{rooms_str}", floor.name));

    }

    if !state.groups.is_empty() {
        lines.push("Groups:".into());
        for g in &state.groups {
            lines.push(format!("  • **{}**: floors {}", g.name, g.floor_names.join(", ")));
        }
    }
    Ok(Some(lines.join("\n")))
}

// ── !swap @target [floor] [week <N>] ─────────────────────────────────────────

async fn cmd_swap(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let target_str = match args.first() {
        Some(t) => *t,
        None => return Ok(Some("Usage: !swap @user [floor_or_group] [week <N>]".into())),
    };

    let sender_str = sender.as_str();
    let (cur_year, cur_week) = current_iso_week();

    // Parse optional `week <N>` from the tail of the args.
    // Any tokens before the `week` keyword are treated as the floor name.
    let remaining = &args[1..]; // everything after @user
    let (floor_args, (year, week)) = if let Some(pos) = remaining.iter().position(|a| a.eq_ignore_ascii_case("week")) {
        let n_str = remaining.get(pos + 1).copied().unwrap_or("");
        match n_str.parse::<u32>() {
            Ok(n) if n >= 1 && n <= 53 => {
                // If the requested week is already past this year, assume next year.
                let y = if n < cur_week { cur_year + 1 } else { cur_year };
                (&remaining[..pos], (y, n))
            }
            _ => return Ok(Some("Usage: !swap @user [floor] [week <1-53>]".into())),
        }
    } else {
        (remaining, (cur_year, cur_week))
    };

    // Can't swap weeks that have already passed.
    if (year, week) < (cur_year, cur_week) {
        return Ok(Some(format!(
            "Week {week} ({}) is already in the past.",
            week_dates(year, week)
        )));
    }

    let mut state = ctx.state.lock().await;

    // Determine which unit the sender belongs to (or explicit override).
    let unit_name = if let Some(name) = floor_args.first() {
        name.to_string()
    } else {
        match state.unit_for_user(sender_str) {
            Some(u) => u.name.clone(),
            None => return Ok(Some("You are not assigned to any floor. Specify: !swap @user floor_name".into())),
        }
    };

    // Sender must be assigned to the unit; target can be anyone.
    let units = state.units();
    let unit = match units.iter().find(|u| u.name == unit_name) {
        Some(u) => u,
        None => return Ok(Some(format!("Unknown unit «{unit_name}»"))),
    };

    if !unit.users.iter().any(|u| u == sender_str) {
        return Ok(Some(format!("You are not assigned to «{unit_name}».")));
    }
    if sender_str == target_str {
        return Ok(Some("You cannot swap with yourself.".into()));
    }

    // Avoid duplicate pending requests for the same unit/week.
    let dupe = state.swap_requests.iter().any(|s| {
        s.unit_name == unit_name
            && s.iso_year == year
            && s.iso_week == week
            && s.status == SwapStatus::Pending
            && s.requester == sender_str
    });
    if dupe {
        return Ok(Some(format!(
            "You already have a pending swap request for «{unit_name}» week {week} ({}).",
            week_dates(year, week)
        )));
    }

    let id = state.alloc_id();
    state.swap_requests.push(SwapRequest {
        id,
        requester: sender_str.to_owned(),
        target: target_str.to_owned(),
        unit_name: unit_name.clone(),
        iso_year: year,
        iso_week: week,
        created_at: Utc::now(),
        status: SwapStatus::Pending,
    });
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "🔄 Swap request #{id} sent to {target_str} for «{unit_name}» (week {week}, {dates}).\n\
         {target_str}: reply `!acceptswap {id}` or `!rejectswap {id}`.",
        dates = week_dates(year, week),
    )))
}

// ── !acceptswap <id> ──────────────────────────────────────────────────────────

async fn cmd_acceptswap(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let id: u64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return Ok(Some("Usage: !acceptswap <id>".into())),
    };
    let sender_str = sender.as_str();
    let mut state = ctx.state.lock().await;

    let req = match state.swap_requests.iter_mut().find(|r| r.id == id) {
        Some(r) => r,
        None => return Ok(Some(format!("Swap request #{id} not found."))),
    };

    if req.target != sender_str {
        return Ok(Some("This swap request is not addressed to you.".into()));
    }
    if req.status != SwapStatus::Pending {
        return Ok(Some(format!("Request #{id} is already {:?}.", req.status)));
    }

    let requester = req.requester.clone();
    let unit = req.unit_name.clone();
    req.status = SwapStatus::Accepted;
    state.save(&ctx.state_path).await?;

    Ok(Some(format!(
        "✅ Swap #{id} accepted. {sender_str} will clean «{unit}» this week instead of {requester}."
    )))
}

// ── !rejectswap <id> ─────────────────────────────────────────────────────────

async fn cmd_rejectswap(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let id: u64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return Ok(Some("Usage: !rejectswap <id>".into())),
    };
    let sender_str = sender.as_str();
    let mut state = ctx.state.lock().await;

    let req = match state.swap_requests.iter_mut().find(|r| r.id == id) {
        Some(r) => r,
        None => return Ok(Some(format!("Swap request #{id} not found."))),
    };

    if req.target != sender_str {
        return Ok(Some("This swap request is not addressed to you.".into()));
    }
    if req.status != SwapStatus::Pending {
        return Ok(Some(format!("Request #{id} is already {:?}.", req.status)));
    }

    req.status = SwapStatus::Rejected;
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("❌ Swap #{id} rejected.")))
}

// ── !joinfloor <floor> ────────────────────────────────────────────────────────

async fn cmd_joinfloor(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let floor_name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !joinfloor <floor_name>".into())),
    };

    let sender_str = sender.as_str();
    let mut state = ctx.state.lock().await;

    let floor = match state.floors.iter_mut().find(|f| f.name == floor_name) {
        Some(f) => f,
        None => return Ok(Some(format!("Floor «{floor_name}» not found.  Use !floors to see available floors."))),
    };

    if floor.users.iter().any(|u| u == sender_str) {
        return Ok(Some(format!("You are already on floor «{floor_name}».")));
    }
    floor.users.push(sender_str.to_owned());
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ You have joined floor «{floor_name}».")))
}

// ── !leavefloor <floor> ───────────────────────────────────────────────────────

async fn cmd_leavefloor(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let floor_name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !leavefloor <floor_name>".into())),
    };

    let sender_str = sender.as_str();
    let mut state = ctx.state.lock().await;

    let floor = match state.floors.iter_mut().find(|f| f.name == floor_name) {
        Some(f) => f,
        None => return Ok(Some(format!("Floor «{floor_name}» not found."))),
    };

    let before = floor.users.len();
    floor.users.retain(|u| u != sender_str);
    if floor.users.len() == before {
        return Ok(Some(format!("You are not on floor «{floor_name}».")));
    }
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ You have left floor «{floor_name}».")))
}

// ── Admin: !adduser @user floor ───────────────────────────────────────────────

async fn cmd_adduser(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (user, floor_name) = match (args.first(), args.get(1)) {
        (Some(u), Some(f)) => (*u, f.to_string()),
        _ => return Ok(Some("Usage: !adduser @user floor_name".into())),
    };

    let mut state = ctx.state.lock().await;
    let floor = match state.floors.iter_mut().find(|f| f.name == floor_name) {
        Some(f) => f,
        None => return Ok(Some(format!("Floor «{floor_name}» not found."))),
    };

    if floor.users.iter().any(|u| u == user) {
        return Ok(Some(format!("{user} is already on floor «{floor_name}».")));
    }
    floor.users.push(user.to_owned());
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Added {user} to floor «{floor_name}».")))
}

// ── Admin: !removeuser @user floor ───────────────────────────────────────────

async fn cmd_removeuser(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (user, floor_name) = match (args.first(), args.get(1)) {
        (Some(u), Some(f)) => (*u, f.to_string()),
        _ => return Ok(Some("Usage: !removeuser @user floor_name".into())),
    };

    let mut state = ctx.state.lock().await;
    let floor = match state.floors.iter_mut().find(|f| f.name == floor_name) {
        Some(f) => f,
        None => return Ok(Some(format!("Floor «{floor_name}» not found."))),
    };

    let before = floor.users.len();
    floor.users.retain(|u| u != user);
    if floor.users.len() == before {
        return Ok(Some(format!("{user} is not on floor «{floor_name}».")));
    }
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Removed {user} from floor «{floor_name}».")))
}

// ── Admin: !addfloor name ─────────────────────────────────────────────────────

async fn cmd_addfloor(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !addfloor floor_name".into())),
    };

    let mut state = ctx.state.lock().await;
    if state.floors.iter().any(|f| f.name == name) {
        return Ok(Some(format!("Floor «{name}» already exists.")));
    }
    state.floors.push(Floor { name: name.clone(), users: vec![], rooms: vec![] });
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Added floor «{name}».")))
}

// ── Admin: !removefloor name ──────────────────────────────────────────────────

async fn cmd_removefloor(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !removefloor floor_name".into())),
    };

    let mut state = ctx.state.lock().await;
    let before = state.floors.len();
    state.floors.retain(|f| f.name != name);
    state.groups.iter_mut().for_each(|g| g.floor_names.retain(|fn_| fn_ != &name));
    if state.floors.len() == before {
        return Ok(Some(format!("Floor «{name}» not found.")));
    }
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Removed floor «{name}».")))
}

// ── Admin: !addroom <floor> <room> ───────────────────────────────────────────

async fn cmd_addroom(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (floor_name, room_name) = match (args.first(), args.get(1..).map(|s| s.join(" "))) {
        (Some(f), Some(r)) if !r.is_empty() => (f.to_string(), r),
        _ => return Ok(Some("Usage: !addroom <floor> <room name>".into())),
    };

    let mut state = ctx.state.lock().await;
    let floor = match state.floors.iter_mut().find(|f| f.name == floor_name) {
        Some(f) => f,
        None => return Ok(Some(format!("Floor «{floor_name}» not found."))),
    };

    if floor.rooms.iter().any(|r| r.eq_ignore_ascii_case(&room_name)) {
        return Ok(Some(format!("Room «{room_name}» already exists on floor «{floor_name}».")));
    }
    floor.rooms.push(room_name.clone());
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Added room «{room_name}» to floor «{floor_name}».")))
}

// ── Admin: !removeroom <floor> <room> ────────────────────────────────────────

async fn cmd_removeroom(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (floor_name, room_name) = match (args.first(), args.get(1..).map(|s| s.join(" "))) {
        (Some(f), Some(r)) if !r.is_empty() => (f.to_string(), r),
        _ => return Ok(Some("Usage: !removeroom <floor> <room name>".into())),
    };

    let mut state = ctx.state.lock().await;
    let floor = match state.floors.iter_mut().find(|f| f.name == floor_name) {
        Some(f) => f,
        None => return Ok(Some(format!("Floor «{floor_name}» not found."))),
    };

    let before = floor.rooms.len();
    floor.rooms.retain(|r| !r.eq_ignore_ascii_case(&room_name));
    if floor.rooms.len() == before {
        return Ok(Some(format!("Room «{room_name}» not found on floor «{floor_name}».")));
    }
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Removed room «{room_name}» from floor «{floor_name}».")))
}

// ── Admin: !linkfloors group_name floor1 floor2 … ────────────────────────────

async fn cmd_linkfloors(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    if args.len() < 2 {
        return Ok(Some("Usage: !linkfloors group_name floor1 [floor2 …]".into()));
    }
    let group_name = args[0].to_string();
    let floor_names: Vec<String> = args[1..].iter().map(|s| s.to_string()).collect();

    let mut state = ctx.state.lock().await;
    for fn_ in &floor_names {
        if !state.floors.iter().any(|f| &f.name == fn_) {
            return Ok(Some(format!("Floor «{fn_}» not found.")));
        }
    }

    if let Some(g) = state.groups.iter_mut().find(|g| g.name == group_name) {
        g.floor_names = floor_names.clone();
    } else {
        state.groups.push(FloorGroup { name: group_name.clone(), floor_names: floor_names.clone() });
    }
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Group «{group_name}» now covers: {}.", floor_names.join(", "))))
}

// ── Admin: !unlinkfloors group_name ──────────────────────────────────────────

async fn cmd_unlinkfloors(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let name = match args.first() {
        Some(n) => n.to_string(),
        None => return Ok(Some("Usage: !unlinkfloors group_name".into())),
    };

    let mut state = ctx.state.lock().await;
    let before = state.groups.len();
    state.groups.retain(|g| g.name != name);
    if state.groups.len() == before {
        return Ok(Some(format!("Group «{name}» not found.")));
    }
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ Removed group «{name}».")))
}

// ── !cleanplan [N] ────────────────────────────────────────────────────────────

/// Show the next N due cleaning occurrences.
///
/// N defaults to 6 (a comfortable lookahead regardless of interval).
/// Due weeks are generated by stepping `interval_weeks` forward from the first
/// due week at or after the current week.
///
/// **Membership is always "current"**: whoever is on a floor/group right now
/// is shown for every future week.  There is no rotation — all assigned users
/// are responsible every due week together.
///
/// • New person joins  → they appear in !cleanplan from the very next due week.
/// • Person leaves     → they disappear from future weeks immediately; past
///                       `Completion` records keep the snapshot of who was
///                       responsible at the time, so history is preserved.
async fn cmd_cleanplan(ctx: &BotContext, _sender: &OwnedUserId, room: &Room, args: &[&str]) -> Result<Option<RoomMessageEventContent>> {
    let n: usize = args
        .first()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(6)
        .clamp(1, 20);

    let state    = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let units    = state.units();

    if units.is_empty() {
        return Ok(Some(format::mentionify("No floors configured yet.")));
    }

    let (cur_y, cur_w) = current_iso_week();
    let (start_y, start_w) = state.tracking_start();
    let iv = interval as i64;

    // Find the first due week that is >= the current week.
    let weeks_elapsed = weeks_between((start_y, start_w), (cur_y, cur_w));
    let first_due = if weeks_elapsed < 0 {
        // Current week is before tracking_start; first due week = tracking_start.
        (start_y, start_w)
    } else {
        let past_intervals = weeks_elapsed / iv;
        if weeks_elapsed % iv == 0 {
            // Current week is itself a due week.
            (cur_y, cur_w)
        } else {
            // Next due week after the current week.
            add_weeks(start_y, start_w, (past_intervals + 1) * iv)
        }
    };

    // Collect every MXID that will appear in the output — one responsible user
    // per (unit, week) pair, plus anyone who completed a cleaning.
    let mut all_user_ids: Vec<String> = Vec::new();
    for i in 0..(n as i64) {
        let (dy, dw) = add_weeks(first_due.0, first_due.1, i * iv);
        for unit in &units {
            if let Some(u) = state.responsible_user(unit, dy, dw, interval) {
                if !all_user_ids.contains(&u) {
                    all_user_ids.push(u);
                }
            }
        }
    }
    for c in &state.completions {
        if !all_user_ids.iter().any(|x| x == &c.completed_by) {
            all_user_ids.push(c.completed_by.clone());
        }
    }
    // Release the state lock before the async room calls.
    drop(state);

    let uid_refs: Vec<&str> = all_user_ids.iter().map(String::as_str).collect();
    let names = format::fetch_names(room, &uid_refs).await;

    // Re-acquire state to build the output.
    let state    = ctx.state.lock().await;
    let units    = state.units();

    let mut lines = vec![format!(
        "📅 Cleaning plan — next {} due week{} (interval: every {} week{}):",
        n,
        if n == 1 { "" } else { "s" },
        interval,
        if interval == 1 { "" } else { "s" },
    )];

    // Helper: render a user ID as either the sender's full MXID (→ ping pill)
    // or another user's MXID (→ silent pill showing their display name).
    // mentionify_with_names will replace all MXIDs with display-name pills.
    // The sender's MXID is left as-is; mentionify will make it a mention.
    let fmt_user = |uid: &str| uid.to_owned();

    for i in 0..(n as i64) {
        let (dy, dw) = add_weeks(first_due.0, first_due.1, i * iv);
        let is_current = (dy, dw) == (cur_y, cur_w);

        lines.push(String::new());
        if is_current {
            lines.push(format!("📆 **Week {dw} ({})** ← this week", week_dates(dy, dw)));
        } else {
            lines.push(format!("📆 Week {dw} ({})", week_dates(dy, dw)));
        }

        for unit in &units {
            let done = state.is_completed(&unit.name, dy, dw);
            let icon = if done {
                "✅"
            } else if is_current {
                "🔲"
            } else {
                "🗓"
            };

            let detail = if done {
                let c = state.completions.iter()
                    .find(|c| c.unit_name == unit.name && c.iso_year == dy && c.iso_week == dw);
                match c {
                    Some(c) if c.skipped => "skipped ⏭️".to_owned(),
                    Some(c) => format!("done by {}", fmt_user(&c.completed_by)),
                    None    => "done".to_owned(),
                }
            } else {
                match state.responsible_user(unit, dy, dw, interval) {
                    None => "nobody assigned yet".to_owned(),
                    Some(u) => {
                        let floor_name = unit.floor_names.first()
                            .map(String::as_str)
                            .unwrap_or(&unit.name);
                        if state.is_absent(&u, floor_name, dy, dw) {
                            format!("{} (away)", fmt_user(&u))
                        } else {
                            fmt_user(&u)
                        }
                    }
                }
            };

            lines.push(format!("  {icon} {} — {detail}", unit.name));
        }
    }

    Ok(Some(format::mentionify_with_names(&lines.join("\n"), &names)))
}

// ── !undo [floor] ─────────────────────────────────────────────────────────────

async fn cmd_undo(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let (year, week) = current_iso_week();
    let sender_str   = sender.as_str();
    let is_admin     = ctx.admin_users.contains(sender);
    let mut state    = ctx.state.lock().await;

    let target_units: Vec<String> = if let Some(name) = args.first() {
        vec![name.to_string()]
    } else {
        state.units().iter()
            .filter(|u| u.users.iter().any(|u2| u2 == sender_str))
            .map(|u| u.name.clone())
            .collect()
    };

    if target_units.is_empty() {
        return Ok(Some("You are not assigned to any floor.".into()));
    }

    let mut undone  = vec![];
    let mut absent  = vec![];
    let mut no_perm = vec![];

    for unit_name in &target_units {
        let units    = state.units();
        let assigned = units.iter()
            .find(|u| u.name == *unit_name)
            .map(|u| u.users.iter().any(|u2| u2 == sender_str))
            .unwrap_or(false);

        if !assigned && !is_admin {
            no_perm.push(unit_name.clone());
            continue;
        }

        let before = state.completions.len();
        state.completions.retain(|c| {
            !(c.unit_name == *unit_name && c.iso_year == year && c.iso_week == week)
        });
        if state.completions.len() < before {
            undone.push(unit_name.clone());
            // Also clean up any stale reaction_done entries for this week.
            state.reaction_dones.retain(|_, rd| {
                !(rd.unit_name == *unit_name && rd.iso_year == year && rd.iso_week == week)
            });
        } else {
            absent.push(unit_name.clone());
        }
    }

    state.save(&ctx.state_path).await?;

    let mut lines = vec![];
    if !undone.is_empty()  { lines.push(format!("↩️ Undone this week: {}", undone.join(", "))); }
    if !absent.is_empty()  { lines.push(format!("Not marked done this week: {}", absent.join(", "))); }
    if !no_perm.is_empty() { lines.push(format!("❌ Not your floor: {}", no_perm.join(", "))); }
    Ok(Some(lines.join("\n")))
}

// ── !next [@user] ─────────────────────────────────────────────────────────────

async fn cmd_next(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    let state    = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let (cur_y, cur_w)     = current_iso_week();
    let (start_y, start_w) = state.tracking_start();
    let iv = interval as i64;

    let user = args.first().copied().unwrap_or_else(|| sender.as_str());

    let unit = match state.unit_for_user(user) {
        Some(u) => u,
        None => return Ok(Some(format!("{user} is not assigned to any floor."))),
    };

    // First due week >= current week (same logic as !cleanplan).
    let elapsed = weeks_between((start_y, start_w), (cur_y, cur_w));
    let first_due = if elapsed < 0 {
        (start_y, start_w)
    } else {
        let past = elapsed / iv;
        if elapsed % iv == 0 { (cur_y, cur_w) }
        else { add_weeks(start_y, start_w, (past + 1) * iv) }
    };

    let (dy, dw) = first_due;
    let away = weeks_between((cur_y, cur_w), (dy, dw));
    let when = match away {
        0 => "this week ⚠️".to_owned(),
        1 => "next week".to_owned(),
        n => format!("in {n} weeks"),
    };
    let done = state.is_cleaned(&unit.name, dy, dw);
    let suffix = if done { "  ✅ already done!" } else { "" };

    Ok(Some(format!(
        "📅 Next due for {user}: **Week {dw} ({dates})** ({when}) — {}{}",
        unit.name, suffix,
        dates = week_dates(dy, dw),
    )))
}

// ── !skip [floor] ─────────────────────────────────────────────────────────────

async fn cmd_skip(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;
    let (year, week) = current_iso_week();
    let interval     = ctx.config.schedule.interval_weeks;
    let now          = Utc::now();
    let mut state    = ctx.state.lock().await;

    let target_units: Vec<String> = if let Some(name) = args.first() {
        vec![name.to_string()]
    } else {
        state.units().iter()
            .filter(|u| state.is_due(&u.name, year, week, interval))
            .map(|u| u.name.clone())
            .collect()
    };

    let mut skipped = vec![];
    let mut already = vec![];

    for unit_name in &target_units {
        if state.is_completed(unit_name, year, week) {
            already.push(unit_name.clone());
            continue;
        }
        let responsible_users = state.units().into_iter()
            .find(|u| u.name == *unit_name)
            .map(|u| u.users)
            .unwrap_or_default();
        state.completions.push(Completion {
            unit_name: unit_name.clone(),
            iso_year: year,
            iso_week: week,
            completed_at: now,
            completed_by: sender.as_str().to_owned(),
            responsible_users,
            skipped: true,
        });
        skipped.push(unit_name.clone());
    }

    state.save(&ctx.state_path).await?;

    let mut lines = vec![];
    if !skipped.is_empty() {
        lines.push(format!("⏭️ Week skipped (won't count as missed): {}", skipped.join(", ")));
    }
    if !already.is_empty() {
        lines.push(format!("Already marked done this week: {}", already.join(", ")));
    }
    Ok(Some(lines.join("\n")))
}

// ── !remind [floor] ───────────────────────────────────────────────────────────

async fn cmd_remind(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    require_admin(ctx, sender)?;

    let (year, week) = current_iso_week();
    let interval     = ctx.config.schedule.interval_weeks;

    // Collect what we need from state, then release the lock for async sends.
    // tuple: (unit_name, [responsible_user], rooms_text)
    let reminder_data: Vec<(String, Vec<String>, Option<String>)> = {
        let state = ctx.state.lock().await;
        let units = state.units();
        if let Some(name) = args.first() {
            units.iter()
                .filter(|u| u.name.eq_ignore_ascii_case(name))
                .map(|u| {
                    let resp = state.responsible_user(u, year, week, interval)
                        .map(|s| vec![s])
                        .unwrap_or_default();
                    (u.name.clone(), resp, u.rooms_text())
                })
                .collect()
        } else {
            units.iter()
                .filter(|u| {
                    state.is_due(&u.name, year, week, interval)
                        && !state.is_completed(&u.name, year, week)
                })
                .map(|u| {
                    let resp = state.responsible_user(u, year, week, interval)
                        .map(|s| vec![s])
                        .unwrap_or_default();
                    (u.name.clone(), resp, u.rooms_text())
                })
                .collect()
        }
    };

    if reminder_data.is_empty() {
        return Ok(Some(format::mentionify(
            "✅ Nothing due and uncleaned right now.",
        )));
    }

    let mut sent = vec![];
    for (unit_name, users, rooms_text) in &reminder_data {
        let users_text = users.join(", ");
        let rooms_line = rooms_text.as_ref()
            .map(|r| format!("\n{r}"))
            .unwrap_or_default();
        let msg = format!(
            "🧹 Cleaning reminder — {unit_name} (week {week}, {dates})\n\
             Responsible: {users_text}{rooms_line}\n\
             React with ✅ or type !done when done.",
            dates = week_dates(year, week),
        );

        // Build with display names + push-notification mentions.
        let uid_refs: Vec<&str> = users.iter().map(String::as_str).collect();
        let names   = format::fetch_names(room, &uid_refs).await;
        let parsed: Vec<matrix_sdk::ruma::OwnedUserId> =
            users.iter().filter_map(|s| s.parse().ok()).collect();
        let content = format::mentionify_with_names(&msg, &names)
            .add_mentions(matrix_sdk::ruma::events::Mentions::with_user_ids(parsed));

        match room.send(content).await {
            Ok(resp) => {
                let mut state = ctx.state.lock().await;
                state.reminder_event_ids
                    .insert(resp.response.event_id.to_string(), unit_name.clone());
                state.save(&ctx.state_path).await?;
                sent.push(unit_name.clone());
            }
            Err(e) => tracing::warn!("!remind send failed for {unit_name}: {e}"),
        }
    }

    Ok(Some(format::mentionify(&format!(
        "✅ Reminder sent for: {}",
        sent.join(", ")
    ))))
}

// ── !leaderboard ──────────────────────────────────────────────────────────────

async fn cmd_leaderboard(ctx: &BotContext) -> Result<Option<String>> {
    let state    = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    let (cur_y, cur_w) = current_iso_week();

    let units = state.units();
    if units.is_empty() {
        return Ok(Some("No floors configured yet.".into()));
    }

    // Collect every unique user across all floors.
    let mut all_users: Vec<String> = Vec::new();
    for unit in &units {
        for u in &unit.users {
            if !all_users.contains(u) { all_users.push(u.clone()); }
        }
    }
    if all_users.is_empty() {
        return Ok(Some("No users assigned to any floor.".into()));
    }

    let due_weeks  = state.all_due_weeks(interval, (cur_y, cur_w));
    let closed_due: Vec<_> = due_weeks.iter()
        .filter(|&&(y, w)| (y, w) != (cur_y, cur_w))
        .collect();
    let n_due = closed_due.len();

    struct Entry { user: String, done: usize, skips: usize, streak: u32, pct: usize }

    let mut board: Vec<Entry> = all_users.into_iter().map(|user| {
        let done = state.completions.iter()
            .filter(|c| c.completed_by == user && !c.skipped)
            .count();
        let skips = state.completions.iter()
            .filter(|c| c.completed_by == user && c.skipped)
            .count();
        let streak = state.unit_for_user(&user)
            .map(|u| state.streak_for(&u.name, interval))
            .unwrap_or(0);
        let pct = if n_due > 0 { 100 * done.min(n_due) / n_due } else { 100 };
        Entry { user, done, skips, streak, pct }
    }).collect();

    board.sort_by(|a, b| b.pct.cmp(&a.pct).then(b.streak.cmp(&a.streak)));

    let mut lines = vec!["🏆 Cleaning Leaderboard".to_owned(), String::new()];
    for (i, e) in board.iter().enumerate() {
        let medal  = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " };
        let streak = if e.streak >= 2 { format!("  🔥{}", e.streak) } else { String::new() };
        let skip_s = if e.skips > 0 { format!("  ⏭️{}", e.skips) } else { String::new() };
        lines.push(format!(
            "{medal} {:>2}. {} — {}/{} ({:>3}%){}{}",
            i + 1, e.user, e.done, n_due, e.pct, streak, skip_s
        ));
    }
    Ok(Some(lines.join("\n")))
}

// ── !absent @user [weeks] ─────────────────────────────────────────────────────

async fn cmd_absent(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let user = match args.first() {
        Some(u) => u.to_string(),
        None    => return Ok(Some("Usage: !absent @user [weeks]  (default 4 weeks; use !back @user to cancel early)".into())),
    };
    let weeks: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4);

    let mut state = ctx.state.lock().await;

    let unit = match state.unit_for_user(&user) {
        Some(u) => u,
        None    => return Ok(Some(format!("{user} is not assigned to any floor."))),
    };

    // Replace any existing absence for this user/floor.
    state.absences.retain(|a| a.user_id != user || a.floor_name != unit.name);

    let (from_year, from_week) = current_iso_week();
    state.absences.push(Absence {
        user_id:        user.clone(),
        floor_name:     unit.name.clone(),
        from_year,
        from_week,
        duration_weeks: weeks,
    });
    state.save(&ctx.state_path).await?;

    let end = add_weeks(from_year, from_week, weeks as i64);
    Ok(Some(format!(
        "🌴 {user} is away from «{}» for {weeks} week{} (back week {ew}, {edates}). \
         Use !back {user} to cancel early.",
        unit.name,
        if weeks == 1 { "" } else { "s" },
        ew = end.1,
        edates = week_dates(end.0, end.1),
    )))
}

// ── !back @user ───────────────────────────────────────────────────────────────

async fn cmd_back(ctx: &BotContext, sender: &OwnedUserId, args: &[&str]) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let user = match args.first() {
        Some(u) => u.to_string(),
        None    => return Ok(Some("Usage: !back @user".into())),
    };

    let mut state = ctx.state.lock().await;
    let before = state.absences.len();
    state.absences.retain(|a| a.user_id != user);
    if state.absences.len() == before {
        return Ok(Some(format!("{user} has no active absence.")));
    }
    state.save(&ctx.state_path).await?;
    Ok(Some(format!("✅ {user} is back — absence cancelled.")))
}

// ── !blame / !blame @user / !blame <floor> ────────────────────────────────────

async fn cmd_blame(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let (cur_y, cur_w) = current_iso_week();
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;

    Ok(Some(match args.first() {
        None          => blame_all(&state, cur_y, cur_w, interval),
        Some(arg) if arg.starts_with('@') => blame_user(&state, arg, interval),
        Some(arg)     => blame_unit(&state, arg, cur_y, cur_w, interval),
    }))
}

/// !blame — all units due this week but not yet cleaned.
fn blame_all(state: &crate::state::State, year: i32, week: u32, interval: u32) -> String {
    let units = state.units();
    let uncleaned: Vec<_> = units.iter()
        .filter(|u| state.is_due(&u.name, year, week, interval)
                 && !state.is_completed(&u.name, year, week))
        .collect();

    if uncleaned.is_empty() {
        return "✅ All due units are cleaned this week — nothing to blame!".into();
    }

    let mut lines = vec![format!("😤 Blame — week {week} ({})", week_dates(year, week))];
    for unit in uncleaned {
        let users = if unit.users.is_empty() {
            "(nobody assigned)".into()
        } else {
            unit.users.join(", ")
        };
        let n_due    = state.all_due_weeks(interval, (year, week)).len();
        let n_missed = state.missed_weeks_for(&unit.name, interval).len();
        let streak   = state.streak_for(&unit.name, interval);
        lines.push(String::new());
        lines.push(format!("❌ {}", unit.name));
        lines.push(format!("   Responsible: {users}"));
        lines.push(format!("   Streak: {streak}  │  Missed: {n_missed} of {n_due} weeks"));
    }
    lines.join("\n")
}

/// !blame @user — personal cleaning record for one user.
fn blame_user(state: &crate::state::State, user: &str, interval: u32) -> String {
    let (cur_y, cur_w) = current_iso_week();

    let unit = match state.unit_for_user(user) {
        Some(u) => u,
        None    => return format!("{user} is not assigned to any floor."),
    };

    let due_weeks   = state.all_due_weeks(interval, (cur_y, cur_w));
    let closed_due: Vec<_> = due_weeks.iter()
        .filter(|&&(y, w)| (y, w) != (cur_y, cur_w))
        .collect();
    let n_due = closed_due.len();

    let n_done_by_user = state.completions.iter()
        .filter(|c| c.unit_name == unit.name && c.completed_by == user)
        .count()
        .min(n_due);
    let n_done_by_anyone = closed_due.iter()
        .filter(|(y, w)| state.is_completed(&unit.name, *y, *w))
        .count();
    let n_missed        = n_due - n_done_by_anyone;
    let n_someone_else  = n_done_by_anyone.saturating_sub(n_done_by_user);
    let pct = if n_due > 0 { 100 * n_done_by_user / n_due } else { 100 };
    let streak = state.streak_for(&unit.name, interval);

    let mut lines = vec![format!("😤 Blame for {user}")];
    lines.push(String::new());
    lines.push(format!("Floor: {}", unit.name));
    lines.push(format!("Personally cleaned: {n_done_by_user}/{n_due} due weeks ({pct}%)"));
    if n_someone_else > 0 {
        lines.push(format!("Covered by others: {n_someone_else}"));
    }
    if n_missed > 0 {
        lines.push(format!("Not cleaned at all: {n_missed}"));
    }
    lines.push(format!("Streak: {streak}"));

    if let Some(last) = state.completions.iter()
        .filter(|c| c.unit_name == unit.name && c.completed_by == user)
        .max_by_key(|c| c.completed_at)
    {
        lines.push(format!(
            "Last cleaned by them: week {w} ({})",
            week_dates(last.iso_year, last.iso_week), w = last.iso_week,
        ));
    }

    let missed = state.missed_weeks_for(&unit.name, interval);
    if !missed.is_empty() {
        let shown: Vec<String> = missed.iter().take(5)
            .map(|(y, w)| format!("w{w} ({})", week_dates(*y, *w))).collect();
        let suffix = if missed.len() > 5 {
            format!(" (+{})", missed.len() - 5)
        } else {
            String::new()
        };
        lines.push(String::new());
        lines.push(format!("Weeks nobody cleaned: {}{suffix}", shown.join(", ")));
    }

    lines.join("\n")
}

/// !blame <floor> — cleaning record for a specific unit.
fn blame_unit(state: &crate::state::State, unit_name: &str, year: i32, week: u32, interval: u32) -> String {
    let units = state.units();
    let unit  = match units.iter().find(|u| u.name.eq_ignore_ascii_case(unit_name)) {
        Some(u) => u,
        None    => return format!("Unit «{unit_name}» not found."),
    };

    let due_weeks  = state.all_due_weeks(interval, (year, week));
    let closed_due: Vec<_> = due_weeks.iter()
        .filter(|&&(y, w)| (y, w) != (year, week))
        .collect();
    let n_due    = closed_due.len();
    let n_done   = closed_due.iter()
        .filter(|(y, w)| state.is_completed(&unit.name, *y, *w))
        .count();
    let n_missed = n_due - n_done;
    let pct      = if n_due > 0 { 100 * n_done / n_due } else { 100 };
    let streak   = state.streak_for(&unit.name, interval);
    let this_week = state.is_completed(&unit.name, year, week);
    let users    = if unit.users.is_empty() {
        "(nobody assigned)".into()
    } else {
        unit.users.join(", ")
    };

    let mut lines = vec![format!("😤 Blame for {}", unit.name)];
    lines.push(String::new());
    lines.push(format!("Responsible: {users}"));
    lines.push(format!("Completed: {n_done}/{n_due} ({pct}%)  │  Missed: {n_missed}"));
    lines.push(format!("Streak: {streak}  │  This week: {}", if this_week { "✅" } else { "❌" }));

    if let Some(last) = state.last_completed(&unit.name) {
        lines.push(format!(
            "Last cleaned: week {w} ({dates}) by {by}",
            w = last.iso_week,
            dates = week_dates(last.iso_year, last.iso_week),
            by = last.completed_by,
        ));
    }

    // Per-user breakdown when multiple users share a floor
    if unit.users.len() > 1 {
        lines.push(String::new());
        lines.push("Per user:".into());
        for user in &unit.users {
            let cnt = state.completions.iter()
                .filter(|c| c.unit_name == unit.name && c.completed_by.as_str() == user.as_str())
                .count();
            lines.push(format!("  {user}: {cnt} cleanings"));
        }
    }

    let missed = state.missed_weeks_for(&unit.name, interval);
    if !missed.is_empty() {
        let shown: Vec<String> = missed.iter().take(5)
            .map(|(y, w)| format!("w{w} ({})", week_dates(*y, *w))).collect();
        let suffix = if missed.len() > 5 {
            format!(" (+{})", missed.len() - 5)
        } else {
            String::new()
        };
        lines.push(String::new());
        lines.push(format!("Missed weeks: {}{suffix}", shown.join(", ")));
    }

    lines.join("\n")
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn require_admin(ctx: &BotContext, sender: &OwnedUserId) -> Result<()> {
    if ctx.admin_users.contains(sender) {
        Ok(())
    } else {
        Err(anyhow::anyhow!("__not_admin__"))
    }
}

fn help_text() -> String {
    r#"🧹 Cleaning bot commands:

  !help                        — show this help
  !status                      — this week's cleaned / not-cleaned overview
  !floors                      — list all floors, groups and their users
  !done [floor_or_group]       — mark your cleaning as done for this week
  !undo [floor_or_group]       — undo this week's done mark
                                 (also fires automatically when you remove a ✅ reaction)
  !next [@user]                — when is your (or @user's) next due week?
  !stats [@user]               — completion statistics; add @user for one person
  !leaderboard                 — overall cleaning leaderboard with streaks
  !cleanplan [N]               — show the next N due cleaning weeks (default 6)
  !blame                       — show all units due but not cleaned this week
  !blame @user                 — cleaning record for one user
  !blame <floor>               — cleaning record for a specific floor
  !joinfloor <floor>           — add yourself to a floor
  !leavefloor <floor>          — remove yourself from a floor
  !swap @user [floor] [week <N>] — propose a cleaning swap; omit week for current,
                                 or add e.g. `week 24` to swap a future week;
                                 they must accept with !acceptswap
  !acceptswap <id>             — accept a pending swap request
  !rejectswap <id>             — reject a pending swap request

Admin commands:
  !skip [floor]                         — excuse this week (won't count as missed)
  !remind [floor]                       — manually fire the cleaning reminder now
  !absent @user [weeks]                 — mark user away for N weeks (default 4)
  !back @user                           — cancel an absence early
  !addfloor <name>                      — create a new floor
  !removefloor <name>                   — delete a floor
  !adduser @user <floor>                — assign a user to a floor
  !removeuser @user <floor>             — unassign a user from a floor
  !addroom <floor> <room>               — add a room to clean on a floor
  !removeroom <floor> <room>            — remove a room from a floor
  !linkfloors <group> <floor1> [floor2…]— group floors onto one shared schedule
  !unlinkfloors <group>                 — dissolve a floor group"#.to_owned()
}
