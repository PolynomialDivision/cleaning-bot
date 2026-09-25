//! Read-only overviews: !status, !groups, !stats.

use super::*;
use crate::state::{Completion, State};

/// Name shown in overviews — never a mention, so looking at the status does
/// not ping anyone.
fn name(person: &Person) -> String {
    match &person.matrix_id {
        Some(_) => person.display_name.clone(),
        None => format!("{} (no Matrix)", person.display_name),
    }
}

fn away_marker(
    state: &State,
    person: &Person,
    group_id: &GroupId,
    year: i32,
    week: u32,
) -> &'static str {
    if state.is_absent(&person.id, group_id, year, week) {
        " 🌴 away"
    } else {
        ""
    }
}

/// One status line per task: one per slot in a group with slots (several
/// people clean the same week, each with their own done/open state), one
/// for the whole group otherwise.
pub(crate) fn week_task_lines(
    state: &State,
    group: &CleaningGroup,
    year: i32,
    week: u32,
    interval: u32,
) -> Vec<(bool, String)> {
    let line = |label: Option<&str>, assignee: Option<&Person>, completion: Option<&Completion>| {
        let prefix = label.map(|l| format!("{l} · ")).unwrap_or_default();
        let who = assignee
            .map(name)
            .unwrap_or_else(|| "nobody assigned".into());
        match completion {
            Some(c) if c.skipped => (true, format!("⏭️ {prefix}{who} · skipped")),
            Some(c) => {
                let by = (assignee.map(|p| &p.id) != Some(&c.completed_by_id))
                    .then(|| state.person_by_id(&c.completed_by_id))
                    .flatten()
                    .map(|p| format!(" (done by {})", p.display_name))
                    .unwrap_or_default();
                (true, format!("✅ {prefix}{who}{by}"))
            }
            None => {
                let away = assignee
                    .map(|p| away_marker(state, p, &group.id, year, week))
                    .unwrap_or_default();
                (false, format!("⬜ {prefix}{who}{away}"))
            }
        }
    };
    let completion_for = |slot_id: Option<&str>| {
        state.completions.iter().find(|c| {
            c.group_id == group.id
                && (c.iso_year, c.iso_week) == (year, week)
                && (slot_id.is_none() || c.slot_id.as_deref() == slot_id)
        })
    };

    if group.is_multi_slot() {
        group
            .slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                line(
                    Some(&slot.name),
                    state.slot_assignee(group, i, year, week, interval),
                    completion_for(Some(&slot.id)),
                )
            })
            .collect()
    } else {
        vec![line(
            None,
            state.responsible_person(group, year, week, interval),
            completion_for(None),
        )]
    }
}

// ── !status ───────────────────────────────────────────────────────────────────

pub(crate) async fn cmd_status(ctx: &BotContext) -> Result<Option<String>> {
    let (year, week) = current_iso_week();
    let state = ctx.state.lock().await;
    Ok(Some(status_text(
        &state,
        year,
        week,
        ctx.config.schedule.interval_weeks,
    )))
}

pub(crate) fn status_text(state: &State, year: i32, week: u32, interval: u32) -> String {
    let active: Vec<_> = state
        .cleaning_groups
        .iter()
        .filter(|g| g.is_active)
        .collect();
    if active.is_empty() {
        return "No active cleaning groups configured yet.".into();
    }

    let mut body = Vec::new();
    let mut not_due = Vec::new();
    let (mut done, mut total) = (0, 0);
    for group in active {
        if !state.belongs_in_weekly_plan(&group.id, year, week, interval) {
            not_due.push(group.name.as_str());
            continue;
        }
        body.push(String::new());
        body.push(format!("**{}**", group.name));
        for (is_done, line) in week_task_lines(state, group, year, week, interval) {
            total += 1;
            done += usize::from(is_done);
            body.push(line);
        }
    }

    let mut lines = vec![format!(
        "📋 **Week {week}** ({}) · {done} of {total} done",
        week_dates(year, week)
    )];
    lines.extend(body);
    if !not_due.is_empty() {
        lines.push(String::new());
        lines.push(format!("Not due this week: {}", not_due.join(", ")));
    }
    lines.join("\n")
}

// ── !groups [group] ───────────────────────────────────────────────────────────

pub(crate) async fn cmd_groups(
    ctx: &BotContext,
    group_name: Option<&str>,
) -> Result<Option<String>> {
    let state = ctx.state.lock().await;
    let interval = ctx.config.schedule.interval_weeks;
    Ok(Some(match group_name {
        None => groups_text(&state),
        Some(name) => match state.group_by_name(name) {
            Some(group) => group_detail_text(&state, group, interval),
            None => format!("Group «{name}» not found. !groups lists all groups."),
        },
    }))
}

fn rooms_summary(group: &CleaningGroup) -> Option<String> {
    if group.is_multi_slot() {
        let slots: Vec<String> = group
            .slots
            .iter()
            .map(|slot| {
                if slot.room_names.is_empty() {
                    slot.name.clone()
                } else {
                    format!("{} ({})", slot.name, slot.room_names.join(", "))
                }
            })
            .collect();
        Some(format!("Slots: {}", slots.join(" · ")))
    } else {
        group.rooms_text()
    }
}

/// Every group with its members at a glance.
pub(crate) fn groups_text(state: &State) -> String {
    if state.cleaning_groups.is_empty() {
        return "No cleaning groups configured yet.".into();
    }
    let (year, week) = current_iso_week();
    let active: Vec<_> = state
        .cleaning_groups
        .iter()
        .filter(|g| g.is_active)
        .collect();
    let mut lines = vec![format!("🏢 **Cleaning groups** ({})", active.len())];
    for group in active {
        let members = state.members_of(group);
        let members_text = if members.is_empty() {
            "no members".to_owned()
        } else {
            members
                .iter()
                .map(|p| {
                    format!(
                        "{}{}",
                        name(p),
                        away_marker(state, p, &group.id, year, week)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        lines.push(String::new());
        lines.push(format!(
            "**{}** ({}) · {members_text}",
            group.name,
            members.len()
        ));
        if let Some(rooms) = rooms_summary(group) {
            lines.push(rooms);
        }
    }
    let disabled: Vec<&str> = state
        .cleaning_groups
        .iter()
        .filter(|g| !g.is_active)
        .map(|g| g.name.as_str())
        .collect();
    if !disabled.is_empty() {
        lines.push(String::new());
        lines.push(format!("🚫 Disabled: {}", disabled.join(", ")));
    }
    lines.join("\n")
}

/// One group in detail: rotation order, this week, next turn, rooms, weights.
pub(crate) fn group_detail_text(state: &State, group: &CleaningGroup, interval: u32) -> String {
    let (year, week) = current_iso_week();
    let mut header = format!("🏢 **{}**", group.name);
    if !group.is_active {
        header.push_str(" · 🚫 disabled");
    }
    if (group.weight - 1.0).abs() > 0.01 {
        header.push_str(&format!(" · weight ×{:.1}", group.weight));
    }
    let mut lines = vec![header];

    let queue = resolver::reconcile_queue(state, group);
    if queue.is_empty() {
        lines.push("No members yet.".into());
    } else {
        lines.push("Members, next unplanned turn first:".into());
        for (i, pid) in queue.iter().enumerate() {
            let label = state
                .person_by_id(pid)
                .map(|p| {
                    format!(
                        "{}{}",
                        person_label(p),
                        away_marker(state, p, &group.id, year, week)
                    )
                })
                .unwrap_or_else(|| format!("unknown ({pid})"));
            lines.push(format!("{}. {label}", i + 1));
        }
    }

    if group.is_active && state.belongs_in_weekly_plan(&group.id, year, week, interval) {
        lines.push(format!("This week (week {week}):"));
        lines.extend(
            week_task_lines(state, group, year, week, interval)
                .into_iter()
                .map(|(_, line)| line),
        );
    }
    if !queue.is_empty() {
        lines.push(next_assignment_summary(state, &group.id));
    }

    if group.is_multi_slot() {
        lines.push("Slots:".into());
        for slot in &group.slots {
            let rooms = if slot.room_names.is_empty() {
                "no rooms".to_owned()
            } else {
                slot.room_names.join(", ")
            };
            let weight = if (slot.weight - 1.0).abs() > 0.01 {
                format!(" · ×{:.1}", slot.weight)
            } else {
                String::new()
            };
            lines.push(format!("• {} · {rooms}{weight}", slot.name));
        }
    } else if let Some(rooms) = group.rooms_text() {
        lines.push(rooms);
    }
    let mut room_weights: Vec<String> = group
        .room_weights
        .iter()
        .chain(group.slots.iter().flat_map(|s| s.room_weights.iter()))
        .filter(|(_, w)| (**w - 1.0).abs() > 0.01)
        .map(|(room, w)| format!("{room} ×{w:.1}"))
        .collect();
    room_weights.sort();
    if !room_weights.is_empty() {
        lines.push(format!("Room weights: {}", room_weights.join(", ")));
    }
    lines.join("\n")
}

// ── !stats [person | group | fairness | load] ─────────────────────────────────

pub(crate) async fn cmd_stats_overview(ctx: &BotContext, args: &[&str]) -> Result<Option<String>> {
    let sub = args.first().map(|a| a.to_ascii_lowercase());
    match sub.as_deref() {
        None => {
            let board = cmd_leaderboard(ctx).await?.unwrap_or_default();
            let state = ctx.state.lock().await;
            let groups = group_completion_lines(&state, ctx.config.schedule.interval_weeks);
            Ok(Some(if groups.is_empty() {
                board
            } else {
                format!("{board}\n\n{groups}")
            }))
        }
        Some("fairness") => cmd_fairness(ctx, &args[1..]).await,
        Some("load") => {
            let workload = cmd_workload(ctx).await?.unwrap_or_default();
            let groups = cmd_groupstats(ctx).await?.unwrap_or_default();
            Ok(Some(format!("{workload}\n\n{groups}")))
        }
        Some(_) => {
            let query = args.join(" ");
            let interval = ctx.config.schedule.interval_weeks;
            let group = {
                let state = ctx.state.lock().await;
                state.group_by_name(&query).cloned().map(|group| {
                    let (year, week) = current_iso_week();
                    blame_group(&state, &group, year, week, interval)
                })
            };
            match group {
                Some(record) => {
                    let fairness = cmd_fairness(ctx, &[query.as_str()])
                        .await?
                        .unwrap_or_default();
                    Ok(Some(format!("{record}\n\n{fairness}")))
                }
                None => cmd_stats(ctx, &[query.as_str()]).await,
            }
        }
    }
}

/// One line per active group: completion rate, streak, this week.
fn group_completion_lines(state: &State, interval: u32) -> String {
    let (year, week) = current_iso_week();
    let lines: Vec<String> = state
        .cleaning_groups
        .iter()
        .filter(|g| g.is_active)
        .filter_map(|group| {
            let gs = analytics::group_stats(state, &group.id, interval)?;
            let pct = (gs.completion_rate * 100.0).round() as u32;
            let this_week = if state.is_completed(&group.id, year, week) {
                "✅"
            } else {
                "⬜"
            };
            let missed = if gs.missed > 0 {
                format!(" · missed {}", gs.missed)
            } else {
                String::new()
            };
            Some(format!(
                "{this_week} **{}** · {}/{} ({pct}%) · streak {}{missed}",
                group.name, gs.completed, gs.due_weeks, gs.current_streak
            ))
        })
        .collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!("🏢 **Groups**\n{}", lines.join("\n"))
    }
}
