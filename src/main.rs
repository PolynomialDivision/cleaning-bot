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
                room::{
                    member::{MembershipState, OriginalSyncRoomMemberEvent},
                    message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
                    redaction::OriginalSyncRoomRedactionEvent,
                },
            },
            OwnedEventId, OwnedRoomId, OwnedUserId,
        },
        Client, Room, RoomState,
    },
    send::{in_thread, thread_root},
    Bot,
};
use tokio::sync::Mutex;
use tracing::{error, info};

mod analytics;
mod commands;
mod config;
mod domain;
mod format;
mod http;
mod ical;
mod onboarding;
mod pdf;
mod pdf_renderer;
mod resolver;
mod rhythm;
mod schedule;
mod scheduler;
mod state;
mod validate;
mod view;

use config::Config;
use state::{MarkedDuty, ReactionDone, State};

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

fn thread_reply(text: &str, root: OwnedEventId, reply_to: OwnedEventId) -> RoomMessageEventContent {
    in_thread(format::mentionify(text), root, reply_to)
}

#[derive(Clone)]
pub struct BotContext {
    pub state: Arc<Mutex<State>>,
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
                if !admin_dm && room.room_id() != ctx.room_id {
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

                let thread_root = thread_root(&ev);

                let mut replies: Vec<RoomMessageEventContent> = Vec::new();
                for line in cmd_lines {
                    match commands::handle(
                        &ctx,
                        &ev.sender,
                        &room,
                        line,
                        ev.event_id.clone(),
                        thread_root.clone(),
                    )
                    .await
                    {
                        Ok(Some(reply)) => replies.push(reply),
                        Err(e) if e.is::<mxbot_common::admin::NotAdmin>() => replies.push(
                            format::mentionify("❌ This command requires admin privileges."),
                        ),
                        Ok(None) => {}
                        Err(e) => error!("Command error: {e}"),
                    }
                }
                if !replies.is_empty() {
                    let target = if admin_dm {
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
                        if !admin_dm {
                            content = in_thread(content, thread_root, ev.event_id.clone());
                        }
                        r.send(content).await.ok();
                    }
                }
                if !admin_dm {
                    onboarding::welcome_if_new(&ctx, &room, &ev.sender).await;
                    onboarding::refresh_selectors(&ctx, &room).await;
                }
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
                if room.room_id() != ctx.room_id {
                    return;
                }

                let reacted_to = ev.content.relates_to.event_id.to_string();
                let emoji_key = ev.content.relates_to.key.clone();
                let sender_mxid = ev.sender.as_str().to_owned();

                // A first reaction here is a first visit too.
                onboarding::welcome_if_new(&ctx, &room, &ev.sender).await;

                // ── Group selector tap ────────────────────────────────────────
                {
                    let mut state = ctx.state.lock().await;
                    if state.group_selectors.contains_key(&reacted_to) {
                        let reply = onboarding::tap(
                            &ctx,
                            &mut state,
                            &reacted_to,
                            ev.event_id.as_str(),
                            &sender_mxid,
                            &emoji_key,
                        );
                        if let Err(e) = state.save(&ctx.state_path).await {
                            tracing::error!("Failed to save after a group selector tap: {e}");
                        }
                        drop(state);
                        match reply {
                            Ok(Some(text)) => {
                                if let Some(r) = client.get_room(&ctx.room_id) {
                                    onboarding::reply_to_tap(&r, &reacted_to, &text).await;
                                    onboarding::refresh_selectors(&ctx, &r).await;
                                    let (year, week) = state::current_iso_week();
                                    scheduler::refresh_pinned_plan(&ctx, &r, year, week).await;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => tracing::error!("Group selector tap failed: {e}"),
                        }
                        return;
                    }
                }

                if emoji_key != "✅" {
                    return;
                }

                let mut state = ctx.state.lock().await;

                // ── Consolidated weekly plan reaction ─────────────────────────
                if let Some((plan_year, plan_week)) =
                    state.weekly_plan_event_ids.get(&reacted_to).copied()
                {
                    let new_pid = uuid::Uuid::new_v4().to_string();
                    if let Err(e) = state.apply_event(analytics::DomainEvent::PersonCreated {
                        person_id: new_pid,
                        display_name: sender_mxid.clone(),
                        matrix_id: Some(sender_mxid.clone()),
                    }) {
                        tracing::error!("PersonCreated failed in plan reaction: {e}");
                        return;
                    }
                    let sender_person_id = state
                        .person_by_matrix_id(&sender_mxid)
                        .map(|p| p.id.clone())
                        .unwrap_or_else(|| sender_mxid.clone());

                    // The sender's own open turns of that week that have
                    // started (or the next one) — same rule as `!done`.
                    let duties = commands::markable_duties(
                        &state,
                        &sender_person_id,
                        (plan_year, plan_week),
                        None,
                    );
                    let root_eid = ev.content.relates_to.event_id.clone();

                    if duties.is_empty() {
                        drop(state);
                        if let Some(r) = client.get_room(&ctx.room_id) {
                            r.send(thread_reply(
                                "You are not assigned to any open item in this plan.",
                                root_eid.clone(),
                                root_eid,
                            ))
                            .await
                            .ok();
                        }
                        return;
                    }

                    if let Err(e) =
                        commands::mark_duties_done(&mut state, &sender_person_id, &duties)
                    {
                        tracing::error!("Marking plan reaction done failed: {e}");
                    }
                    state.reaction_dones.insert(
                        ev.event_id.to_string(),
                        ReactionDone {
                            group_id: duties[0].group.id.clone(),
                            completed_by_id: sender_person_id.clone(),
                            iso_year: plan_year,
                            iso_week: plan_week,
                            marked: duties
                                .iter()
                                .map(|d| MarkedDuty {
                                    group_id: d.group.id.clone(),
                                    slot_id: d.group.slots.get(d.slot_index).map(|s| s.id.clone()),
                                    shift: d.turn.shift,
                                })
                                .collect(),
                        },
                    );
                    if let Err(e) = state.save(&ctx.state_path).await {
                        tracing::error!("Failed to save after plan reaction: {e}");
                    }
                    drop(state);

                    if let Some(r) = client.get_room(&ctx.room_id) {
                        scheduler::refresh_pinned_plan(&ctx, &r, plan_year, plan_week).await;
                    }
                }
            }
        }
    });

    // ── Redaction handler (undo a ✅ or a group selector tap) ──────────────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        move |ev: OriginalSyncRoomRedactionEvent, room: Room, client: Client| {
            let ctx = ctx.clone();
            async move {
                if room.state() != RoomState::Joined {
                    return;
                }
                if room.room_id() != ctx.room_id {
                    return;
                }

                let redacted_id = match &ev.redacts {
                    Some(id) => id.to_string(),
                    None => return,
                };
                let mut state = ctx.state.lock().await;

                // A group selector tap taken back: undo it.
                match onboarding::untap(&ctx, &mut state, &redacted_id) {
                    Ok(None) => {}
                    Ok(Some((selector_id, text))) => {
                        if let Err(e) = state.save(&ctx.state_path).await {
                            tracing::error!(
                                "Failed to save after undoing a group selector tap: {e}"
                            );
                        }
                        drop(state);
                        if let Some(r) = client.get_room(&ctx.room_id) {
                            onboarding::reply_to_tap(&r, &selector_id, &text).await;
                            onboarding::refresh_selectors(&ctx, &r).await;
                            let (year, week) = state::current_iso_week();
                            scheduler::refresh_pinned_plan(&ctx, &r, year, week).await;
                        }
                        return;
                    }
                    Err(e) => {
                        tracing::error!("Undoing a group selector tap failed: {e}");
                        return;
                    }
                }

                let rd = match state.reaction_dones.remove(&redacted_id) {
                    Some(rd) => rd,
                    None => return,
                };

                let removed = match commands::undo_reaction_done(&mut state, &rd) {
                    Ok(changed) => changed,
                    Err(e) => {
                        tracing::error!("Undoing a ✅ reaction failed: {e}");
                        false
                    }
                };

                if let Err(e) = state.save(&ctx.state_path).await {
                    tracing::error!("Failed to save after reaction removal: {e}");
                }

                if removed {
                    let (undo_year, undo_week) = (rd.iso_year, rd.iso_week);
                    drop(state);
                    if let Some(r) = client.get_room(&ctx.room_id) {
                        scheduler::refresh_pinned_plan(&ctx, &r, undo_year, undo_week).await;
                    }
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
                onboarding::welcome_if_new(&ctx, &room, &ev.state_key).await;
            }
        }
    });

    // ── Initial sync ──────────────────────────────────────────────────────────
    bot.initial_sync().await;
    info!("Initial sync complete");

    // Show real Matrix display names in !status/!groups right away instead
    // of Matrix usernames until each person sends their first command.
    if let Some(room) = client.get_room(&ctx.room_id) {
        commands::refresh_display_names(&ctx, &room).await;
        let mut state = ctx.state.lock().await;
        if let Err(e) = state.save(&ctx.state_path).await {
            error!("Failed to save refreshed display names: {e}");
        }
    }

    // Self-heal the current week's plan message against persisted state
    // before the scheduler loop (or any further event handling) starts, so
    // this can never race a concurrent refresh/announce/tick for the same
    // week. A failure here is logged but never prevents startup.
    scheduler::reconcile_on_startup(&ctx, &client).await;
    // Group selectors catch up with anything that changed while down.
    if let Some(room) = client.get_room(&ctx.room_id) {
        onboarding::refresh_selectors(&ctx, &room).await;
    }

    tokio::spawn(scheduler::run(ctx, client.clone()));

    bot.run().await
}
