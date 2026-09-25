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
                reaction::{OriginalSyncReactionEvent, ReactionEventContent},
                relation::Annotation,
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
mod pdf;
mod pdf_renderer;
mod resolver;
mod schedule;
mod scheduler;
mod state;
mod validate;

use config::Config;
use state::{GreetingChoice, GreetingInfo, ReactionDone, State};

/// Send the group-selection step of the greeting.
///
/// Called either directly for new users (no unlinked non-Matrix persons found)
/// or as the second step after the user has confirmed their identity.
async fn send_join_greeting(ctx: &BotContext, room: &Room, user_id: &str, intro: Option<String>) {
    let groups: Vec<_> = { ctx.state.lock().await.cleaning_groups.clone() };
    if groups.is_empty() {
        return;
    }

    let number_emojis = ["1️⃣", "2️⃣", "3️⃣", "4️⃣", "5️⃣", "6️⃣", "7️⃣", "8️⃣", "9️⃣"];
    let mut choices: Vec<GreetingChoice> = Vec::new();
    let mut lines: Vec<String> = if let Some(h) = intro {
        vec![h, String::new()]
    } else {
        vec![]
    };
    lines.push("Please pick your cleaning group by reacting with the matching number:".to_owned());
    lines.push(String::new());

    for (i, group) in groups.iter().enumerate() {
        let Some(emoji) = number_emojis.get(i) else {
            break;
        };
        let members_text = {
            let state = ctx.state.lock().await;
            let names: Vec<String> = state
                .members_of(group)
                .iter()
                .map(|p| p.display_name.clone())
                .collect();
            if names.is_empty() {
                String::new()
            } else {
                format!(" — {}", names.join(", "))
            }
        };
        lines.push(format!("{emoji} **{}**{members_text}", group.name));
        choices.push(GreetingChoice {
            emoji: emoji.to_string(),
            group_id: group.id.clone(),
            group_name: group.name.clone(),
            person_id: None,
        });
    }

    let content = format::mentionify_rich(&lines.join("\n"), room).await;
    let resp = match room.send(content).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to send join greeting: {e}");
            return;
        }
    };
    let event_id_str = resp.response.event_id.to_string();
    let greeting_eid = resp.response.event_id;

    {
        let mut state = ctx.state.lock().await;
        state.greeting_event_ids.insert(
            event_id_str,
            GreetingInfo {
                for_user: user_id.to_owned(),
                choices: choices.clone(),
                is_linking: false,
            },
        );
        if let Err(e) = state.save(&ctx.state_path).await {
            tracing::error!("Failed to save join greeting: {e}");
        }
    }

    for choice in &choices {
        let reaction =
            ReactionEventContent::new(Annotation::new(greeting_eid.clone(), choice.emoji.clone()));
        if let Err(e) = room.send(reaction).await {
            tracing::warn!("Failed to send self-reaction {}: {e}", choice.emoji);
        }
    }
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

    // Materialize future assignments (idempotent — skips already-stored weeks).
    {
        let mat_events = resolver::materialize(
            &st,
            config.schedule.interval_weeks,
            config.schedule.materialize_weeks as usize,
        );
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
        let config_clone = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(e) = http::run(state_clone, config_clone, &bind_addr).await {
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

                // ── Greeting reaction handler ─────────────────────────────────
                {
                    let mut state = ctx.state.lock().await;
                    if let Some(info) = state.greeting_event_ids.get(&reacted_to).cloned() {
                        if info.for_user == sender_mxid {
                            if let Some(choice) = info.choices.iter().find(|c| c.emoji == emoji_key) {
                                if info.is_linking {
                                    // ── Identity linking step ─────────────────
                                    state.greeting_event_ids.remove(&reacted_to);

                                    if let Some(person_id) = &choice.person_id {
                                        // Link sender to existing non-Matrix person.
                                        let person_id = person_id.clone();
                                        let person_name = choice.group_name.clone();
                                        let _ = state.apply_event(analytics::DomainEvent::PersonMatrixLinked {
                                            person_id: person_id.clone(),
                                            matrix_id: sender_mxid.clone(),
                                        });
                                        let person_groups: Vec<String> = state
                                            .groups_for_person(&person_id)
                                            .iter().map(|g| g.name.clone()).collect();
                                        if let Err(e) = state.save(&ctx.state_path).await {
                                            tracing::error!("Failed to save after identity link: {e}");
                                        }
                                        drop(state);

                                        if let Some(r) = client.get_room(&ctx.room_id) {
                                            if !person_groups.is_empty() {
                                                let msg = format!(
                                                    "✅ Welcome back, **{person_name}**! Your account is linked. You are in: {}",
                                                    person_groups.join(", ")
                                                );
                                                r.send(format::mentionify_rich(&msg, &r).await).await.ok();
                                            } else {
                                                let msg = format!("✅ Linked as **{person_name}**!");
                                                r.send(format::mentionify_rich(&msg, &r).await).await.ok();
                                                send_join_greeting(&ctx, &r, &sender_mxid, None).await;
                                            }
                                        }
                                    } else {
                                        // "I'm new" — skip to group selection.
                                        if let Err(e) = state.save(&ctx.state_path).await {
                                            tracing::error!("Failed to save after 'I'm new': {e}");
                                        }
                                        drop(state);
                                        if let Some(r) = client.get_room(&ctx.room_id) {
                                            send_join_greeting(&ctx, &r, &sender_mxid, None).await;
                                        }
                                    }
                                } else {
                                    // ── Group joining step ────────────────────
                                    let group_id   = choice.group_id.clone();
                                    let group_name = choice.group_name.clone();

                                    let new_pid = uuid::Uuid::new_v4().to_string();
                                    let _ = state.apply_event(analytics::DomainEvent::PersonCreated {
                                        person_id: new_pid,
                                        display_name: sender_mxid.clone(),
                                        matrix_id: Some(sender_mxid.clone()),
                                    });
                                    let person_id = state.person_by_matrix_id(&sender_mxid)
                                        .map(|p| p.id.clone()).unwrap_or_else(|| sender_mxid.clone());
                                    let already = state.group_by_id(&group_id)
                                        .map(|g| g.member_ids.contains(&person_id)).unwrap_or(false);
                                    if !already {
                                        commands::apply_group_join(&ctx, &mut state, &group_id, &person_id).ok();
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
                        }
                        return;
                    }
                }

                if emoji_key != "✅" { return; }

                let mut state = ctx.state.lock().await;
                let interval = ctx.config.schedule.interval_weeks;

                // ── Consolidated weekly plan / final-reminder reaction ─────────
                if let Some((plan_year, plan_week)) = state.weekly_plan_event_ids.get(&reacted_to).copied() {
                    let new_pid = uuid::Uuid::new_v4().to_string();
                    if let Err(e) = state.apply_event(analytics::DomainEvent::PersonCreated {
                        person_id: new_pid, display_name: sender_mxid.clone(), matrix_id: Some(sender_mxid.clone()),
                    }) {
                        tracing::error!("PersonCreated failed in plan reaction: {e}");
                        return;
                    }
                    let sender_person_id = state.person_by_matrix_id(&sender_mxid)
                        .map(|p| p.id.clone()).unwrap_or_else(|| sender_mxid.clone());

                    let groups_to_mark: Vec<_> = state.cleaning_groups.iter()
                        .filter(|g| state.is_due(&g.id, plan_year, plan_week, interval))
                        .filter(|g| {
                            if g.is_multi_slot() {
                                g.slots.iter().enumerate().any(|(i, _)|
                                    state.slot_assignee(g, i, plan_year, plan_week, interval)
                                        .is_some_and(|p| p.id == sender_person_id)
                                )
                            } else {
                                state.responsible_person(g, plan_year, plan_week, interval)
                                    .is_some_and(|p| p.id == sender_person_id)
                            }
                        })
                        .cloned()
                        .collect();

                    let root_eid = ev.content.relates_to.event_id.clone();

                    if groups_to_mark.is_empty() {
                        drop(state);
                        if let Some(r) = client.get_room(&ctx.room_id) {
                            r.send(thread_reply(
                                "You are not assigned to any open item in this plan.",
                                root_eid.clone(), root_eid,
                            )).await.ok();
                        }
                        return;
                    }

                    let mut reaction_done: Option<ReactionDone> = None;
                    for group in &groups_to_mark {
                        if group.is_multi_slot() {
                            for (slot_idx, slot) in group.slots.iter().enumerate() {
                                if state.is_slot_completed(&group.id, &slot.id, plan_year, plan_week) { continue; }
                                if state.slot_assignee(group, slot_idx, plan_year, plan_week, interval)
                                    .is_some_and(|p| p.id == sender_person_id)
                                {
                                    state.apply_event(analytics::DomainEvent::CleaningCompleted {
                                        group_id: group.id.clone(), slot_id: Some(slot.id.clone()),
                                        person_id: sender_person_id.clone(),
                                        responsible_person_ids: vec![sender_person_id.clone()],
                                        iso_year: plan_year, iso_week: plan_week,
                                    }).ok();
                                    reaction_done.get_or_insert_with(|| ReactionDone {
                                        group_id:        group.id.clone(),
                                        completed_by_id: sender_person_id.clone(),
                                        iso_year:        plan_year,
                                        iso_week:        plan_week,
                                    });
                                }
                            }
                        } else {
                            if state.is_completed(&group.id, plan_year, plan_week) { continue; }
                            let resp_ids = state.responsible_person(group, plan_year, plan_week, interval)
                                .map(|p| vec![p.id.clone()]).unwrap_or_default();
                            state.apply_event(analytics::DomainEvent::CleaningCompleted {
                                group_id: group.id.clone(), slot_id: None,
                                person_id: sender_person_id.clone(),
                                responsible_person_ids: resp_ids,
                                iso_year: plan_year, iso_week: plan_week,
                            }).ok();
                            reaction_done.get_or_insert_with(|| ReactionDone {
                                group_id:        group.id.clone(),
                                completed_by_id: sender_person_id.clone(),
                                iso_year:        plan_year,
                                iso_week:        plan_week,
                            });
                        }
                    }

                    if let Some(rd) = reaction_done {
                        state.reaction_dones.insert(ev.event_id.to_string(), rd);
                    }
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

    // ── Redaction handler (undo ✅ reaction) ──────────────────────────────────
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
                let rd = match state.reaction_dones.remove(&redacted_id) {
                    Some(rd) => rd,
                    None => return,
                };

                let before = state.completions.len();
                state.completions.retain(|c| {
                    !(c.group_id == rd.group_id
                        && c.iso_year == rd.iso_year
                        && c.iso_week == rd.iso_week
                        && c.completed_by_id == rd.completed_by_id)
                });
                let removed = state.completions.len() < before;

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

                // Check for non-Matrix persons who might be this user.
                let unlinked: Vec<_> = {
                    let state = ctx.state.lock().await;
                    state.persons.iter()
                        .filter(|p| p.active && p.matrix_id.is_none())
                        .cloned()
                        .collect()
                };

                if !unlinked.is_empty() {
                    // Phase 1: ask who they are.
                    let number_emojis = ["1️⃣","2️⃣","3️⃣","4️⃣","5️⃣","6️⃣","7️⃣","8️⃣","9️⃣"];
                    let mut choices: Vec<GreetingChoice> = Vec::new();
                    let mut lines = vec![
                        format!("👋 Welcome, {user_id}!"),
                        String::new(),
                        "Are you already on the cleaning plan? React with your name, or 🆕 if you are a new person:".to_owned(),
                        String::new(),
                    ];
                    for (i, person) in unlinked.iter().enumerate() {
                        let Some(emoji) = number_emojis.get(i) else { break };
                        lines.push(format!("{emoji} **{}**", person.display_name));
                        choices.push(GreetingChoice {
                            emoji:      emoji.to_string(),
                            group_id:   String::new(),
                            group_name: person.display_name.clone(),
                            person_id:  Some(person.id.clone()),
                        });
                    }
                    lines.push(String::new());
                    lines.push("🆕 I'm a new person".to_owned());
                    choices.push(GreetingChoice {
                        emoji:      "🆕".to_string(),
                        group_id:   String::new(),
                        group_name: String::new(),
                        person_id:  None,
                    });

                    let content = format::mentionify_rich(&lines.join("\n"), &room).await;
                    let resp = match room.send(content).await {
                        Ok(r) => r,
                        Err(e) => { tracing::error!("Failed to send linking greeting: {e}"); return; }
                    };
                    let event_id_str = resp.response.event_id.to_string();
                    let greeting_eid = resp.response.event_id;

                    {
                        let mut state = ctx.state.lock().await;
                        state.greeting_event_ids.insert(event_id_str, GreetingInfo {
                            for_user:   user_id.clone(),
                            choices:    choices.clone(),
                            is_linking: true,
                        });
                        if let Err(e) = state.save(&ctx.state_path).await {
                            tracing::error!("Failed to save linking greeting: {e}");
                        }
                    }

                    for choice in &choices {
                        let reaction = ReactionEventContent::new(Annotation::new(greeting_eid.clone(), choice.emoji.clone()));
                        if let Err(e) = room.send(reaction).await {
                            tracing::warn!("Failed to send self-reaction {}: {e}", choice.emoji);
                        }
                    }
                } else {
                    // No unlinked persons — go straight to group selection.
                    send_join_greeting(
                        &ctx, &room, &user_id,
                        Some(format!("👋 Welcome, {user_id}!")),
                    ).await;
                }
                let _ = client;
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

    tokio::spawn(scheduler::run(ctx, client.clone()));

    bot.run().await
}
