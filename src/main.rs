// Tests build fixtures by tweaking `Default` values field by field.
#![cfg_attr(test, allow(clippy::field_reassign_with_default))]

use std::{path::PathBuf, sync::Arc, time::Duration};

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
            OwnedRoomId, OwnedUserId, UserId,
        },
        Client, Room, RoomState,
    },
    supervise::{supervise, supervise_ticking},
    Bot,
};
use tokio::sync::Mutex;
use tracing::{error, info};

mod analytics;
mod calendars;
mod commands;
mod config;
mod directory;
mod domain;
mod format;
mod help_board;
mod house;
mod ical;
mod lang;
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
mod web;

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
/// every client, or, when it was written in a thread, in that thread.
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
    /// `[security] admin_users` and, while the directory read is fresh, the house accounts
    /// of `admin_groups` (never a linked account).
    pub admin_users: mxbot_common::admin::Admins,
    pub room_id: OwnedRoomId,
    /// The last good read of the house's directory (`[directory]`); `None` until the first.
    pub directory: directory::Shared,
    /// Every resident's directory login (`[directory]`); `None` until the directory was read
    /// once.
    pub logins: Arc<std::sync::RwLock<Option<Arc<directory::Logins>>>>,
    /// The Matrix account that sent the event being handled (`acting_as`), whichever person
    /// in the plan it counts as: admin rights are its own, never another account's.
    pub acting: Option<OwnedUserId>,
}

impl BotContext {
    /// The account the plan knows `user`'s resident by (`directory`).
    pub async fn person_account(&self, user: &OwnedUserId) -> OwnedUserId {
        let snapshot = directory::read(&self.directory);
        let state = self.state.lock().await;
        directory::person_account(snapshot.as_deref().map(|s| &s.people), &state, user)
    }

    /// Every account of `user`'s resident the directory recognises, `user` among them; empty
    /// when it doesn't know `user` (`directory::recognised`).
    pub fn recognised(&self, user: &UserId) -> Vec<OwnedUserId> {
        let snapshot = directory::read(&self.directory);
        directory::recognised(snapshot.as_deref().map(|s| &s.people), user)
    }

    /// This context for an event `account` sent (see `acting`).
    pub fn acting_as(&self, account: &UserId) -> BotContext {
        BotContext {
            acting: Some(account.to_owned()),
            ..self.clone()
        }
    }

    /// An admin: one of `[security] admin_users`, as the account that wrote. The person
    /// `user` stands for in the plan (their address) never makes another of their accounts
    /// an admin: rights never come from a link.
    pub fn is_admin(&self, user: &UserId) -> bool {
        self.admin_users
            .contains(self.acting.as_deref().unwrap_or(user))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    mxbot_common::logging::init("cleaning_bot");

    let config: Config =
        mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;
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
    config.web.validate()?;
    // The website needs the proxy's secret: refused at startup, not on the first visit.
    let web_secret = if config.web.enabled {
        Some(web::proxy_secret_from_env()?)
    } else {
        None
    };
    let config = Arc::new(config);

    // Must happen before any `current_iso_week()` call (materialize below,
    // state load, etc.) so "what week is it" is decided in the configured
    // local timezone from the very first use, not just once the scheduler
    // ticks.
    state::set_timezone(config.schedule.timezone);

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

    let room_id =
        mxbot_common::rooms::parse_room_id("[schedule] room_id", &config.schedule.room_id)?;
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

    // Materialize future assignments (idempotent, skips already-stored turns).
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

    // Validate on startup: log issues but never abort.
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
    // Filled by `directory::run` once the bot is up; the website reads it too.
    let directory_read: directory::Shared = Arc::default();

    // ── Optional website (its own listener, behind the house login) ─────────────
    if let Some(secret) = web_secret {
        let site = web::Web::new(
            state.clone(),
            directory_read.clone(),
            web::Settings::new(&config.web, secret),
            &config.matrix.user_id,
        )?;
        supervise("website", web::serve(site));
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
        admin_users: bot.admin.admin_list().clone(),
        room_id: room_id.clone(),
        directory: directory_read,
        logins: Arc::default(),
        acting: None,
    };
    // The plan follows the directory once the first sync is done (the rooms are known).
    let synced = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(cfg) = config.directory.clone() {
        if let Some(directory) = directory::open(&ctx, &cfg) {
            // Three missed reads (each gives up after 30 s) before the bot counts as stuck. A
            // directory that can't be read is Authentik's outage, not the bot's: the task is
            // "unavailable" in the status file, never unhealthy, and nothing follows it.
            let within = (cfg.reader.refresh() + Duration::from_secs(60)) * 3;
            let synced = Arc::clone(&synced);
            let admin_groups = config.security.admin_groups.clone();
            supervise_ticking("directory sync", within, |alive| {
                directory::run(
                    ctx.clone(),
                    client.clone(),
                    directory,
                    cfg,
                    admin_groups,
                    alive,
                    synced,
                )
            });
        }
    }

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
                // The resident, whichever of their accounts wrote (`directory`).
                // `ev.sender` stays the account, for the private-chat checks and for
                // admin rights (`BotContext::acting`).
                let ctx = ctx.acting_as(&ev.sender);
                let sender = ctx.person_account(&ev.sender).await;
                if let MessageType::Image(ref image) = ev.content.msgtype {
                    if let Err(e) = paper::image(&ctx, &room, &sender, &ev.event_id, image).await {
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
                // in a verified private chat (see `private`): checked only
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
                    match commands::handle(&ctx, &sender, &room, line, answer_to.clone()).await {
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
                        if let Err(e) =
                            mxbot_common::send::send_with_retry(&r, format::intentional(content))
                                .await
                        {
                            error!("Failed to answer {sender}: {e}");
                        }
                    }
                }
                if !private {
                    onboarding::welcome_if_new(&ctx, &room, &sender).await;
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
                let ctx = ctx.acting_as(&ev.sender);
                let sender = ctx.person_account(&ev.sender).await;

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
                let sender_mxid = sender.as_str().to_owned();

                match paper::reaction(&ctx, &room, &sender, &reacted_to, &emoji_key).await {
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
                    onboarding::welcome_if_new(&ctx, &room, &sender).await;
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
                        // again. Never an undo, see the redaction handler.
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
                    &sender,
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
                    help_board::run(&ctx, &client, &room, &board, &ev.sender, &sender, action)
                        .await;
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
                        &sender,
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
                // ✅ on the plan, or on one of that week's reminders.
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
                            "{sender_mxid} 🤷 You have no open turn this week, nothing to mark."
                        )));
                        content.relates_to = Some(Relation::Reply(Reply::with_event_id(
                            ev.content.relates_to.event_id.clone(),
                        )));
                        if let Some(r) = client.get_room(&ctx.room_id) {
                            if let Err(e) = mxbot_common::send::send_with_retry(&r, content).await {
                                error!("Failed to answer {sender_mxid}'s ✅: {e}");
                            }
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
                // taps (`onboarding::consume_tap`), never an undo.
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
                let ctx = ctx.acting_as(&ev.sender);
                let sender = ctx.person_account(&ev.sender).await;

                let redacted_id = match ev.content.redacts.as_ref().or(ev.redacts.as_ref()) {
                    Some(id) => id.to_string(),
                    None => return,
                };
                let mut state = ctx.state.lock().await;
                let outcome = reactions::redaction(
                    &ctx,
                    &mut state,
                    room.room_id(),
                    sender.as_str(),
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
                    trades::on_redaction(&ctx, &room, sender.as_str(), &redacted_id).await;
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
                let person = ctx.person_account(&ev.state_key).await;
                onboarding::welcome_if_new(&ctx, &room, &person).await;
                // Someone accepting the private chat the bot opened for them.
                help_board::greet_on_join(&ctx, &room, &ev.state_key).await;
            }
        }
    });

    // ── Initial sync ──────────────────────────────────────────────────────────
    bot.initial_sync().await;
    info!("Initial sync complete");
    synced.store(true, std::sync::atomic::Ordering::Release);

    // Show real Matrix display names in !status/!groups right away instead
    // of Matrix usernames until each person sends their first command:
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

    // Every resident's turns in a calendar of their own in the cloud (`[nextcloud]`).
    // Without it, nobody is told their turns are there any more.
    let nextcloud = config
        .nextcloud
        .clone()
        .and_then(|cfg| calendars::connect(&ctx, &cfg).map(|nc| (cfg, nc)));
    if let Some((cfg, nc)) = nextcloud {
        // A look every 30 s; a complete comparison (every request up to 30 s) can take minutes.
        supervise_ticking("calendar sync", Duration::from_secs(30 * 60), |alive| {
            calendars::run(ctx.clone(), client.clone(), cfg, nc, alive)
        });
    } else {
        calendars::not_managed(&ctx, &client).await;
    }

    // A round every 30 minutes; two hours without one and the bot counts as stuck.
    supervise_ticking(
        "cleaning scheduler",
        Duration::from_secs(2 * 60 * 60),
        |alive| scheduler::run(ctx, client.clone(), alive),
    );

    bot.run().await
}
