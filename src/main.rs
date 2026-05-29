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
mod domain;
mod format;
mod http;
mod ical;
mod pdf;
mod schedule;
mod scheduler;
mod state;
mod validate;

use config::Config;
use domain::Person;
use state::{Completion, GreetingChoice, GreetingInfo, ReactionDone, State};

fn add_thread_relation(content: &mut RoomMessageEventContent, root: OwnedEventId, reply_to: OwnedEventId) {
    content.relates_to = Some(Relation::Thread(Thread::reply(root, reply_to)));
}

fn thread_reply(text: &str, root: OwnedEventId, reply_to: OwnedEventId) -> RoomMessageEventContent {
    let mut content = format::mentionify(text);
    add_thread_relation(&mut content, root, reply_to);
    content
}

#[derive(Clone)]
pub struct BotContext {
    pub state:      Arc<Mutex<State>>,
    pub state_path: PathBuf,
    pub config:     Arc<Config>,
    pub admin_users: HashSet<OwnedUserId>,
    pub room_id:    OwnedRoomId,
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
    if st.created_at.is_none() {
        st.created_at = Some(chrono::Utc::now());
        st.save(&state_path).await?;
    }

    // Validate on startup — log issues but never abort.
    {
        let report = validate::validate_state(&st);
        if !report.errors.is_empty() {
            tracing::error!("State validation errors on startup:\n{}", report.summary());
        } else if !report.warnings.is_empty() {
            tracing::warn!("State validation warnings on startup:\n{}", report.summary());
        } else {
            tracing::info!("State validation passed.");
        }
    }

    let state = Arc::new(Mutex::new(st));

    let admin_users: HashSet<OwnedUserId> = config.security.admin_users
        .iter().filter_map(|s| s.parse().ok()).collect();
    let allowed_inviters: HashSet<String> = config.security.allowed_inviters.iter().cloned().collect();
    let room_id: OwnedRoomId = config.schedule.room_id.parse().context("Invalid room_id in [schedule]")?;

    let ctx = BotContext { state: state.clone(), state_path, config: Arc::clone(&config), admin_users, room_id: room_id.clone() };

    // ── Optional HTTP iCal server ─────────────────────────────────────────────
    if let Some(ref ical_cfg) = config.ical_server {
        let bind_addr = ical_cfg.bind_addr.clone();
        let state_clone  = state.clone();
        let config_clone = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(e) = http::run(state_clone, config_clone, &bind_addr).await {
                error!("iCal HTTP server error: {e}");
            }
        });
    }

    let (client, bot_user_id) = mxbot_common::session::build_and_restore(
        &config.matrix, &store_path, config.security.encryption_strategy.clone().into(),
    ).await?;

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
                let cmd_lines: Vec<&str> = body.lines().map(str::trim).filter(|l| l.starts_with('!')).collect();
                if cmd_lines.is_empty() { return; }

                let thread_root = match &ev.content.relates_to {
                    Some(Relation::Thread(t)) => t.event_id.clone(),
                    _ => ev.event_id.clone(),
                };

                let mut replies: Vec<RoomMessageEventContent> = Vec::new();
                for line in cmd_lines {
                    match commands::handle(&ctx, &ev.sender, &room, line).await {
                        Ok(Some(reply)) => replies.push(reply),
                        Err(e) if e.to_string() == "__not_admin__" =>
                            replies.push(format::mentionify("❌ This command requires admin privileges.")),
                        Ok(None) => {}
                        Err(e)   => error!("Command error: {e}"),
                    }
                }
                if !replies.is_empty() {
                    if let Some(r) = client.get_room(&ctx.room_id) {
                        let mut content = if replies.len() == 1 {
                            replies.remove(0)
                        } else {
                            let joined = replies.iter()
                                .filter_map(|c| if let MessageType::Text(t) = &c.msgtype { Some(t.body.as_str()) } else { None })
                                .collect::<Vec<_>>().join("\n\n");
                            format::mentionify(&joined)
                        };
                        add_thread_relation(&mut content, thread_root, ev.event_id.clone());
                        r.send(content).await.ok();
                    }
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
                if ev.sender == bot_user_id { return; }
                if room.state() != RoomState::Joined { return; }
                if room.room_id() != ctx.room_id { return; }

                let reacted_to = ev.content.relates_to.event_id.to_string();
                let emoji_key  = ev.content.relates_to.key.clone();
                let sender_mxid = ev.sender.as_str().to_owned();

                // ── Greeting number-reaction → join group ─────────────────────
                {
                    let mut state = ctx.state.lock().await;
                    if let Some(info) = state.greeting_event_ids.get(&reacted_to).cloned() {
                        if info.for_user == sender_mxid {
                            if let Some(choice) = info.choices.iter().find(|c| c.emoji == emoji_key) {
                                let group_id   = choice.group_id.clone();
                                let group_name = choice.group_name.clone();

                                // Find or create Person for this MXID.
                                let person_id = {
                                    if let Some(p) = state.person_by_matrix_id(&sender_mxid) {
                                        p.id.clone()
                                    } else {
                                        let p = Person::new_matrix(&sender_mxid);
                                        let id = p.id.clone();
                                        state.persons.push(p);
                                        id
                                    }
                                };

                                if let Some(group) = state.cleaning_groups.iter_mut().find(|g| g.id == group_id) {
                                    if !group.member_ids.contains(&person_id) {
                                        group.member_ids.push(person_id);
                                    }
                                }

                                if let Err(e) = state.save(&ctx.state_path).await {
                                    tracing::error!("Failed to save after greeting join: {e}");
                                }
                                state.greeting_event_ids.remove(&reacted_to);
                                drop(state);

                                if let Some(r) = client.get_room(&ctx.room_id) {
                                    let msg = format!(
                                        "✅ {sender_mxid} joined **{group_name}** — welcome to the cleaning crew! 🧹"
                                    );
                                    r.send(format::mentionify_rich(&msg, &r).await).await.ok();
                                }
                            }
                        }
                        return;
                    }
                }

                if emoji_key != "✅" { return; }

                let mut state = ctx.state.lock().await;
                let group_id = match state.reminder_event_ids.get(&reacted_to).cloned() {
                    Some(id) => id,
                    None     => return,
                };

                let (year, week) = state::current_iso_week();
                let interval = ctx.config.schedule.interval_weeks;

                if state.is_completed(&group_id, year, week) {
                    drop(state);
                    if let Some(r) = client.get_room(&ctx.room_id) {
                        let group_name = {
                            let s = ctx.state.lock().await;
                            s.group_by_id(&group_id).map(|g| g.name.clone()).unwrap_or_default()
                        };
                        let msg = format!("«{group_name}» is already marked as cleaned this week.");
                        let rid = ev.content.relates_to.event_id.clone();
                        r.send(thread_reply(&msg, rid.clone(), rid)).await.ok();
                    }
                    return;
                }

                // Find or create sender Person.
                let sender_person_id = {
                    if let Some(p) = state.person_by_matrix_id(&sender_mxid) {
                        p.id.clone()
                    } else {
                        let p = Person::new_matrix(&sender_mxid);
                        let id = p.id.clone();
                        state.persons.push(p);
                        id
                    }
                };

                let responsible_ids = state.cleaning_groups.iter()
                    .find(|g| g.id == group_id)
                    .and_then(|g| state.responsible_person(g, year, week, interval))
                    .map(|p| vec![p.id.clone()])
                    .unwrap_or_default();

                let group_name = state.group_by_id(&group_id).map(|g| g.name.clone()).unwrap_or_default();

                state.completions.push(Completion {
                    group_id:               group_id.clone(),
                    completed_by_id:        sender_person_id.clone(),
                    responsible_person_ids: responsible_ids,
                    iso_year:     year,
                    iso_week:     week,
                    completed_at: chrono::Utc::now(),
                    skipped:      false,
                    unit_name:    String::new(),
                    completed_by: String::new(),
                    responsible_users: Vec::new(),
                });
                state.reaction_dones.insert(
                    ev.event_id.to_string(),
                    ReactionDone {
                        group_id:        group_id.clone(),
                        completed_by_id: sender_person_id,
                        iso_year:  year,
                        iso_week:  week,
                        unit_name:    String::new(),
                        completed_by: String::new(),
                    },
                );
                if let Err(e) = state.save(&ctx.state_path).await {
                    tracing::error!("Failed to save state after reaction: {e}");
                }
                let root_eid = ev.content.relates_to.event_id.clone();
                drop(state);

                if let Some(r) = client.get_room(&ctx.room_id) {
                    let msg = format!("✅ {sender_mxid} marked «{group_name}» as cleaned.");
                    r.send(thread_reply(&msg, root_eid.clone(), root_eid)).await.ok();
                }
            }
        }
    });

    // ── Redaction handler (undo ✅ reaction) ──────────────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        move |ev: OriginalSyncRoomRedactionEvent, room: Room, client: Client| {
            let ctx = ctx.clone();
            async move {
                if room.state() != RoomState::Joined { return; }
                if room.room_id() != ctx.room_id     { return; }

                let redacted_id = match &ev.redacts { Some(id) => id.to_string(), None => return };
                let mut state   = ctx.state.lock().await;
                let rd = match state.reaction_dones.remove(&redacted_id) { Some(rd) => rd, None => return };

                let before = state.completions.len();
                state.completions.retain(|c| {
                    !(c.group_id == rd.group_id && c.iso_year == rd.iso_year && c.iso_week == rd.iso_week
                      && c.completed_by_id == rd.completed_by_id)
                });
                let removed = state.completions.len() < before;

                if let Err(e) = state.save(&ctx.state_path).await {
                    tracing::error!("Failed to save after reaction removal: {e}");
                }

                if removed {
                    let by = state.person_by_id(&rd.completed_by_id)
                        .map(|p| p.display_name.clone())
                        .unwrap_or_else(|| rd.completed_by_id.clone());
                    let group_name = state.group_by_id(&rd.group_id)
                        .map(|g| g.name.clone())
                        .unwrap_or_else(|| rd.group_id.clone());
                    drop(state);
                    if let Some(r) = client.get_room(&ctx.room_id) {
                        let msg = format!("↩️ {by} removed their ✅ — «{group_name}» marked undone.");
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
                if room.room_id() != ctx.room_id { return; }
                if room.state() != RoomState::Joined { return; }
                if ev.content.membership != MembershipState::Join { return; }
                if ev.state_key == bot_user_id { return; }
                if let Some(prev) = ev.prev_content() {
                    if prev.membership == MembershipState::Join { return; }
                }

                let user_id = ev.state_key.to_string();

                {
                    let mut state = ctx.state.lock().await;
                    if state.greeted_users.contains(&user_id) { return; }
                    state.greeted_users.insert(user_id.clone());
                    if let Err(e) = state.save(&ctx.state_path).await {
                        tracing::error!("Failed to save greeted_users: {e}");
                    }
                }

                let groups: Vec<_> = {
                    let state = ctx.state.lock().await;
                    state.cleaning_groups.clone()
                };
                if groups.is_empty() { return; }

                let number_emojis = ["1️⃣","2️⃣","3️⃣","4️⃣","5️⃣","6️⃣","7️⃣","8️⃣","9️⃣"];
                let mut choices: Vec<GreetingChoice> = Vec::new();
                let mut lines = vec![
                    format!("👋 Welcome, {user_id}!"),
                    String::new(),
                    "Please pick your cleaning group by reacting with the matching number:".to_owned(),
                    String::new(),
                ];

                for (i, group) in groups.iter().enumerate() {
                    let Some(emoji) = number_emojis.get(i) else { break };
                    let members_text = {
                        let state = ctx.state.lock().await;
                        let names: Vec<String> = state.members_of(group).iter().map(|p| p.display_name.clone()).collect();
                        if names.is_empty() { String::new() } else { format!(" — {}", names.join(", ")) }
                    };
                    lines.push(format!("{emoji} **{}**{members_text}", group.name));
                    choices.push(GreetingChoice {
                        emoji:      emoji.to_string(),
                        group_id:   group.id.clone(),
                        group_name: group.name.clone(),
                    });
                }

                let content = format::mentionify_rich(&lines.join("\n"), &room).await;
                let resp = match room.send(content).await {
                    Ok(r) => r,
                    Err(e) => { tracing::error!("Failed to send greeting: {e}"); return; }
                };
                let event_id_str = resp.response.event_id.to_string();
                let greeting_eid = resp.response.event_id;

                {
                    let mut state = ctx.state.lock().await;
                    state.greeting_event_ids.insert(event_id_str, GreetingInfo { for_user: user_id.clone(), choices: choices.clone() });
                    if let Err(e) = state.save(&ctx.state_path).await {
                        tracing::error!("Failed to save greeting_event_ids: {e}");
                    }
                }

                for choice in &choices {
                    let reaction = ReactionEventContent::new(Annotation::new(greeting_eid.clone(), choice.emoji.clone()));
                    if let Err(e) = room.send(reaction).await {
                        tracing::warn!("Failed to send self-reaction {}: {e}", choice.emoji);
                    }
                }
                let _ = client;
            }
        }
    });

    // ── Verification handler ──────────────────────────────────────────────────
    client.add_event_handler({
        let reset_allowed: Arc<Mutex<HashSet<OwnedUserId>>> = Arc::new(Mutex::new(HashSet::new()));
        move |ev: ToDeviceKeyVerificationRequestEvent, client: Client| {
            let reset = Arc::clone(&reset_allowed);
            async move {
                if let Some(req) = client.encryption()
                    .get_verification_request(&ev.sender, &ev.content.transaction_id).await
                {
                    tokio::spawn(mxbot_common::verify::handle_verification_request(client, reset, req));
                }
            }
        }
    });

    // ── Initial sync ──────────────────────────────────────────────────────────
    client.sync_once(SyncSettings::default().filter(FilterDefinition::with_lazy_loading().into()))
        .await.context("Initial sync failed")?;
    info!("Initial sync complete");

    tokio::spawn(scheduler::run(ctx, client.clone()));

    loop {
        match client.sync(SyncSettings::default()).await {
            Ok(())  => warn!("Sync loop exited — reconnecting"),
            Err(e)  => { warn!("Sync error: {e} — reconnecting in 5s"); tokio::time::sleep(tokio::time::Duration::from_secs(5)).await; }
        }
    }
}
