// Tests build fixtures by tweaking `Default` values field by field.
#![cfg_attr(test, allow(clippy::field_reassign_with_default))]

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use anyhow::Result;
use mxbot_common::{
    admin::Dispatch,
    matrix_sdk::{
        deserialized_responses::EncryptionInfo,
        ruma::{
            events::{
                reaction::OriginalSyncReactionEvent,
                relation::{Reply, Thread},
                room::{
                    member::{MembershipState, OriginalSyncRoomMemberEvent},
                    message::{
                        MessageType, OriginalSyncRoomMessageEvent, Relation,
                        RoomMessageEventContent,
                    },
                    redaction::OriginalSyncRoomRedactionEvent,
                },
            },
            OwnedRoomId, OwnedUserId,
        },
        Client, Room, RoomState,
    },
    Bot,
};
use tokio::sync::Mutex;
use tracing::{error, info};

mod analytics;
mod commands;
mod config;
mod domain;
mod format;
mod help_board;
mod http;
mod ical;
mod names;
mod onboarding;
mod paper;
mod pdf;
mod pdf_renderer;
mod private;
mod reactions;
mod resolver;
mod rhythm;
mod schedule;
mod scheduler;
mod state;
mod trades;
mod turn_menu;
mod validate;
mod view;

use config::Config;
use state::State;

/// Give every group without an explicit rhythm the old global
/// `interval_weeks`. Returns whether anything changed.
fn migrate_rhythms(state: &mut State, interval_weeks: u32) -> bool {
    let mut changed = false;
    for group in &mut state.cleaning_groups {
        if group.rhythm.every_weeks.is_none() {
            group.rhythm.every_weeks = Some(interval_weeks.max(1));
            changed = true;
        }
    }
    changed
}

/// How the bot answers a message: as a plain reply, visible in the room in
/// every client — or, when it was written in a thread, in that thread.
/// (Answers used to go into a thread always, which some clients tuck away
/// behind a reply counter.)
fn answer_relation(ev: &OriginalSyncRoomMessageEvent) -> commands::Answer {
    match &ev.content.relates_to {
        Some(Relation::Thread(thread)) => {
            Relation::Thread(Thread::reply(thread.event_id.clone(), ev.event_id.clone()))
        }
        _ => Relation::Reply(Reply::with_event_id(ev.event_id.clone())),
    }
}

#[derive(Clone)]
pub struct BotContext {
    pub state: Arc<Mutex<State>>,
    pub operations: Arc<Mutex<()>>,
    pub state_path: PathBuf,
    pub config: Arc<Config>,
    pub admin_users: HashSet<OwnedUserId>,
    pub room_id: OwnedRoomId,
}

#[tokio::main]
async fn main() -> Result<()> {
    mxbot_common::logging::init("cleaning_bot");

    let config: Config =
        mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;
    anyhow::ensure!(
        config.schedule.timezone.parse::<chrono_tz::Tz>().is_ok(),
        "Invalid schedule timezone"
    );
    anyhow::ensure!(
        (1..=52).contains(&config.schedule.interval_weeks),
        "interval_weeks must be 1–52"
    );
    anyhow::ensure!(
        config.schedule.reminder_weekday < 7 && config.schedule.final_reminder_weekday < 7,
        "Reminder weekdays must be 0–6"
    );
    anyhow::ensure!(
        config.schedule.fill_strategy == config::FillStrategy::RoundRobin,
        "least_loaded_first is not implemented; use round_robin"
    );
    anyhow::ensure!(
        (1..=104).contains(&config.schedule.materialize_weeks),
        "materialize_weeks must be 1–104"
    );
    if let Some(ical) = &config.ical_server {
        if !ical.public_url.starts_with("https://") {
            tracing::warn!(
                "ical_server.public_url is not HTTPS — calendar feed tokens would travel in clear text"
            );
        }
    }
    let config = Arc::new(config);

    // Must happen before any `current_iso_week()` call (materialize below,
    // state load, etc.) so "what week is it" is decided in the configured
    // local timezone from the very first use, not just once the scheduler
    // ticks.
    state::set_timezone(config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC));

    let store_path = mxbot_common::config::store_path_from_env();
    tokio::fs::create_dir_all(&store_path).await?;

    let state_path = store_path.join("state.json");
    let mut st = State::load(&state_path).await?;
    if st.created_at.is_none() {
        st.created_at = Some(chrono::Utc::now());
        st.save(&state_path).await?;
    }

    // Everyone who already used the bot has had their first time.
    if onboarding::migrate_welcomes(&mut st) {
        st.save(&state_path).await?;
    }

    // Groups from before per-group rhythms keep the old global interval.
    if migrate_rhythms(&mut st, config.schedule.interval_weeks) {
        st.save(&state_path).await?;
    }

    // Plans made before the start of a split week took turns: re-seat the
    // weeks from next week on once, so nobody keeps always starting.
    if !st.shifts_rebalanced {
        let (year, week) = state::current_iso_week();
        let next = state::add_weeks(year, week, 1);
        let n = resolver::rebalance_shifts(&mut st, next)?;
        st.shifts_rebalanced = true;
        st.save(&state_path).await?;
        tracing::info!("Re-seated {n} turns so the start of split weeks takes turns.");
    }

    // Materialize future assignments (idempotent — skips already-stored turns).
    {
        let mat_events = resolver::materialize(&st, config.schedule.materialize_weeks as usize);
        let n = mat_events.len();
        for ev in mat_events {
            st.apply_event(ev).ok();
        }
        if n > 0 {
            st.save(&state_path).await?;
            tracing::info!("Materialized {n} slot assignments.");
        }
    }

    // Validate on startup — log issues but never abort.
    {
        let report = validate::validate_state(&st);
        if !report.errors.is_empty() {
            tracing::error!("State validation errors on startup:\n{}", report.summary());
        } else if !report.warnings.is_empty() {
            tracing::warn!(
                "State validation warnings on startup:\n{}",
                report.summary()
            );
        } else {
            tracing::info!("State validation passed.");
        }
    }

    let state = Arc::new(Mutex::new(st));

    let room_id =
        mxbot_common::rooms::parse_room_id("[schedule] room_id", &config.schedule.room_id)?;

    // ── Optional HTTP iCal server ─────────────────────────────────────────────
    if let Some(ref ical_cfg) = config.ical_server {
        let bind_addr = ical_cfg.bind_addr.clone();
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = http::run(state_clone, &bind_addr).await {
                error!("iCal HTTP server error: {e}");
            }
        });
    }

    let bot = Bot::builder("cleaning-bot", env!("CARGO_PKG_VERSION"))
        .store_path(&store_path)
        .admin_help("Any admin command of the cleaning plan (!help admin lists them), e.g. !member add, !plan assign, !groups")
        .start(&config.matrix, &config.security)
        .await?;
    let client = bot.client.clone();
    let bot_user_id = bot.user_id.clone();

    let ctx = BotContext {
        operations: Arc::new(Mutex::new(())),
        state: state.clone(),
        state_path,
        config: Arc::clone(&config),
        admin_users: bot.admins().clone(),
        room_id: room_id.clone(),
    };

    // ── Message / command handler ─────────────────────────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        let bot = bot.clone();
        move |ev: OriginalSyncRoomMessageEvent,
              room: Room,
              client: Client,
              encryption: Option<EncryptionInfo>| {
            let ctx = ctx.clone();
            let bot = bot.clone();
            async move {
                if ev.sender == bot.user_id {
                    return;
                }
                if room.state() != RoomState::Joined {
                    return;
                }
                // Admin commands sent in a direct chat are answered there.
                let admin_dm = match bot.admin.handle(&room, &ev, encryption.as_ref()).await {
                    Dispatch::Handled => return,
                    Dispatch::AdminDm => true,
                    Dispatch::Continue => false,
                };
                // Editing a message doesn't run its commands again.
                if matches!(ev.content.relates_to, Some(Relation::Replacement(_))) {
                    return;
                }
                if let MessageType::Image(ref image) = ev.content.msgtype {
                    if let Err(e) = paper::image(&ctx, &room, &ev.sender, &ev.event_id, image).await
                    {
                        error!("Paper scan failed: {e}");
                    }
                    return;
                }
                let MessageType::Text(ref text) = ev.content.msgtype else {
                    return;
                };
                let body = text.body.trim();
                let cmd_lines: Vec<&str> = body
                    .lines()
                    .map(str::trim)
                    .filter(|l| l.starts_with('!'))
                    .collect();
                if cmd_lines.is_empty() {
                    return;
                }
                // Outside the cleaning room: admins in their DM, or a resident
                // in a verified private chat (see `private`) — checked only
                // for commands, as it asks the server for both rooms' members.
                let private = room.room_id() != ctx.room_id;
                if private && !admin_dm && !private::authorized(&ctx, &room, &ev.sender).await {
                    return;
                }
                let _operation = ctx.operations.lock().await;

                let answer_to = answer_relation(&ev);

                let mut replies: Vec<RoomMessageEventContent> = Vec::new();
                for (index, line) in cmd_lines.into_iter().enumerate() {
                    // A re-delivered message (sync after a restart) runs
                    // each of its commands at most once.
                    let key = format!("{}:{index}", ev.event_id);
                    {
                        let mut state = ctx.state.lock().await;
                        if state.processed_commands.contains(&key) {
                            continue;
                        }
                        state.active_command = Some(key.clone());
                    }
                    match commands::handle(&ctx, &ev.sender, &room, line, answer_to.clone()).await {
                        Ok(Some(reply)) => replies.push(reply),
                        Err(e) if e.is::<mxbot_common::admin::NotAdmin>() => replies.push(
                            format::mentionify("❌ This command requires admin privileges."),
                        ),
                        Ok(None) => {}
                        Err(e) => {
                            // Marked processed only if it already saved a change.
                            ctx.state.lock().await.active_command = None;
                            error!("Command error: {e}");
                            continue;
                        }
                    }
                    let mut state = ctx.state.lock().await;
                    state.active_command = None;
                    state.processed_commands.insert(key);
                    if let Err(e) = state.save(&ctx.state_path).await {
                        error!("Cannot persist command replay guard: {e}");
                        return;
                    }
                }
                if !replies.is_empty() {
                    let target = if private {
                        Some(room.clone())
                    } else {
                        client.get_room(&ctx.room_id)
                    };
                    if let Some(r) = target {
                        let mut content = if replies.len() == 1 {
                            replies.remove(0)
                        } else {
                            let joined = replies
                                .iter()
                                .filter_map(|c| {
                                    if let MessageType::Text(t) = &c.msgtype {
                                        Some(t.body.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("\n\n");
                            format::mentionify(&joined)
                        };
                        if !private {
                            content.relates_to = Some(answer_to);
                        }
                        r.send(format::intentional(content)).await.ok();
                    }
                }
                if !private {
                    onboarding::welcome_if_new(&ctx, &room, &ev.sender).await;
                }
                onboarding::refresh_all_selectors(&ctx, &client).await;
            }
        }
    });

    // ── Reaction handler ──────────────────────────────────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncReactionEvent, room: Room, client: Client| {
            let ctx = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                if ev.sender == bot_user_id {
                    return;
                }
                if room.state() != RoomState::Joined {
                    return;
                }
                if room.room_id() != ctx.room_id
                    && !private::authorized(&ctx, &room, &ev.sender).await
                {
                    return;
                }
                let _operation = ctx.operations.lock().await;

                if ctx
                    .state
                    .lock()
                    .await
                    .redacted_reactions
                    .contains(ev.event_id.as_str())
                {
                    return;
                }
                let reacted_to = ev.content.relates_to.event_id.to_string();
                let emoji_key = ev.content.relates_to.key.clone();
                let sender_mxid = ev.sender.as_str().to_owned();

                match paper::reaction(&ctx, &room, &ev.sender, &reacted_to, &emoji_key).await {
                    Ok(true) => {
                        onboarding::consume_tap(&room, &ev.event_id, &bot_user_id).await;
                        return;
                    }
                    Err(e) => {
                        error!("Paper confirmation failed: {e}");
                        return;
                    }
                    Ok(false) => {}
                }

                // A first reaction here is a first visit too.
                if room.room_id() == ctx.room_id {
                    onboarding::welcome_if_new(&ctx, &room, &ev.sender).await;
                }

                // ── Group selector tap ────────────────────────────────────────
                {
                    let mut state = ctx.state.lock().await;
                    if state
                        .group_selectors
                        .get(&reacted_to)
                        .is_some_and(|s| onboarding::in_room(s, room.room_id(), &ctx))
                    {
                        let reply = onboarding::tap(
                            &ctx,
                            &mut state,
                            &reacted_to,
                            ev.event_id.as_str(),
                            &sender_mxid,
                            &emoji_key,
                        );
                        let saved = match state.save(&ctx.state_path).await {
                            Ok(()) => true,
                            Err(e) => {
                                tracing::error!("Failed to save after a group selector tap: {e}");
                                false
                            }
                        };
                        let recorded = state.group_selectors.get(&reacted_to).is_some_and(|s| {
                            s.taps.get(ev.event_id.as_str()).is_some_and(|t| !t.undone)
                        });
                        drop(state);
                        match reply {
                            Ok(Some(_)) => {
                                // The answer is edited into the selector itself.
                                onboarding::refresh_all_selectors(&ctx, &client).await;
                                if let Some(r) = client.get_room(&ctx.room_id) {
                                    let (year, week) = state::current_iso_week();
                                    scheduler::refresh_pinned_plan(&ctx, &r, year, week).await;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => tracing::error!("Group selector tap failed: {e}"),
                        }
                        // Only once the tap is safely saved: take the
                        // reaction away, so the same number can be tapped
                        // again. Never an undo — see the redaction handler.
                        if saved && recorded {
                            onboarding::consume_tap(&room, &ev.event_id, &bot_user_id).await;
                        }
                        return;
                    }
                }

                // ── Turn menu tap (in the cleaning room or a private chat) ─────
                if turn_menu::on_reaction(
                    &ctx,
                    &room,
                    &ev.event_id,
                    &ev.sender,
                    &reacted_to,
                    &emoji_key,
                    &bot_user_id,
                )
                .await
                {
                    return;
                }

                // ── Help board button ─────────────────────────────────────────
                let on_board = room.room_id() == ctx.room_id
                    && ctx.state.lock().await.help_boards.contains(&reacted_to);
                if on_board {
                    let Some(action) = help_board::action_for(&emoji_key) else {
                        return;
                    };
                    {
                        // Once per tap, also when delivered again later.
                        let mut state = ctx.state.lock().await;
                        if !state.help_taps.insert(ev.event_id.to_string()) {
                            return;
                        }
                        if let Err(e) = state.save(&ctx.state_path).await {
                            error!("Failed to save a help board tap: {e}");
                            return;
                        }
                    }
                    let board = ev.content.relates_to.event_id.clone();
                    help_board::run(&ctx, &client, &room, &board, &ev.sender, action).await;
                    // Take the button press back, so it can be pressed again.
                    onboarding::consume_tap(&room, &ev.event_id, &bot_user_id).await;
                    return;
                }

                // ── Swapping: 🆘 on the plan, taps on a request or an early swap ──
                if room.room_id() == ctx.room_id
                    && trades::on_reaction(
                        &ctx,
                        &room,
                        &ev.event_id,
                        &ev.sender,
                        &reacted_to,
                        &emoji_key,
                    )
                    .await
                {
                    return;
                }

                if room.room_id() != ctx.room_id || emoji_key != "✅" {
                    return;
                }

                // ── Consolidated weekly plan reaction ─────────────────────────
                let mut state = ctx.state.lock().await;
                // ✅ on the plan — or on one of that week's reminders.
                let Some(plan_week) = state
                    .weekly_plan_event_ids
                    .get(&reacted_to)
                    .copied()
                    .or_else(|| {
                        state
                            .reminder_messages
                            .get(&reacted_to)
                            .map(|r| (r.iso_year, r.iso_week))
                    })
                else {
                    return;
                };
                match reactions::plan_done(
                    &mut state,
                    ev.event_id.as_str(),
                    &sender_mxid,
                    plan_week,
                ) {
                    Ok(reactions::PlanDone::Seen) => {}
                    Ok(reactions::PlanDone::NothingOpen) => {
                        drop(state);
                        // Said to them, as a reply to what they reacted on.
                        let mut content = format::intentional(format::mentionify(&format!(
                            "{sender_mxid} — you have no open turn this week, nothing to mark."
                        )));
                        content.relates_to = Some(Relation::Reply(Reply::with_event_id(
                            ev.content.relates_to.event_id.clone(),
                        )));
                        if let Some(r) = client.get_room(&ctx.room_id) {
                            r.send(content).await.ok();
                        }
                    }
                    Ok(
                        outcome @ (reactions::PlanDone::Marked
                        | reactions::PlanDone::SwappedEarly(_)),
                    ) => {
                        if let Err(e) = state.save(&ctx.state_path).await {
                            tracing::error!("Failed to save after plan reaction: {e}");
                            return;
                        }
                        drop(state);
                        if let Some(r) = client.get_room(&ctx.room_id) {
                            if let reactions::PlanDone::SwappedEarly(trade) = outcome {
                                trades::post_early_swap(
                                    &ctx,
                                    &r,
                                    &sender_mxid,
                                    trade,
                                    Some(ev.event_id.to_string()),
                                )
                                .await;
                            }
                            let (year, week) = plan_week;
                            scheduler::refresh_pinned_plan(&ctx, &r, year, week).await;
                        }
                    }
                    Err(e) => tracing::error!("Marking plan reaction done failed: {e}"),
                }
            }
        }
    });

    // ── Redaction handler (undo a ✅, a 🆘 or a group selector tap) ───────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncRoomRedactionEvent, room: Room, client: Client| {
            let ctx = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                // The bot's own redactions only clear consumed selector
                // taps (`onboarding::consume_tap`) — never an undo.
                if ev.sender == bot_user_id {
                    return;
                }
                if room.state() != RoomState::Joined {
                    return;
                }
                if room.room_id() != ctx.room_id
                    && !private::authorized(&ctx, &room, &ev.sender).await
                {
                    return;
                }
                let _operation = ctx.operations.lock().await;

                let redacted_id = match ev.content.redacts.as_ref().or(ev.redacts.as_ref()) {
                    Some(id) => id.to_string(),
                    None => return,
                };
                let mut state = ctx.state.lock().await;
                let outcome = reactions::redaction(
                    &ctx,
                    &mut state,
                    room.room_id(),
                    ev.sender.as_str(),
                    &redacted_id,
                );
                if outcome == reactions::Redaction::Refused {
                    return;
                }
                if let Err(e) = state.save(&ctx.state_path).await {
                    error!("Failed to save after a redaction: {e}");
                }
                drop(state);
                let refresh = match outcome {
                    reactions::Redaction::TapUndone => {
                        onboarding::refresh_all_selectors(&ctx, &client).await;
                        Some(state::current_iso_week())
                    }
                    reactions::Redaction::DoneUndone { year, week } => Some((year, week)),
                    _ => None,
                };
                if let (Some((year, week)), Some(r)) = (refresh, client.get_room(&ctx.room_id)) {
                    scheduler::refresh_pinned_plan(&ctx, &r, year, week).await;
                }
                // A ✅ behind an early swap swaps back; a 🆘 withdraws.
                if room.room_id() == ctx.room_id {
                    trades::on_redaction(&ctx, &room, ev.sender.as_str(), &redacted_id).await;
                }
            }
        }
    });

    // ── Member-join handler (welcome new users) ───────────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncRoomMemberEvent, room: Room| {
            let ctx = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                if room.state() != RoomState::Joined {
                    return;
                }
                if ev.content.membership != MembershipState::Join {
                    return;
                }
                if ev.state_key == bot_user_id {
                    return;
                }
                if ev
                    .prev_content()
                    .is_some_and(|prev| prev.membership == MembershipState::Join)
                {
                    return; // a profile change, not a join
                }
                let _operation = ctx.operations.lock().await;
                onboarding::welcome_if_new(&ctx, &room, &ev.state_key).await;
                // Someone accepting the private chat the bot opened for them.
                help_board::greet_on_join(&ctx, &room, &ev.state_key).await;
            }
        }
    });

    // ── Initial sync ──────────────────────────────────────────────────────────
    bot.initial_sync().await;
    info!("Initial sync complete");

    // Show real Matrix display names in !status/!groups right away instead
    // of Matrix usernames until each person sends their first command —
    // also for people the bot shares no room with (their global profile).
    // Bounded in time; saved when a name changed.
    names::refresh(&ctx, &client, client.get_room(&ctx.room_id).as_ref()).await;

    // Self-heal the current week's plan message against persisted state
    // before the scheduler loop (or any further event handling) starts, so
    // this can never race a concurrent refresh/announce/tick for the same
    // week. A failure here is logged but never prevents startup.
    scheduler::reconcile_on_startup(&ctx, &client).await;
    // Of the bot's messages, only the newest plan stays pinned.
    if let Some(room) = client.get_room(&ctx.room_id) {
        scheduler::tidy_pins_on_startup(&ctx, &room).await;
    }
    // Group selectors catch up with anything that changed while down.
    onboarding::refresh_all_selectors(&ctx, &client).await;

    tokio::spawn(scheduler::run(ctx, client.clone()));

    bot.run().await
}
