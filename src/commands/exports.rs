//! Plan exports: !plan [N], !plan pdf, !ical, !ical reset.

use super::*;

// ── !plan [N] ────────────────────────────────────────────────────────────

pub(crate) async fn cmd_cleanplan(
    ctx: &BotContext,
    _sender: &OwnedUserId,
    _room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    let n: usize = args
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6)
        .clamp(1, 20);
    let state = ctx.state.lock().await;
    Ok(Some(plan_message(&state, n)))
}

/// `!plan` as sent: user pills, but nobody in `m.mentions`.
pub(crate) fn plan_message(state: &crate::state::State, n: usize) -> RoomMessageEventContent {
    format::intentional(format::mentionify(&plan_text(state, n)))
}

/// The next `n` weeks, week by week — everyone as a user pill that pings
/// nobody (`view::user_link`). This week's lines show their status
/// (⬜ ✅ ⏭️ ❌); later weeks are plain bullets.
pub(crate) fn plan_text(state: &crate::state::State, n: usize) -> String {
    let snapshot = build_schedule(state, n);
    if snapshot.is_empty() {
        return "No active cleaning groups configured yet.".into();
    }
    let mut lines = vec![format!(
        "📋 **Plan · next {}**",
        crate::view::plural(n, "week", "weeks")
    )];
    let mut week = None;
    for a in &snapshot.assignments {
        if week != Some((a.iso_year, a.iso_week)) {
            lines.push(format!(
                "\n📅 **{}**",
                crate::view::week_label(a.iso_year, a.iso_week)
            ));
            week = Some((a.iso_year, a.iso_week));
        }
        lines.push(a.matrix_line(true, false));
    }
    lines.join("\n")
}

// ── !plan pdf [history] [N] [group] ─────────────────────────────────────────────

pub(crate) async fn cmd_pdf(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
    event_id: OwnedEventId,
    thread_root: OwnedEventId,
) -> Result<Option<RoomMessageEventContent>> {
    let _ = sender;
    let history = args.first() == Some(&"history");
    let args = if history { &args[1..] } else { args };
    // !plan pdf [weeks] [group name]
    // First arg: either a number (weeks) or start of group name.
    let (n, group_filter) = {
        let weeks = args.first().and_then(|s| s.parse::<usize>().ok());
        if let Some(w) = weeks {
            let name = if args.len() > 1 {
                Some(args[1..].join(" "))
            } else {
                None
            };
            (w.clamp(1, 52), name)
        } else {
            let name = if !args.is_empty() {
                Some(args.join(" "))
            } else {
                None
            };
            (8, name)
        }
    };

    // Refresh Matrix display names so the PDF shows "Thomas" not "thomas99"
    // — as the cleaning room knows them, also when asked in a private chat.
    if let Some(main) = room.client().get_room(&ctx.room_id) {
        refresh_display_names(ctx, &main).await;
    }

    let (tex, file_name) = {
        let state = ctx.state.lock().await;
        let mut snapshot = if history {
            let (y, w) = current_iso_week();
            crate::schedule::build_schedule_from(&state, add_weeks(y, w, -(n as i64 - 1)), n)
        } else {
            build_schedule(&state, n)
        };
        if let Some(ref name) = group_filter {
            match state.group_by_name(name) {
                Some(g) => {
                    let gid = g.id.clone();
                    snapshot.assignments.retain(|a| a.group_id == gid);
                }
                None => {
                    return Ok(Some(RoomMessageEventContent::text_plain(group_not_found(
                        name,
                    ))))
                }
            }
        }
        let tex = crate::pdf::render_tex(&snapshot);
        // Build filename: cleaning-plan-KW{first}-KW{last}.pdf
        let weeks_range = {
            let first = snapshot.assignments.first();
            let last = snapshot.assignments.last();
            match (first, last) {
                (Some(f), Some(l)) if (f.iso_year, f.iso_week) != (l.iso_year, l.iso_week) => {
                    format!("KW{}-KW{}", f.iso_week, l.iso_week)
                }
                (Some(f), _) => format!("KW{}", f.iso_week),
                _ => format!("{n}w"),
            }
        };
        let file_name = match &group_filter {
            Some(name) => format!(
                "cleaning-plan-{}_{}.pdf",
                name.to_lowercase().replace(' ', "_"),
                weeks_range
            ),
            None => format!("cleaning-plan-{weeks_range}.pdf"),
        };
        (tex, file_name)
    };

    // Render .tex → PDF via tectonic.
    let pdf_bytes = match crate::pdf_renderer::tex_to_pdf(&tex).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("tectonic render failed: {e}");
            return Ok(Some(format::mentionify(
                "❌ PDF could not be generated. Please ask an admin to check the renderer.",
            )));
        }
    };

    let mime: mime::Mime = "application/pdf".parse().expect("valid mime");
    room.send_attachment(
        file_name,
        &mime,
        pdf_bytes,
        matrix_sdk::attachment::AttachmentConfig::new()
            .mentions(Some(matrix_sdk::ruma::events::Mentions::new()))
            .extra_content(Some(thread_relation(thread_root, event_id))),
    )
    .await?;
    Ok(None)
}

/// `m.relates_to` putting a file into the same thread as a text reply
/// (`in_thread`). Passed as extra content rather than an SDK `Reply`, which
/// would first fetch the command event — and lose the file if that fails.
/// The SDK still encrypts the event and the upload in encrypted rooms.
fn thread_relation(
    root: OwnedEventId,
    reply_to: OwnedEventId,
) -> serde_json::Map<String, serde_json::Value> {
    let content =
        mxbot_common::send::in_thread(RoomMessageEventContent::text_plain(""), root, reply_to);
    let mut json = serde_json::to_value(content).expect("message content serializes");
    let mut extra = serde_json::Map::new();
    extra.insert("m.relates_to".into(), json["m.relates_to"].take());
    extra
}

// ── !ical [N] / !ical <person> [N] ────────────────────────────────────────────

pub(crate) async fn cmd_ical(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    if !crate::private::authorized(ctx, room, sender).await {
        return Ok(Some(format::intentional(format::mentionify(
            "🗓 Send !ical in an encrypted private chat with me. Calendar links are private.",
        ))));
    }
    let sender_mxid = sender.as_str();
    {
        let mut state = ctx.state.lock().await;
        state.apply_event(DomainEvent::PersonCreated {
            person_id: Uuid::new_v4().to_string(),
            display_name: sender_mxid.into(),
            matrix_id: Some(sender_mxid.into()),
        })?;
        state.save(&ctx.state_path).await?;
    }
    if args.len() > 1 || args.first().is_some_and(|a| a.parse::<usize>().is_err()) {
        return Ok(Some(format::mentionify(
            "🗓 !ical [weeks] shows only your calendar. !ical reset replaces its private link.",
        )));
    }
    let weeks = args
        .first()
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(26)
        .clamp(1, 104);
    let person_id = ctx
        .state
        .lock()
        .await
        .person_by_matrix_id(sender_mxid)
        .unwrap()
        .id
        .clone();

    // If HTTP server is configured, return URL (token-based feed).
    if let Some(ical_cfg) = &ctx.config.ical_server {
        let mut state = ctx.state.lock().await;

        let raw_token = crate::private::calendar_token(&mut state, &person_id);
        state.save(&ctx.state_path).await?;
        drop(state);

        let url = crate::private::feed_url(ical_cfg, &raw_token);
        return Ok(Some(format::mentionify(&format!(
            "📅 Your calendar feed URL:\n{url}\n\n\
             Add this URL to your calendar app for automatic updates.\n\
             Keep this URL private. !ical shows it again; !ical reset replaces it."
        ))));
    }

    // Fallback: generate and upload as a Matrix file attachment.
    let ical_data = {
        let state = ctx.state.lock().await;
        let snapshot = build_schedule(&state, weeks);
        crate::ical::render_ics(&snapshot, &person_id)
    };

    let mime: mime::Mime = "text/calendar".parse().expect("valid mime");
    room.send_attachment(
        format!("cleaning-dates-{weeks}-weeks.ics"),
        &mime,
        ical_data.into_bytes(),
        matrix_sdk::attachment::AttachmentConfig::new()
            .mentions(Some(matrix_sdk::ruma::events::Mentions::new())),
    )
    .await?;
    Ok(None)
}

// ── !ical reset [person] ───────────────────────────────────────────────────────

pub(crate) async fn cmd_icalreset(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    if !crate::private::authorized(ctx, room, sender).await {
        return Ok(Some(format::intentional(format::mentionify(
            "🗓 Send !ical in an encrypted private chat with me. Calendar links are private.",
        ))));
    }
    let sender_mxid = sender.as_str();
    {
        let mut state = ctx.state.lock().await;
        state.apply_event(DomainEvent::PersonCreated {
            person_id: Uuid::new_v4().to_string(),
            display_name: sender_mxid.into(),
            matrix_id: Some(sender_mxid.into()),
        })?;
        state.save(&ctx.state_path).await?;
    }
    if !args.is_empty() {
        return Ok(Some(format::mentionify(
            "🗓 !ical reset replaces only your own calendar link.",
        )));
    }
    let Some(ical_cfg) = &ctx.config.ical_server else {
        return Ok(Some(format::mentionify(
            "🗓 Live subscriptions are unavailable. !ical gives you a calendar file.",
        )));
    };
    let person_id = ctx
        .state
        .lock()
        .await
        .person_by_matrix_id(sender_mxid)
        .unwrap()
        .id
        .clone();

    let mut state = ctx.state.lock().await;
    // Revoke all existing tokens for this person; their secrets go too.
    for ct in state
        .calendar_tokens
        .iter_mut()
        .filter(|ct| ct.person_id == person_id)
    {
        ct.revoked = true;
        ct.raw_token = None;
    }

    // Issue new token.
    let (raw_token, hash) = new_calendar_token();
    state.calendar_tokens.push(CalendarToken {
        id: Uuid::new_v4().to_string(),
        raw_token: Some(raw_token.clone()),
        token_hash: hash,
        person_id: person_id.clone(),
        created_at: Utc::now(),
        revoked: false,
    });
    let url = crate::private::feed_url(ical_cfg, &raw_token);
    for selector in state
        .group_selectors
        .values_mut()
        .filter(|s| s.user_id == sender_mxid && s.calendar_url.is_some())
    {
        selector.calendar_url = Some(url.clone());
    }
    state.save(&ctx.state_path).await?;
    drop(state);

    Ok(Some(format::mentionify(&format!(
        "🔄 New calendar feed URL:\n{url}\n\n\
         ⚠️ Old URLs for this person are now invalid."
    ))))
}

/// Revoke only: administrators may revoke someone else's feed, never retrieve it.
pub(crate) async fn cmd_icalrevoke(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    let mut state = ctx.state.lock().await;
    let person = if args.is_empty() {
        state.person_by_matrix_id(sender.as_str())
    } else {
        require_admin(ctx, sender)?;
        match lookup_person(&state, &args.join(" ")) {
            Ok(p) => p,
            Err(e) => return Ok(Some(format::mentionify(&e))),
        }
    };
    let Some(person) = person.cloned() else {
        return Ok(Some(format::mentionify("No calendar found.")));
    };
    for token in state
        .calendar_tokens
        .iter_mut()
        .filter(|t| t.person_id == person.id)
    {
        token.revoked = true;
        token.raw_token = None;
    }
    for selector in state
        .group_selectors
        .values_mut()
        .filter(|s| Some(s.user_id.as_str()) == person.matrix_id.as_deref())
    {
        selector.calendar_url = None;
    }
    state.save(&ctx.state_path).await?;
    Ok(Some(format::intentional(format::mentionify(
        "🗓 Calendar links revoked. Existing subscriptions no longer work.",
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::owned_event_id;

    #[test]
    fn a_pdf_lands_in_the_same_thread_as_a_text_reply() {
        let relation = thread_relation(owned_event_id!("$root"), owned_event_id!("$command"));
        assert_eq!(
            serde_json::Value::Object(relation),
            serde_json::json!({ "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": "$root",
                "m.in_reply_to": { "event_id": "$command" },
            }})
        );
    }
}
