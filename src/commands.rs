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
use rotation::*;
use setup::*;
use stats::*;
use swaps::*;

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

    // Commands that need direct room access.
    match cmd {
        "!linkmatrix" => {
            let s = cmd_linkmatrix(ctx, sender, room, &args).await?;
            return Ok(s.map(RoomMessageEventContent::text_plain));
        }
        "!cleanplan" => return cmd_cleanplan(ctx, sender, room, &args).await,
        "!remind" => return cmd_remind(ctx, sender, room, &args).await,
        "!announceweek" => return cmd_announceweek(ctx, sender, room).await,
        "!repostplan" => return cmd_announceweek(ctx, sender, room).await,
        "!testnotify" => return cmd_testnotify(room).await,
        "!pdf" => return cmd_pdf(ctx, sender, room, &args, event_id, thread_root).await,
        "!ical" => return cmd_ical(ctx, sender, room, &args).await,
        "!icalreset" => return cmd_icalreset(ctx, sender, room, &args).await,
        _ => {}
    }

    let reply: Option<String> = match cmd {
        "!done" => cmd_done(ctx, sender, &args).await,
        "!status" => cmd_status(ctx).await,
        "!stats" => cmd_stats(ctx, &args).await,
        "!groups" => cmd_floors(ctx).await,
        "!cleaning" => cmd_cleaning(ctx, sender, &args).await,
        "!joingroup" => cmd_joinfloor(ctx, sender, &args).await,
        "!leavegroup" => cmd_leavefloor(ctx, sender, &args).await,
        "!swap" => cmd_swap(ctx, sender, &args).await,
        "!acceptswap" => cmd_acceptswap(ctx, sender, &args).await,
        "!rejectswap" => cmd_rejectswap(ctx, sender, &args).await,
        "!assign" => cmd_assign(ctx, sender, &args).await,
        "!unassign" => cmd_unassign(ctx, sender, &args).await,
        "!importplan" => cmd_importplan(ctx, sender, &args).await,
        "!takeover" => cmd_takeover(ctx, sender, &args).await,
        "!adduser" => cmd_adduser(ctx, sender, &args).await,
        "!removeuser" => cmd_removeuser(ctx, sender, &args).await,
        "!addperson" => cmd_addperson(ctx, sender, &args).await,
        "!removeperson" => cmd_removeperson(ctx, sender, &args).await,
        "!addgroup" => cmd_addfloor(ctx, sender, &args).await,
        "!removegroup" => cmd_removefloor(ctx, sender, &args).await,
        "!resetplan" => cmd_resetplan(ctx, sender, &args).await,
        "!addslot" => cmd_addslot(ctx, sender, &args).await,
        "!removeslot" => cmd_removeslot(ctx, sender, &args).await,
        "!addroom" => cmd_addroom(ctx, sender, &args).await,
        "!removeroom" => cmd_removeroom(ctx, sender, &args).await,
        "!undo" => cmd_undo(ctx, sender, &args).await,
        "!next" => cmd_next(ctx, sender, &args).await,
        "!skip" => cmd_skip(ctx, sender, &args).await,
        "!leaderboard" => cmd_leaderboard(ctx).await,
        "!fairness" => cmd_fairness(ctx, &args).await,
        "!planfairness" => cmd_fairness(ctx, &args).await,
        "!workload" => cmd_workload(ctx).await,
        "!groupstats" => cmd_groupstats(ctx).await,
        "!setgroupweight" => cmd_setgroupweight(ctx, sender, &args).await,
        "!setroomweight" => cmd_setroomweight(ctx, sender, &args).await,
        "!disablegroup" => cmd_disablegroup(ctx, sender, &args).await,
        "!enablegroup" => cmd_enablegroup(ctx, sender, &args).await,
        "!listgroups" => cmd_listgroups(ctx).await,
        "!validate" => cmd_validate(ctx, sender).await,
        "!absent" => cmd_absent(ctx, sender, &args).await,
        "!back" => cmd_back(ctx, sender, &args).await,
        "!blame" => cmd_blame(ctx, &args).await,
        "!help" => Ok(Some(help_text())),
        _ => Ok(None),
    }?;

    // Every mutation that can change who's responsible for, or the status
    // of, the running week's plan goes through this single refresh call —
    // the pinned Matrix message is re-rendered straight from `State`, never
    // patched in place, so it can never drift from the persisted domain state.
    if command_may_change_current_plan(cmd) {
        let (year, week) = current_iso_week();
        scheduler::refresh_pinned_plan(ctx, room, year, week).await;
    }

    match reply {
        None => Ok(None),
        Some(s) => Ok(Some(format::mentionify_rich(&s, room).await)),
    }
}

// ── help ─────────────────────────────────────────────────────────────────────

fn help_text() -> String {
    r#"🧹 Cleaning bot commands:

  !help                        · show this help
  !status                      · this week's cleaned / not-cleaned overview
  !areas                       · list all cleaning groups and their members
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
  !takeover [group] [slot] [week N] · claim an already-assigned week for yourself right now (defaults to your own group)
  !ical [N]                    · get your cleaning schedule as iCal (default 26 weeks)
  !icalreset                   · revoke and regenerate your iCal feed URL

Admin commands:
  !skip [group]                         · excuse this week (won't count as missed)
  !announceweek                         · post (or repost) this week's cleaning plan and pin it
  !remind [group]                       · manually fire the cleaning reminder now
  !pdf [N]                              · generate printable HTML schedule (default 8 weeks)
  !absent <person> [weeks]              · skip person in new rotation picks (default 4 weeks; already-frozen weeks are unaffected)
  !back <person>                        · cancel an absence early
  !addgroup <name>                      · create a new cleaning group
  !removegroup <name>                   · delete a cleaning group
  !cleaning add @user:server <group>    · append a Matrix user after the active week
  !cleaning remove @user:server <group> · safely remove a Matrix user from future turns
  !cleaning people [group]              · show ordered cleaning rotations
  !adduser / !removeuser                · legacy aliases for the commands above
  !addperson <name> <group>             · add a non-Matrix person to a group
  !removeperson <name> <group>          · remove a non-Matrix person from a group
  !addroom <group> <room>               · add a room to clean in a group
  !removeroom <group> <room>            · remove a room from a group
  !ical <person> [N]                    · get iCal for any person (admin)
  !icalreset <person>                   · reset iCal token for any person (admin)
  !listgroups                           · list all groups with active/disabled status
  !disablegroup <name>                  · exclude group from scheduling and stats
  !enablegroup <name>                   · re-include a previously disabled group
  !assign <group> [slot] <person> [week N]   · directly assign/change who cleans one week
  !unassign <group> [slot] [week N]          · clear who cleans one week (leave unassigned)
  !importplan [--replace] <YYYY-Www> <group>[/slot] <person> [; ...]  · one-time import of upcoming weeks from the old paper plan (--replace overrides already-frozen weeks)
  !validate                             · check state for consistency issues"#.to_owned()
}
