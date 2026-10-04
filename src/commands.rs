use anyhow::Result;
use chrono::Utc;
use matrix_sdk::{
    ruma::{
        events::room::message::{
            Relation, RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
        },
        OwnedUserId,
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
    rhythm::{parse_weekday, Turn},
    schedule::build_schedule,
    scheduler,
    state::{add_weeks, current_iso_week, week_dates, SwapStatus},
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

pub(crate) use assignments::cmd_next;
use assignments::*;
use exports::*;
pub(crate) use exports::{cmd_pdf, plan_text};
pub(crate) use helpers::*;
use maintenance::*;
use member::*;
pub(crate) use member::{join_group, leave_group};
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

/// How an answer relates to what it answers: a reply, or a reply in the
/// thread the command was written in.
pub type Answer = Relation<RoomMessageEventContentWithoutRelation>;

pub async fn handle(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    body: &str,
    answer_to: Answer,
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

    if room.room_id() != ctx.room_id
        && !ctx.admin_users.contains(sender)
        && !private_command_allowed(cmd, &args)
    {
        return Ok(Some(format::intentional(format::mentionify(
            "🏠 Use that command in the cleaning room.",
        ))));
    }

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
        ("!plan", Some("remind")) => {
            let main = room
                .client()
                .get_room(&ctx.room_id)
                .ok_or_else(|| anyhow::anyhow!("Cleaning room unavailable"))?;
            return cmd_remind(ctx, sender, &main, rest).await;
        }
        ("!plan", Some("announce")) => {
            let main = room
                .client()
                .get_room(&ctx.room_id)
                .ok_or_else(|| anyhow::anyhow!("Cleaning room unavailable"))?;
            return cmd_announceweek(ctx, sender, &main).await;
        }
        ("!plan", Some("pdf")) => return cmd_pdf(ctx, sender, room, rest, Some(answer_to)).await,
        ("!mygroups", _) => return cmd_mygroups(ctx, sender, room).await,
        ("!help", Some("post")) => return cmd_help_post(ctx, sender, room).await,
        ("!member", Some("welcome")) => return cmd_member_welcome(ctx, sender, room, rest).await,
        ("!ical", Some("revoke")) => return cmd_icalrevoke(ctx, sender, rest).await,
        ("!ical", Some("reset")) => return cmd_icalreset(ctx, sender, rest).await,
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
        ("!next" | "!myplan" | "!mycleaning", _) => cmd_next(ctx, sender, &args).await,
        ("!cleaning", Some("person")) => cmd_cleaning_person(ctx, sender, rest).await,
        ("!cleaning", _) => Ok(Some(
            "!cleaning person <name> shows someone's turns (same as !next <name>). \
             !groups lists groups; !help lists everything."
                .into(),
        )),
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
        ("!help", Some("more")) => Ok(Some(more_help_text())),
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
        ("!groups", Some("rhythm")) => cmd_groups_rhythm(ctx, sender, rest).await,
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
        if let Some(main) = room.client().get_room(&ctx.room_id) {
            scheduler::refresh_pinned_plan(ctx, &main, year, week).await;
        }
    }

    match reply {
        None => Ok(None),
        Some(s) => Ok(Some(format::intentional(
            format::mentionify_rich(&s, room).await,
        ))),
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
        ("!done" | "!undo" | "!join" | "!leave", _) => joined(args),
        ("!next" | "!myplan" | "!mycleaning", _) => person_then_number(args),
        ("!cleaning", Some("person")) => with_sub(args[0], person_then_number(rest)),
        ("!takeover", _) => group_first(state, args, 0),
        ("!swap", Some("accept" | "reject")) => owned(args),
        ("!swap", Some(_)) => {
            let mut out = vec![args[0].to_owned()];
            out.extend(group_first(state, rest, 0));
            out
        }
        ("!stats", Some("fairness")) => with_sub(args[0], joined(rest)),
        ("!stats", _) => joined(args),
        ("!ical", Some("reset" | "revoke")) => with_sub(args[0], joined(rest)),
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
        ("!groups", Some("rhythm")) => with_sub(args[0], group_first(state, rest, 0)),
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
        ("!member", Some("back" | "welcome")) => with_sub(args[0], joined(rest)),
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
const MEMBER_USAGE: &str = "Usage: !member add|remove <@user:server | name> <group> · !member link <name> <@user:server> · !member away <person> [weeks] · !member back <person> · !member welcome <person>";

/// What a resident may do in a verified private chat with the bot: their
/// own turns, groups, completions and calendar, and the public plan —
/// nothing about other people, nothing for admins (they have their own DM).
pub(crate) fn private_command_allowed(cmd: &str, args: &[&str]) -> bool {
    match cmd {
        "!help" | "!mygroups" | "!join" | "!leave" | "!done" | "!undo" => true,
        "!next" | "!myplan" | "!mycleaning" => {
            args.is_empty() || (args.len() == 1 && args[0].parse::<usize>().is_ok())
        }
        // The plan and its PDF are public in the cleaning room anyway.
        "!plan" => {
            args.is_empty()
                || (args.len() == 1 && args[0].parse::<usize>().is_ok())
                || args[0].eq_ignore_ascii_case("pdf")
        }
        "!ical" => {
            args.is_empty()
                || (args.len() == 1
                    && (matches!(args[0], "reset" | "revoke") || args[0].parse::<usize>().is_ok()))
        }
        _ => false,
    }
}

fn help_text() -> String {
    "🧹 **A little cleaning, a happier house** ✨\n\n\
📅 !next · your next turn\n\
📋 !plan · see the plan\n\
👥 !mygroups · join or leave groups\n\
✅ !done · done! (or ✅ on the plan)\n\
🔄 !swap @user · ask for cover\n\
🗓 !ical · your calendar\n\
📄 !plan pdf · printable plan\n\n\
❓ !help more · !help admin"
        .into()
}

fn more_help_text() -> String {
    "🫧 **A little more help**\n\
!status · this week's progress\n\
!undo [group] · take your done mark back\n\
!done <group> · mark a group you cleaned\n\
!takeover [group] · take a turn over yourself\n\
!swap @user [group] · ask someone to cover; they accept or reject\n\
!join <group> · !leave <group> · without the number buttons\n\
!groups [group] · groups, members, rooms\n\
!stats · cleaning history\n\
!plan pdf next 20 · a PDF from next week on · history = past weeks\n\
!ical reset · a new calendar link (the old one stops working)\n\n\
👥 In !mygroups, tap a number to join that group — tap it again to leave. \
✅ marks the groups you're in.\n\
🔒 !next, !plan, !mygroups, !done and !ical also work in a private chat with me \
— tap 💬 on the help board and I'll invite you."
        .into()
}

fn admin_help_text() -> String {
    r#"🔧 **Admin commands**

**Members**
!member add <@user:server | name> <group> · name = person without Matrix
!member remove <@user:server | name> <group>
!member link <name> <@user:server> · connect a name-only person to Matrix
!member welcome <person> · send them the welcome and group list again
!help post · post the help board (buttons for everyone) in the cleaning room
!member away <person> [weeks] · skip in new rotation picks (default 4)
!member back <person>

**This week & plan**
!plan skip [group] [slot] [on <day>] · excuse this week (not counted as missed)
!plan remind [group] · send the reminder now
!plan announce · (re)post and pin this week's plan
!plan assign <group> [slot] <person> [week N] [on <day>]
!plan unassign <group> [slot] [week N] [on <day>]
!plan reset <group> · redistribute future weeks from the rotation
!plan import [--replace] <YYYY-Www[:day]> <group>[/slot] <person> [; …]
!plan pdf [next | history] [N] [group] · printable plan (next = from next week)

**Groups**
!groups <group> · details: rhythm, turn order, slots, rooms, weights
!groups rhythm <group> weekly | 2x | every 2 | mon-tue thu-fri · how often it is cleaned
!groups add|enable|disable <group>
!groups remove <group> [confirm] · deletes it with its history
!groups slot add|remove <group> <slot>
!groups room add|remove <group> [slot] <room>
!groups weight <group> [room] <factor>

**Other**
!ical · !ical reset (own feed, privately) · !ical revoke [person]
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
