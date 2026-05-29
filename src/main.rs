use std::{collections::HashSet, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use matrix_sdk::{
    Client, Room, RoomState,
    config::SyncSettings,
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedServerName, OwnedUserId, RoomOrAliasId,
        api::client::filter::FilterDefinition,
        events::{
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            reaction::{OriginalSyncReactionEvent, ReactionEventContent},
            relation::{Annotation, Thread},
            room::{
                member::{MembershipState, OriginalSyncRoomMemberEvent, StrippedRoomMemberEvent},
                message::{
                    MessageType, OriginalSyncRoomMessageEvent,
                    Relation, RoomMessageEventContent,
                },
                redaction::OriginalSyncRoomRedactionEvent,
            },
        },
    },
};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

mod commands;
mod config;
mod format;
mod pdf;
mod scheduler;
mod state;

use config::Config;
use state::{PersonRef, State};

/// Attach an `m.thread` relation to an already-built `RoomMessageEventContent`.
/// `root`     — the thread root event (m.thread event_id).
/// `reply_to` — the specific event being quoted (m.in_reply_to).
fn add_thread_relation(content: &mut RoomMessageEventContent, root: OwnedEventId, reply_to: OwnedEventId) {
    content.relates_to = Some(Relation::Thread(Thread::reply(root, reply_to)));
}

/// Wrap a plain-text reply in an `m.thread` relation, mentionifying any MXIDs.
/// `root`     — the thread root event (m.thread event_id).
/// `reply_to` — the specific event being quoted (m.in_reply_to).
fn thread_reply(text: &str, root: OwnedEventId, reply_to: OwnedEventId) -> RoomMessageEventContent {
    let mut content = format::mentionify(text);
    add_thread_relation(&mut content, root, reply_to);
    content
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cleaning_bot=info,matrix_sdk=warn".parse().unwrap()),
        )
        .init();

    let config_path = std::env::args()
        .find(|a| a.ends_with(".toml"))
        .unwrap_or_else(|| "config.toml".to_owned());
    let config: Config = toml::from_str(
        &std::fs::read_to_string(&config_path)
            .with_context(|| format!("Reading config {config_path}"))?,
    )
    .context("Parsing config")?;
    let config = Arc::new(config);

    let store_path = PathBuf::from(
        std::env::var("STORE_PATH").unwrap_or_else(|_| "store".to_owned()),
    );
    tokio::fs::create_dir_all(&store_path).await?;

    let state_path = store_path.join("state.json");
    let mut st = State::load(&state_path).await?;

    // Record when the bot first started — used as the statistics baseline.
    if st.created_at.is_none() {
        st.created_at = Some(chrono::Utc::now());
        st.save(&state_path).await?;
    }

    let state = Arc::new(Mutex::new(st));

    let admin_users: HashSet<OwnedUserId> = config.security.admin_users
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    let allowed_inviters: HashSet<String> = config.security.allowed_inviters
        .iter()
        .cloned()
        .collect();

    let room_id: OwnedRoomId = config.schedule.room_id
        .parse()
        .context("Invalid room_id in [schedule]")?;

    let ctx = BotContext {
        state,
        state_path,
        config: Arc::clone(&config),
        admin_users,
        room_id: room_id.clone(),
    };

    let (client, bot_user_id) = mxbot_common::session::build_and_restore(
        &config.matrix,
        &store_path,
        config.security.encryption_strategy.clone().into(),
    )
    .await?;

    // ── Invite handler ────────────────────────────────────────────────────────
    client.add_event_handler({
        let allowed_inviters = allowed_inviters.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: StrippedRoomMemberEvent, room: Room, client: Client| {
            let allowed_inviters = allowed_inviters.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                if ev.state_key != bot_user_id { return; }
                if !allowed_inviters.is_empty() && !allowed_inviters.contains(ev.sender.as_str()) {
                    warn!("Rejecting invite from {}", ev.sender);
                    room.leave().await.ok();
                    return;
                }
                let room_id = room.room_id().to_owned();
                let mut via: Vec<OwnedServerName> = vec![ev.sender.server_name().to_owned()];
                if let Some(s) = room_id.server_name() {
                    let s = s.to_owned();
                    if !via.contains(&s) { via.push(s); }
                }
                if let Ok(roa) = RoomOrAliasId::parse(room_id.as_str()) {
                    if let Err(e) = client.join_room_by_id_or_alias(&roa, &via).await {
                        warn!("Join failed: {e}");
                    }
                }
            }
        }
    });

    // ── Message / command handler ─────────────────────────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room, client: Client| {
            let ctx = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                if ev.sender == bot_user_id { return; }
                if room.state() != RoomState::Joined { return; }
                if room.room_id() != ctx.room_id { return; }

                let MessageType::Text(ref text) = ev.content.msgtype else { return; };
                let body = text.body.trim();
                let cmd_lines: Vec<&str> = body
                    .lines()
                    .map(str::trim)
                    .filter(|l| l.starts_with('!'))
                    .collect();
                if cmd_lines.is_empty() { return; }

                // If the command was sent inside an existing thread, stay in
                // that thread. Otherwise start a new thread rooted here.
                let thread_root = match &ev.content.relates_to {
                    Some(Relation::Thread(t)) => t.event_id.clone(),
                    _ => ev.event_id.clone(),
                };

                let mut replies: Vec<RoomMessageEventContent> = Vec::new();
                for line in cmd_lines {
                    match commands::handle(&ctx, &ev.sender, &room, line).await {
                        Ok(Some(reply)) => replies.push(reply),
                        Err(e) if e.to_string() == "__not_admin__" => {
                            replies.push(format::mentionify(
                                "❌ This command requires admin privileges.",
                            ));
                        }
                        Ok(None) => {}
                        Err(e) => error!("Command error: {e}"),
                    }
                }
                if !replies.is_empty() {
                    if let Some(r) = client.get_room(&ctx.room_id) {
                        let mut content = if replies.len() == 1 {
                            replies.remove(0)
                        } else {
                            // Multiple commands in one message: join plain bodies.
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
                        add_thread_relation(&mut content, thread_root, ev.event_id.clone());
                        r.send(content).await.ok();
                    }
                }
            }
        }
    });

    // ── Reaction handler (✅ on a reminder = !done; number on greeting = !joinfloor) ──
    client.add_event_handler({
        let ctx = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncReactionEvent, room: Room, client: Client| {
            let ctx = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                if ev.sender == bot_user_id { return; }
                if room.state() != RoomState::Joined { return; }
                if room.room_id() != ctx.room_id { return; }

                let reacted_to = ev.content.relates_to.event_id.to_string();
                let emoji_key  = ev.content.relates_to.key.clone();
                let sender_str = ev.sender.as_str().to_owned();

                // ── Greeting number-reaction → join floor ─────────────────────
                {
                    let mut state = ctx.state.lock().await;
                    if let Some(info) = state.greeting_event_ids.get(&reacted_to).cloned() {
                        // Only the greeted user's reaction counts.
                        if info.for_user == sender_str {
                            if let Some(choice) = info.choices.iter().find(|c| c.emoji == emoji_key) {
                                // Add sender to all floors in this unit.
                                let floor_names = choice.floor_names.clone();
                                let unit_name   = choice.unit_name.clone();
                                for floor_name in &floor_names {
                                    if let Some(floor) = state.floors.iter_mut().find(|f| &f.name == floor_name) {
                                        if !floor.members.iter().any(|p| p.matches_str(&sender_str)) {
                                            floor.members.push(PersonRef::Matrix { mxid: sender_str.clone() });
                                        }
                                    }
                                }
                                if let Err(e) = state.save(&ctx.state_path).await {
                                    tracing::error!("Failed to save after greeting join: {e}");
                                }
                                // Remove greeting so further reactions are ignored.
                                state.greeting_event_ids.remove(&reacted_to);
                                drop(state);

                                if let Some(r) = client.get_room(&ctx.room_id) {
                                    let msg = format!(
                                        "✅ {sender_str} joined **{unit_name}** — welcome to the cleaning crew! 🧹"
                                    );
                                    r.send(format::mentionify_rich(&msg, &r).await).await.ok();
                                }
                            }
                        }
                        return; // don't process as a ✅ cleaning reaction
                    }
                }

                if emoji_key != "✅" { return; }

                let mut state = ctx.state.lock().await;

                let unit_name = match state.reminder_event_ids.get(&reacted_to).cloned() {
                    Some(n) => n,
                    None => return, // reaction on something unrelated
                };

                let (year, week) = state::current_iso_week();

                if state.is_completed(&unit_name, year, week) {
                    // Already marked — acknowledge but don't duplicate
                    drop(state);
                    if let Some(r) = client.get_room(&ctx.room_id) {
                        let msg = format!("«{unit_name}» is already marked as cleaned this week.");
                        let reminder_id = ev.content.relates_to.event_id.clone();
                        r.send(thread_reply(&msg, reminder_id.clone(), reminder_id)).await.ok();
                    }
                    return;
                }

                let responsible_users = {
                    let units = state.units();
                    let interval = ctx.config.schedule.interval_weeks;
                    units.iter()
                        .find(|u| u.name == unit_name)
                        .and_then(|u| state.responsible_user(u, year, week, interval))
                        .map(|p| vec![p.display().to_owned()])
                        .unwrap_or_default()
                };
                state.completions.push(state::Completion {
                    unit_name: unit_name.clone(),
                    iso_year: year,
                    iso_week: week,
                    completed_at: chrono::Utc::now(),
                    completed_by: sender_str.clone(),
                    responsible_users,
                    skipped: false,
                });
                // Track the reaction event ID so we can auto-undo if the
                // user removes their ✅ reaction later.
                state.reaction_dones.insert(
                    ev.event_id.to_string(),
                    state::ReactionDone {
                        unit_name:    unit_name.clone(),
                        iso_year:     year,
                        iso_week:     week,
                        completed_by: sender_str.clone(),
                    },
                );
                if let Err(e) = state.save(&ctx.state_path).await {
                    tracing::error!("Failed to save state after reaction: {e}");
                }
                let root_event_id = ev.content.relates_to.event_id.clone();
                drop(state);

                if let Some(r) = client.get_room(&ctx.room_id) {
                    let msg = format!("✅ {sender_str} marked «{unit_name}» as cleaned.");
                    r.send(thread_reply(&msg, root_event_id.clone(), root_event_id)).await.ok();
                }
            }
        }
    });

    // ── Redaction handler (remove ✅ reaction = !undo) ────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        move |ev: OriginalSyncRoomRedactionEvent, room: Room, client: Client| {
            let ctx = ctx.clone();
            async move {
                if room.state() != RoomState::Joined  { return; }
                if room.room_id() != ctx.room_id      { return; }

                let redacted_id = match &ev.redacts {
                    Some(id) => id.to_string(),
                    None     => return, // malformed redaction with no target
                };
                let mut state   = ctx.state.lock().await;

                let rd = match state.reaction_dones.remove(&redacted_id) {
                    Some(rd) => rd,
                    None     => return, // wasn't a tracked reaction-done
                };

                // Remove the matching completion.
                let before = state.completions.len();
                state.completions.retain(|c| {
                    !(c.unit_name    == rd.unit_name
                      && c.iso_year == rd.iso_year
                      && c.iso_week == rd.iso_week
                      && c.completed_by == rd.completed_by)
                });
                let removed = state.completions.len() < before;

                if let Err(e) = state.save(&ctx.state_path).await {
                    tracing::error!("Failed to save state after reaction removal: {e}");
                }
                drop(state);

                if removed {
                    if let Some(r) = client.get_room(&ctx.room_id) {
                        let msg = format!(
                            "↩️ {} removed their ✅ — «{}» marked undone.",
                            rd.completed_by, rd.unit_name,
                        );
                        r.send(format::mentionify_rich(&msg, &r).await).await.ok();
                    }
                }
            }
        }
    });

    // ── Member-join handler (greet new users) ─────────────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncRoomMemberEvent, room: Room, client: Client| {
            let ctx = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                // Only care about our designated room.
                if room.room_id() != ctx.room_id { return; }
                if room.state() != RoomState::Joined { return; }

                // Only new joins, not profile updates or re-joins we already handled.
                if ev.content.membership != MembershipState::Join { return; }
                if ev.state_key == bot_user_id { return; }

                // Skip if this is merely a profile update (display-name / avatar change).
                // A genuine join has prev_content = None or prev membership != Join.
                if let Some(prev) = ev.prev_content() {
                    if prev.membership == MembershipState::Join {
                        return; // was already joined — just a profile update
                    }
                }

                let user_id = ev.state_key.to_string();

                // Don't greet the same user twice.
                {
                    let mut state = ctx.state.lock().await;
                    if state.greeted_users.contains(&user_id) {
                        return;
                    }
                    state.greeted_users.insert(user_id.clone());
                    if let Err(e) = state.save(&ctx.state_path).await {
                        tracing::error!("Failed to save greeted_users: {e}");
                    }
                }

                // Build the list of cleaning units available to join.
                let units = {
                    let state = ctx.state.lock().await;
                    state.units()
                };
                if units.is_empty() { return; }

                // Number emojis 1️⃣ … 9️⃣ (we support up to 9 floors/groups).
                let number_emojis = [
                    "1️⃣", "2️⃣", "3️⃣", "4️⃣", "5️⃣", "6️⃣", "7️⃣", "8️⃣", "9️⃣",
                ];

                let mut choices: Vec<state::GreetingChoice> = Vec::new();
                let mut lines: Vec<String> = Vec::new();
                lines.push(format!("👋 Welcome, {user_id}!"));
                lines.push(String::new());
                lines.push(
                    "Please pick the floor/group you want to join for cleaning duty \
                     by reacting with the matching number below:".to_owned()
                );
                lines.push(String::new());

                for (i, unit) in units.iter().enumerate() {
                    let Some(emoji) = number_emojis.get(i) else { break };
                    let members_text = if unit.members.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", unit.members.iter().map(|p| p.display()).collect::<Vec<_>>().join(", "))
                    };
                    lines.push(format!("{emoji} **{}**{members_text}", unit.name));
                    choices.push(state::GreetingChoice {
                        emoji: emoji.to_string(),
                        unit_name: unit.name.clone(),
                        floor_names: unit.floor_names.clone(),
                    });
                }

                let msg = lines.join("\n");
                let content = format::mentionify_rich(&msg, &room).await;

                let resp = match room.send(content).await {
                    Ok(r) => r,
                    Err(e) => { tracing::error!("Failed to send greeting: {e}"); return; }
                };
                let greeting_event_id = resp.response.event_id;
                let event_id_str = greeting_event_id.to_string();

                // Store the greeting info so the reaction handler can act on it.
                {
                    let mut state = ctx.state.lock().await;
                    state.greeting_event_ids.insert(
                        event_id_str,
                        state::GreetingInfo {
                            for_user: user_id.clone(),
                            choices: choices.clone(),
                        },
                    );
                    if let Err(e) = state.save(&ctx.state_path).await {
                        tracing::error!("Failed to save greeting_event_ids: {e}");
                    }
                }

                // Self-react with each number emoji so users can just click.
                for choice in &choices {
                    let reaction = ReactionEventContent::new(
                        Annotation::new(
                            greeting_event_id.clone(),
                            choice.emoji.clone(),
                        )
                    );
                    if let Err(e) = room.send(reaction).await {
                        tracing::warn!("Failed to send self-reaction {}: {e}", choice.emoji);
                    }
                }

                // Announce the new member in the room if there's a client handle.
                let _ = client;
            }
        }
    });

    // ── Verification handler ──────────────────────────────────────────────────
    client.add_event_handler({
        let reset_allowed: Arc<Mutex<HashSet<OwnedUserId>>> =
            Arc::new(Mutex::new(HashSet::new()));
        move |ev: ToDeviceKeyVerificationRequestEvent, client: Client| {
            let reset = Arc::clone(&reset_allowed);
            async move {
                if let Some(req) = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.content.transaction_id)
                    .await
                {
                    tokio::spawn(mxbot_common::verify::handle_verification_request(
                        client, reset, req,
                    ));
                }
            }
        }
    });

    // ── Initial sync ──────────────────────────────────────────────────────────
    let filter = FilterDefinition::with_lazy_loading();
    client
        .sync_once(SyncSettings::default().filter(filter.into()))
        .await
        .context("Initial sync failed")?;
    info!("Initial sync complete");

    // ── Scheduler ─────────────────────────────────────────────────────────────
    tokio::spawn(scheduler::run(ctx, client.clone()));

    // ── Continuous sync ───────────────────────────────────────────────────────
    loop {
        match client.sync(SyncSettings::default()).await {
            Ok(()) => warn!("Sync loop exited — reconnecting"),
            Err(e) => {
                warn!("Sync error: {e} — reconnecting in 5s");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    }
}
