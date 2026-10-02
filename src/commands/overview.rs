//! Read-only overviews: !status, !groups, !stats.

use super::*;
use crate::state::State;

/// A person in an overview: a user pill that pings nobody (see
/// `view::user_link`).
fn name(person: &Person) -> String {
    crate::view::user_link(person)
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

/// One status line per slot of each turn of the week — "✅ Scharni: Alice",
/// "⬜ Thu–Sun: Carol ← now", "❌ Bob · missed" — so everyone sharing a week
/// (slots, shifts) shows with their own done/open state.
pub(crate) fn week_task_lines(
    state: &State,
    group: &CleaningGroup,
    year: i32,
    week: u32,
) -> Vec<(bool, String)> {
    let running = state.current_turn(group);
    let mut lines = Vec::new();
    for turn in state.turns_in_week(group, year, week) {
        let now = if group.rhythm.is_split() && Some(turn) == running {
            " ← now"
        } else {
            ""
        };
        for (slot_index, assignee) in state.turn_assignees(group, turn) {
            let what = crate::scheduler::duty_prefix(group, slot_index, turn);
            let who = assignee
                .map(name)
                .unwrap_or_else(|| "nobody assigned".into());
            let line = match state.completion_for(group, slot_index, turn) {
                Some(c) if c.skipped => (true, format!("⏭️ {what}{who} · skipped")),
                Some(c) => {
                    let by = (assignee.map(|p| &p.id) != Some(&c.completed_by_id))
                        .then(|| state.person_by_id(&c.completed_by_id))
                        .flatten()
                        .map(|p| format!(" · done by {}", name(p)))
                        .unwrap_or_default();
                    (true, format!("✅ {what}{who}{by}"))
                }
                None if state.turn_over(group, turn) => (false, format!("❌ {what}{who} · missed")),
                None => {
                    let away = assignee
                        .map(|p| away_marker(state, p, &group.id, year, week))
                        .unwrap_or_default();
                    (false, format!("⬜ {what}{who}{away}{now}"))
                }
            };
            lines.push(line);
        }
    }
    lines
}

/// Everyone on one due week of the group, compactly: "Koch (Scharni),
/// Paul (Colbe)", "Ann (Mon–Wed), Bob (Thu–Sun)", or just "Dave".
fn week_people(state: &State, group: &CleaningGroup, (y, w): (i32, u32)) -> String {
    let mut parts = Vec::new();
    for turn in state.turns_in_week(group, y, w) {
        for (slot_index, assignee) in state.turn_assignees(group, turn) {
            let who = assignee.map(name).unwrap_or_else(|| "nobody".into());
            let part: Vec<String> = turn
                .shift_label(&group.rhythm)
                .into_iter()
                .chain(group.slots.get(slot_index).map(|s| s.name.clone()))
                .collect();
            parts.push(if part.is_empty() {
                who
            } else {
                format!("{who} ({})", part.join(" · "))
            });
        }
    }
    parts.join(", ")
}

/// "Next week: Dave" / "Week 43: Dave" — the group's first due week after
/// `after`, with everyone on it.
pub(crate) fn next_week_line(
    state: &State,
    group: &CleaningGroup,
    after: (i32, u32),
) -> Option<String> {
    let next = state.next_due_week(group, add_weeks(after.0, after.1, 1));
    let people = week_people(state, group, next);
    if people.is_empty() {
        return None;
    }
    let when = if next == add_weeks(after.0, after.1, 1) {
        "Next week".to_owned()
    } else {
        format!("Week {}", next.1)
    };
    Some(format!("{when}: {people}"))
}

// ── !status ───────────────────────────────────────────────────────────────────

pub(crate) async fn cmd_status(ctx: &BotContext) -> Result<Option<String>> {
    let (year, week) = current_iso_week();
    let state = ctx.state.lock().await;
    Ok(Some(status_text(&state, year, week)))
}

pub(crate) fn status_text(state: &State, year: i32, week: u32) -> String {
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
        if !state.belongs_in_weekly_plan(group, year, week) {
            let (_, next) = state.next_due_week(group, (year, week));
            not_due.push(format!("{} (week {next})", group.name));
            continue;
        }
        body.push(String::new());
        if group.rhythm.is_split() || group.rhythm.every_weeks() > 1 {
            body.push(format!("**{}** · {}", group.name, group.rhythm.describe()));
        } else {
            body.push(format!("**{}**", group.name));
        }
        for (is_done, line) in week_task_lines(state, group, year, week) {
            total += 1;
            done += usize::from(is_done);
            body.push(line);
        }
        body.extend(next_week_line(state, group, (year, week)));
    }

    let mut lines = vec![format!(
        "📋 **{}** · {done}/{total} done",
        crate::view::week_label(year, week)
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
    Ok(Some(match group_name {
        None => groups_text(&state),
        Some(name) => match state.group_by_name(name) {
            Some(group) => group_detail_text(&state, group),
            None => format!("Group «{name}» not found. !groups lists all groups."),
        },
    }))
}

/// Every group with its members at a glance.
pub(crate) fn groups_text(state: &State) -> String {
    if state.cleaning_groups.is_empty() {
        return "No cleaning groups configured yet.".into();
    }
    let (year, week) = current_iso_week();
    let mut lines = vec!["🏢 **Groups** · !groups <name> for details".to_owned()];
    // Active groups first; disabled ones follow, marked 🚫 but still with
    // their members.
    let ordered = state
        .cleaning_groups
        .iter()
        .filter(|g| g.is_active)
        .chain(state.cleaning_groups.iter().filter(|g| !g.is_active));
    for group in ordered {
        let members = state.members_of(group);
        let mut header = if group.is_active {
            format!("**{}**", group.name)
        } else {
            format!("🚫 **{}** · disabled", group.name)
        };
        header.push_str(&format!(
            " · {}",
            crate::view::plural(members.len(), "member", "members")
        ));
        lines.push(String::new());
        lines.push(header);
        if !members.is_empty() {
            lines.push(
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
                    .join(", "),
            );
        }
    }
    lines.join("\n")
}

/// One group in detail: this week, what's coming, members, rooms, weights.
pub(crate) fn group_detail_text(state: &State, group: &CleaningGroup) -> String {
    let (year, week) = current_iso_week();
    let mut header = format!("🏢 **{}** · {}", group.name, group.rhythm.describe());
    if !group.is_active {
        header.push_str(" · 🚫 disabled");
    }
    if (group.weight - 1.0).abs() > 0.01 {
        header.push_str(&format!(" · weight ×{:.1}", group.weight));
    }
    let mut lines = vec![header];

    let weight_of = |w: f64| {
        if (w - 1.0).abs() > 0.01 {
            format!(" ×{w:.1}")
        } else {
            String::new()
        }
    };
    if group.slots.iter().all(|s| s.room_names.is_empty()) && group.is_multi_slot() {
        let slots: Vec<String> = group
            .slots
            .iter()
            .map(|s| format!("{}{}", s.name, weight_of(s.weight)))
            .collect();
        lines.push(format!("Slots: {}", slots.join(", ")));
    } else if group.is_multi_slot() {
        for slot in &group.slots {
            lines.push(format!(
                "{}{}: {}",
                slot.name,
                weight_of(slot.weight),
                slot.room_names.join(", ")
            ));
        }
    } else if !group.room_names.is_empty() {
        lines.push(group.room_names.join(", "));
    }

    if group.is_active && state.belongs_in_weekly_plan(group, year, week) {
        lines.push(String::new());
        lines.push(format!("**This week** · week {week}"));
        lines.extend(
            week_task_lines(state, group, year, week)
                .into_iter()
                .map(|(_, line)| line),
        );
    }
    if group.is_active && !group.member_ids.is_empty() {
        let mut upcoming = Vec::new();
        let mut at = (year, week);
        for _ in 0..4 {
            at = state.next_due_week(group, add_weeks(at.0, at.1, 1));
            upcoming.push(format!("Week {}: {}", at.1, week_people(state, group, at)));
        }
        lines.push(String::new());
        lines.push("**Coming up**".into());
        lines.extend(upcoming);
    }

    let members = state.members_of(group);
    lines.push(String::new());
    lines.push(format!("**Members** · {}", members.len()));
    if members.is_empty() {
        lines.push("No members yet — !member add or !join.".into());
    } else {
        lines.push(
            members
                .iter()
                .map(|p| {
                    let no_matrix = if p.matrix_id.is_none() {
                        " (no Matrix)"
                    } else {
                        ""
                    };
                    format!(
                        "{}{no_matrix}{}",
                        name(p),
                        away_marker(state, p, &group.id, year, week)
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
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
            let groups = group_completion_lines(&state);
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
            let group = {
                let state = ctx.state.lock().await;
                state
                    .group_by_name(&query)
                    .cloned()
                    .map(|group| blame_group(&state, &group))
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
fn group_completion_lines(state: &State) -> String {
    let (year, week) = current_iso_week();
    let lines: Vec<String> = state
        .cleaning_groups
        .iter()
        .filter(|g| g.is_active)
        .filter_map(|group| {
            let gs = analytics::group_stats(state, &group.id)?;
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
