//! Statistics shown by !stats: leaderboard, fairness, load, group records.

use super::*;

// ── !stats ─────────────────────────────────────────────────────────────

pub(crate) async fn cmd_leaderboard(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let board = analytics::global_leaderboard(&state);
    if board.is_empty() {
        return Ok(Some("No members assigned to any group yet.".into()));
    }

    let mut lines = vec!["🏆 Cleaning Leaderboard".to_owned(), String::new()];
    for (i, ps) in board.iter().enumerate() {
        let medal = match i {
            0 => "🥇",
            1 => "🥈",
            2 => "🥉",
            _ => "  ",
        };
        let streak = if ps.streak >= 2 {
            format!("  🔥{}", ps.streak)
        } else {
            String::new()
        };
        let skips = if ps.skipped > 0 {
            format!("  ⏭️{}", ps.skipped)
        } else {
            String::new()
        };
        let pct = (ps.completion_rate * 100.0).round() as u32;
        lines.push(format!(
            "{medal} {}  {}/{} ({}%){streak}{skips}",
            ps.display_name, ps.completed, ps.due_weeks, pct
        ));
    }
    Ok(Some(lines.join("\n")))
}

// ── !stats fairness [group] ─────────────────────────────────────────────────────────

pub(crate) async fn cmd_fairness(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let (_, start_w) = state.tracking_start();

    let groups: Vec<crate::domain::GroupId> = if let Some(name) = args.first() {
        match state.group_by_name(name) {
            Some(g) => vec![g.id.clone()],
            None => return Ok(Some(format!("Group «{name}» not found."))),
        }
    } else {
        state
            .cleaning_groups
            .iter()
            .filter(|g| g.is_active)
            .map(|g| g.id.clone())
            .collect()
    };

    if groups.is_empty() {
        return Ok(Some("No cleaning groups configured yet.".into()));
    }

    let mut out = Vec::new();
    for (i, group_id) in groups.iter().enumerate() {
        let Some(report) = analytics::fairness_report(&state, group_id) else {
            continue;
        };
        if i > 0 {
            out.push(String::new());
        }
        out.push(format!(
            "⚖️ **{}** · {} turns · since W{start_w}",
            report.group_name, report.due_weeks,
        ));
        for e in &report.entries {
            let (pct, _) = load_delta_pct(e.actual_load, e.expected_load);
            let icon = load_icon(e.actual_load, e.expected_load);
            out.push(format!(
                "{icon} **{}**  {}/{:.0} ({})",
                e.display_name, e.actual, e.expected as u32, pct,
            ));
        }
        out.push(format!("Score {}/100", report.fairness_score));
    }

    if out.is_empty() {
        return Ok(Some(
            "No history yet — run some cleaning cycles first.".into(),
        ));
    }
    Ok(Some(out.join("\n")))
}

// ── !stats load ─────────────────────────────────────────────────────────────────

pub(crate) async fn cmd_workload(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let report = analytics::workload_report(&state);

    if report.entries.is_empty() {
        return Ok(Some("No members assigned to any group yet.".into()));
    }

    let yrs = report.years_tracked;
    let has_history = report.due_weeks >= 4;
    let header = if has_history {
        format!("🏋️ **Load** · {} wks · {:.2} yr", report.due_weeks, yrs)
    } else {
        format!(
            "🏋️ **Expected load** (structural · {} wks tracked)",
            report.due_weeks
        )
    };
    let mut out = vec![header];

    // Normalize against house average so 1.0× = average resident.
    let avg_expected = {
        let sum: f64 = report.entries.iter().map(|e| e.expected_cli_per_year).sum();
        let n = report.entries.len() as f64;
        if n > 0.0 && sum > 0.0 {
            sum / n
        } else {
            1.0
        }
    };

    for e in &report.entries {
        let ratio = e.expected_cli_per_year / avg_expected;
        let ratio_str = format!("{:.2}×", ratio);

        let line = if has_history {
            let (pct, _) = load_delta_pct(e.actual_cli_per_year, e.expected_cli_per_year);
            let icon = load_icon(e.actual_cli_per_year, e.expected_cli_per_year);
            format!(
                "{icon} **{}**  {} · {} · {}",
                e.display_name,
                pct,
                ratio_str,
                e.group_names.join(", "),
            )
        } else {
            format!(
                "**{}**  {} · {}",
                e.display_name,
                ratio_str,
                e.group_names.join(", "),
            )
        };
        out.push(line);
    }

    if has_history {
        let most = report
            .most_loaded
            .first()
            .map(String::as_str)
            .unwrap_or("-");
        let least = report
            .least_loaded
            .first()
            .map(String::as_str)
            .unwrap_or("-");
        if most != least {
            out.push(format!("⬆ {most} · ⬇ {least}"));
        }
    }

    Ok(Some(out.join("\n")))
}

// ── !stats load ───────────────────────────────────────────────────────────────

pub(crate) async fn cmd_groupstats(ctx: &BotContext) -> Result<Option<String>> {
    let state = ctx.state.lock().await;

    let active: Vec<_> = state
        .cleaning_groups
        .iter()
        .filter(|g| g.is_active)
        .collect();
    if active.is_empty() {
        return Ok(Some("No active cleaning groups configured.".into()));
    }

    let models: Vec<_> = active
        .iter()
        .map(|g| analytics::group_load_model(g))
        .collect();

    let avg_cli = {
        let sum: f64 = models.iter().map(|m| m.cli_per_year).sum();
        let n = models.len() as f64;
        if n > 0.0 && sum > 0.0 {
            sum / n
        } else {
            1.0
        }
    };

    let mut out = vec!["📊 **Groups**  (1.0× = avg load/person/yr)".to_owned()];

    for (group, m) in active.iter().zip(models.iter()) {
        let ratio = m.cli_per_year / avg_cli;
        let weight = if (m.group_weight - 1.0).abs() > 0.01 {
            format!(" · ×{:.1}", m.group_weight)
        } else {
            String::new()
        };
        out.push(format!(
            "**{}**  {:.2}× · {}p · {} · {:.0}r{}",
            group.name, ratio, m.member_count, m.rhythm, m.rooms_per_assignment, weight,
        ));
        for slot in &group.slots {
            let sr = analytics::effective_rooms_pub(&slot.room_names, &slot.room_weights);
            let sw = if (slot.weight - 1.0).abs() > 0.01 {
                format!(" ×{:.1}", slot.weight)
            } else {
                String::new()
            };
            out.push(format!("  └ {}  {:.0}r{}", slot.name, sr, sw));
        }
        let mut room_weights: Vec<String> = if group.slots.is_empty() {
            group
                .room_weights
                .iter()
                .map(|(r, w)| format!("{r} ×{w:.1}"))
                .collect()
        } else {
            vec![]
        };
        room_weights.sort();
        if !room_weights.is_empty() {
            out.push(format!("  weights: {}", room_weights.join(", ")));
        }
    }

    Ok(Some(out.join("\n")))
}

// ── Group record (!stats <group>) ─────────────────────────────────────────────

pub(crate) fn blame_group(state: &crate::state::State, group: &CleaningGroup) -> String {
    let closed = state.closed_turns(group);
    let n_due = closed.len();
    let n_done = closed
        .iter()
        .filter(|t| state.is_turn_done(group, **t))
        .count();
    let pct = (100 * n_done).checked_div(n_due).unwrap_or(100);
    let streak = state.streak_for(group);
    let (year, week) = current_iso_week();
    let this_week: Vec<String> = state
        .turns_in_week(group, year, week)
        .into_iter()
        .map(|t| {
            let icon = if state.is_turn_done(group, t) {
                "✅"
            } else {
                "⬜"
            };
            match t.shift_label(&group.rhythm) {
                Some(label) => format!("{label} {icon}"),
                None => icon.to_owned(),
            }
        })
        .collect();
    let members_text = state
        .members_of(group)
        .iter()
        .map(|p| p.display_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let mut lines = vec![
        format!(
            "📊 **{}** · cleaned {}",
            group.name,
            group.rhythm.describe()
        ),
        String::new(),
    ];
    lines.push(format!("Members: {members_text}"));
    lines.push(format!(
        "Turns done: {n_done}/{n_due} ({pct}%) · Streak: {streak} · This week: {}",
        if this_week.is_empty() {
            "not due".to_owned()
        } else {
            this_week.join(" ")
        }
    ));
    if let Some(last) = state.last_completion(&group.id) {
        let by = state
            .person_by_id(&last.completed_by_id)
            .map(|p| p.display_name.as_str())
            .unwrap_or("?");
        let turn = Turn::new(last.iso_year, last.iso_week, last.shift);
        lines.push(format!(
            "Last: week {} ({}) by {by}",
            last.iso_week,
            turn.period_label(&group.rhythm)
        ));
    }
    let missed = state.missed_turns(group);
    if !missed.is_empty() {
        let shown: Vec<_> = missed
            .iter()
            .rev()
            .take(5)
            .map(|t| format!("w{} ({})", t.week, t.period_label(&group.rhythm)))
            .collect();
        lines.push(format!(
            "Missed: {}{}",
            shown.join(", "),
            if missed.len() > 5 {
                format!(" (+{})", missed.len() - 5)
            } else {
                String::new()
            }
        ));
    }
    lines.join("\n")
}
