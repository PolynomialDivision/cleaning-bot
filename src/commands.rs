use anyhow::Result;
use chrono::Utc;
use matrix_sdk::{
    ruma::{
        events::{
            relation::{Reply, Thread},
            room::message::{
                FileInfo, FileMessageEventContent, MessageType, Relation, RoomMessageEventContent,
            },
        },
        OwnedEventId, OwnedUserId, UInt,
    },
    Room,
};
use mxbot_common::matrix_sdk;
use uuid::Uuid;

use crate::{
    analytics::{self, DomainEvent},
    domain::{
        new_calendar_token, AssignmentSource, CalendarToken, CleaningGroup, GroupId, Person,
        PersonId,
    },
    format, resolver,
    schedule::build_schedule,
    scheduler,
    state::{add_weeks, current_iso_week, week_dates, weeks_between, SwapStatus},
    BotContext,
};

mod assignments;
mod exports;
mod helpers;
mod maintenance;
mod member;
mod overview;
mod rotation;
mod setup;
mod stats;
mod swaps;
#[cfg(test)]
mod tests;

use assignments::*;
use exports::*;
pub(crate) use helpers::*;
use maintenance::*;
use member::*;
use overview::*;
use rotation::*;
use setup::*;
use stats::*;
use swaps::*;

/// Shell-like tokenizer: splits on whitespace but keeps "quoted strings" together.
/// Quotes are stripped from the resulting tokens. Straight and typographic
/// double quotes (as phone keyboards insert them) both work; an apostrophe
/// is just a letter, so names like `Nick's` stay intact.
/// Example: `!groups room add "2. Stock" "Scharni Toilette"` → ["!groups", "room", "add", "2. Stock", "Scharni Toilette"]
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' | '“' | '”' | '„' => quoted = !quoted,
            ' ' | '\t' if !quoted => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
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
    let cmd_owned = if tokens.is_empty() {
        String::new()
    } else {
        tokens.remove(0)
    };
    let cmd = cmd_owned.as_str();
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
                if let Some(p) = state
                    .persons
                    .iter_mut()
                    .find(|p| p.matrix_id.as_deref() == Some(&sender_mxid))
                {
                    p.display_name = name.clone();
                }
            }
        }
    }

    // Multi-word group/person names work without quotes: regroup the tokens
    // so each name is a single argument before any command sees them.
    let normalized = {
        let state = ctx.state.lock().await;
        normalize_args(&state, cmd, &args)
    };
    let args: Vec<&str> = normalized.iter().map(String::as_str).collect();

    let sub = args.first().map(|a| a.to_ascii_lowercase());
    let sub = sub.as_deref();
    let rest = args.get(1..).unwrap_or_default();

    // Commands that need direct room access.
    match (cmd, sub) {
        ("!plan", None) => return cmd_cleanplan(ctx, sender, room, &args).await,
        ("!plan", Some(n)) if n.parse::<usize>().is_ok() => {
            return cmd_cleanplan(ctx, sender, room, &args).await
        }
        ("!plan", Some("remind")) => return cmd_remind(ctx, sender, room, rest).await,
        ("!plan", Some("announce")) => return cmd_announceweek(ctx, sender, room).await,
        ("!plan", Some("pdf")) => {
            return cmd_pdf(ctx, sender, room, rest, event_id, thread_root).await
        }
        ("!ical", Some("reset")) => return cmd_icalreset(ctx, sender, room, rest).await,
        ("!ical", _) => return cmd_ical(ctx, sender, room, &args).await,
        ("!member", Some("link")) => {
            let s = cmd_linkmatrix(ctx, sender, room, rest).await?;
            return Ok(s.map(RoomMessageEventContent::text_plain));
        }
        _ => {}
    }

    let reply: Option<String> = match (cmd, sub) {
        // ── Everyone ──
        ("!status", _) => cmd_status(ctx).await,
        ("!done", _) => cmd_done(ctx, sender, &args).await,
        ("!undo", _) => cmd_undo(ctx, sender, &args).await,
        ("!groups", None) => cmd_groups(ctx, None).await,
        ("!next", _) => cmd_next(ctx, sender, &args).await,
        ("!takeover", _) => cmd_takeover(ctx, sender, &args).await,
        ("!swap", Some("accept")) => cmd_acceptswap(ctx, sender, rest).await,
        ("!swap", Some("reject")) => cmd_rejectswap(ctx, sender, rest).await,
        ("!swap", _) => cmd_swap(ctx, sender, &args).await,
        // Old swap messages in the room still name these.
        ("!acceptswap", _) => cmd_acceptswap(ctx, sender, &args).await,
        ("!rejectswap", _) => cmd_rejectswap(ctx, sender, &args).await,
        ("!join", _) => cmd_joinfloor(ctx, sender, &args).await,
        ("!leave", _) => cmd_leavefloor(ctx, sender, &args).await,
        ("!stats", _) => cmd_stats_overview(ctx, &args).await,
        ("!help", Some("admin")) => Ok(Some(admin_help_text())),
        ("!help", _) => Ok(Some(help_text())),

        // ── Admin: plan ──
        ("!plan", Some("assign")) => cmd_assign(ctx, sender, rest).await,
        ("!plan", Some("unassign")) => cmd_unassign(ctx, sender, rest).await,
        ("!plan", Some("skip")) => cmd_skip(ctx, sender, rest).await,
        ("!plan", Some("reset")) => cmd_resetplan(ctx, sender, rest).await,
        ("!plan", Some("import")) => cmd_importplan(ctx, sender, rest).await,
        ("!plan", Some(_)) => Ok(Some(PLAN_USAGE.to_owned())),

        // ── Admin: members ──
        ("!member", Some("add")) => cmd_member_add(ctx, sender, rest).await,
        ("!member", Some("remove")) => cmd_member_remove(ctx, sender, rest).await,
        ("!member", Some("away")) => cmd_absent(ctx, sender, rest).await,
        ("!member", Some("back")) => cmd_back(ctx, sender, rest).await,
        ("!member", _) => Ok(Some(MEMBER_USAGE.to_owned())),

        // ── Admin: groups (the bare list is for everyone) ──
        ("!groups", Some("add")) => cmd_addfloor(ctx, sender, rest).await,
        ("!groups", Some("remove")) => cmd_removefloor(ctx, sender, rest).await,
        ("!groups", Some("enable")) => cmd_enablegroup(ctx, sender, rest).await,
        ("!groups", Some("disable")) => cmd_disablegroup(ctx, sender, rest).await,
        ("!groups", Some("weight")) => cmd_weight(ctx, sender, rest).await,
        ("!groups", Some("slot")) => cmd_groups_slot(ctx, sender, rest).await,
        ("!groups", Some("room")) => cmd_groups_room(ctx, sender, rest).await,
        ("!groups", Some(_)) => cmd_groups(ctx, Some(&args.join(" "))).await,
        ("!validate", _) => cmd_validate(ctx, sender).await,

        _ => Ok(renamed_command_hint(cmd)),
    }?;

    // Every mutation that can change who's responsible for, or the status
    // of, the running week's plan goes through this single refresh call —
    // the pinned Matrix message is re-rendered straight from `State`, never
    // patched in place, so it can never drift from the persisted domain state.
    if command_may_change_current_plan(cmd, sub) {
        let (year, week) = current_iso_week();
        scheduler::refresh_pinned_plan(ctx, room, year, week).await;
    }

    match reply {
        None => Ok(None),
        Some(s) => Ok(Some(format::mentionify_rich(&s, room).await)),
    }
}

// ── argument normalization ────────────────────────────────────────────────────

fn joined(args: &[&str]) -> Vec<String> {
    if args.is_empty() {
        Vec::new()
    } else {
        vec![args.join(" ")]
    }
}

/// `<group …> rest…`: the longest leading run of tokens that names a group
/// becomes one token, leaving at least `min_rest` tokens after it.
fn group_first(state: &crate::state::State, args: &[&str], min_rest: usize) -> Vec<String> {
    for n in (2..=args.len().saturating_sub(min_rest)).rev() {
        if let Some(group) = state.group_by_name(&args[..n].join(" ")) {
            let mut out = vec![group.name.clone()];
            out.extend(args[n..].iter().map(|a| a.to_string()));
            return out;
        }
    }
    args.iter().map(|a| a.to_string()).collect()
}

/// `<person …> <group …>`: the longest trailing run naming a group becomes
/// one token, everything before it the person.
fn person_then_group(state: &crate::state::State, args: &[&str]) -> Vec<String> {
    for start in 1..args.len() {
        if let Some(group) = state.group_by_name(&args[start..].join(" ")) {
            return vec![args[..start].join(" "), group.name.clone()];
        }
    }
    args.iter().map(|a| a.to_string()).collect()
}

/// `<person …> [N]`: a trailing number stays separate.
fn person_then_number(args: &[&str]) -> Vec<String> {
    match args.split_last() {
        Some((last, head)) if !head.is_empty() && last.parse::<u32>().is_ok() => {
            vec![head.join(" "), last.to_string()]
        }
        _ => joined(args),
    }
}

/// Regroup the tokens of one command so multi-word names need no quotes.
/// Quoted input still works — it simply already arrives as one token.
pub(crate) fn normalize_args(state: &crate::state::State, cmd: &str, args: &[&str]) -> Vec<String> {
    let owned = |a: &[&str]| a.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    let with_sub = |sub: &str, rest: Vec<String>| {
        let mut out = vec![sub.to_owned()];
        out.extend(rest);
        out
    };
    let sub = args.first().map(|a| a.to_ascii_lowercase());
    let rest = args.get(1..).unwrap_or_default();
    match (cmd, sub.as_deref()) {
        ("!done" | "!undo" | "!next" | "!join" | "!leave", _) => joined(args),
        ("!takeover", _) => group_first(state, args, 0),
        ("!swap", Some("accept" | "reject")) => owned(args),
        ("!swap", Some(_)) => {
            let mut out = vec![args[0].to_owned()];
            out.extend(group_first(state, rest, 0));
            out
        }
        ("!stats", Some("fairness")) => with_sub(args[0], joined(rest)),
        ("!stats", _) => joined(args),
        ("!ical", Some("reset")) => with_sub(args[0], joined(rest)),
        ("!ical", _) => person_then_number(args),
        ("!groups", Some("remove")) => match rest.split_last() {
            Some((last, head)) if !head.is_empty() && last.eq_ignore_ascii_case("confirm") => {
                let mut out = with_sub(args[0], joined(head));
                out.push(last.to_string());
                out
            }
            _ => with_sub(args[0], joined(rest)),
        },
        ("!groups", Some("add" | "enable" | "disable")) => with_sub(args[0], joined(rest)),
        ("!groups", Some("slot" | "room")) if !rest.is_empty() => {
            let mut out = vec![args[0].to_owned(), rest[0].to_owned()];
            let tail = group_first(state, &rest[1..], 1);
            if sub.as_deref() == Some("slot") {
                // <group> <slot name …>
                out.extend(tail.first().cloned());
                out.extend(joined(
                    &tail.iter().skip(1).map(String::as_str).collect::<Vec<_>>(),
                ));
            } else {
                out.extend(tail);
            }
            out
        }
        ("!groups", Some("weight")) => match rest.split_last() {
            // <group …> [room …] <factor>
            Some((factor, head)) if !head.is_empty() => {
                let head = group_first(state, head, 0);
                let mut out = vec![args[0].to_owned(), head[0].clone()];
                out.extend(joined(
                    &head[1..].iter().map(String::as_str).collect::<Vec<_>>(),
                ));
                out.push(factor.to_string());
                out
            }
            _ => owned(args),
        },
        ("!groups", Some(_)) => joined(args),
        ("!plan", Some("skip" | "assign" | "unassign")) => {
            with_sub(args[0], group_first(state, rest, 0))
        }
        ("!plan", Some("remind" | "reset")) => with_sub(args[0], joined(rest)),
        ("!member", Some("add" | "remove")) => with_sub(args[0], person_then_group(state, rest)),
        ("!member", Some("away")) => with_sub(args[0], person_then_number(rest)),
        ("!member", Some("back")) => with_sub(args[0], joined(rest)),
        ("!member", Some("link")) => match rest.split_last() {
            Some((mxid, head)) if !head.is_empty() => {
                vec![args[0].to_owned(), head.join(" "), mxid.to_string()]
            }
            _ => owned(args),
        },
        _ => owned(args),
    }
}

// ── help ─────────────────────────────────────────────────────────────────────

const PLAN_USAGE: &str = "Usage: !plan [N] | !plan assign|unassign|skip|remind|announce|pdf|reset|import … (see !help admin)";
const MEMBER_USAGE: &str = "Usage: !member add|remove <@user:server | name> <group> · !member link <name> <@user:server> · !member away <person> [weeks] · !member back <person>";

fn help_text() -> String {
    r#"🧹 **Cleaning bot**

!status · this week: who cleans what, done or open
!done [group] · mark your part done (or react ✅ on the plan)
!undo [group] · take back your done mark
!groups · all groups and their members
!plan [N] · the next N weeks (default 6)
!next [person] · when is your next turn?
!takeover [group] [slot] [week N] · take a task over yourself
!swap @user [group] [slot] [week N] · ask someone to swap · !swap accept|reject <id>
!join <group> · !leave <group>
!stats [person | group | fairness | load]
!ical [N] · calendar feed of your turns · !ical reset

Admins: !help admin"#
        .to_owned()
}

fn admin_help_text() -> String {
    r#"🔧 **Admin commands**

**Members**
!member add <@user:server | name> <group> · name = person without Matrix
!member remove <@user:server | name> <group>
!member link <name> <@user:server> · connect a name-only person to Matrix
!member away <person> [weeks] · skip in new rotation picks (default 4)
!member back <person>

**This week & plan**
!plan skip [group] [slot] · excuse this week (not counted as missed)
!plan remind [group] · send the reminder now
!plan announce · (re)post and pin this week's plan
!plan assign <group> [slot] <person> [week N]
!plan unassign <group> [slot] [week N]
!plan reset <group> · redistribute future weeks from the rotation
!plan import [--replace] <YYYY-Www> <group>[/slot] <person> [; …]
!plan pdf [N] [group] · printable plan

**Groups**
!groups <group> · details: turn order, slots, rooms, weights
!groups add|enable|disable <group>
!groups remove <group> [confirm] · deletes it with its history
!groups slot add|remove <group> <slot>
!groups room add|remove <group> [slot] <room>
!groups weight <group> [room] <factor>

**Other**
!ical <person> [N] · !ical reset <person>
!validate · check the saved state for problems
!admin · bot administration (verification, settings)"#
        .to_owned()
}

/// Commands that were renamed or merged: answer with the replacement instead
/// of silently ignoring the old name.
fn renamed_command_hint(cmd: &str) -> Option<String> {
    let new = match cmd {
        "!cleanplan" => "!plan [N]",
        "!areas" | "!listgroups" | "!floors" => "!groups",
        "!cleaning" => "!groups (list) or !member add|remove (changes)",
        "!joingroup" => "!join <group>",
        "!leavegroup" => "!leave <group>",
        "!icalreset" => "!ical reset",
        "!leaderboard" => "!stats",
        "!fairness" | "!planfairness" => "!stats fairness [group]",
        "!workload" | "!groupstats" => "!stats load",
        "!blame" => "!status (this week) or !stats <person | group>",
        "!adduser" | "!addperson" => "!member add <@user:server | name> <group>",
        "!removeuser" | "!removeperson" => "!member remove <@user:server | name> <group>",
        "!linkmatrix" => "!member link <name> <@user:server>",
        "!absent" => "!member away <person> [weeks]",
        "!back" => "!member back <person>",
        "!assign" => "!plan assign <group> [slot] <person> [week N]",
        "!unassign" => "!plan unassign <group> [slot] [week N]",
        "!importplan" => "!plan import …",
        "!resetplan" => "!plan reset <group>",
        "!skip" => "!plan skip [group]",
        "!remind" => "!plan remind [group]",
        "!announceweek" | "!repostplan" => "!plan announce",
        "!pdf" => "!plan pdf [N]",
        "!addgroup" => "!groups add <group>",
        "!removegroup" => "!groups remove <group>",
        "!enablegroup" => "!groups enable <group>",
        "!disablegroup" => "!groups disable <group>",
        "!addslot" => "!groups slot add <group> <slot>",
        "!removeslot" => "!groups slot remove <group> <slot>",
        "!addroom" => "!groups room add <group> [slot] <room>",
        "!removeroom" => "!groups room remove <group> [slot] <room>",
        "!setgroupweight" => "!groups weight <group> <factor>",
        "!setroomweight" => "!groups weight <group> <room> <factor>",
        _ => return None,
    };
    Some(format!(
        "{cmd} was renamed — use {new}. (!help lists all commands)"
    ))
}
