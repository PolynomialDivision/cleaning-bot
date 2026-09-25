//! Plan exports: !cleanplan, !pdf, !ical, !icalreset.

use super::*;

// ── !cleanplan [N] ────────────────────────────────────────────────────────────

pub(crate) async fn cmd_cleanplan(
    ctx: &BotContext,
    _sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    let n: usize = args
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6)
        .clamp(1, 20);

    let (snapshot, is_empty) = {
        let state = ctx.state.lock().await;
        let interval = ctx.config.schedule.interval_weeks;
        let empty = state.cleaning_groups.is_empty();
        (build_schedule(&state, interval, n), empty)
    };

    if is_empty {
        return Ok(Some(format::mentionify(
            "No cleaning groups configured yet.",
        )));
    }

    // Pre-fetch Matrix display names for all assignees and completers.
    let all_mxids: Vec<String> = {
        let mut ids = Vec::new();
        for a in &snapshot.assignments {
            if let Some(mxid) = a.assignee_mxid() {
                if !ids.contains(&mxid.to_owned()) {
                    ids.push(mxid.to_owned());
                }
            }
            if let Some(by) = &a.completed_by {
                // completed_by is a display_name, look it up
                if !ids.contains(by) {
                    let state = ctx.state.lock().await;
                    if let Some(p) = state.find_person(by) {
                        if let Some(m) = &p.matrix_id {
                            if !ids.contains(m) {
                                ids.push(m.clone());
                            }
                        }
                    }
                }
            }
        }
        ids
    };
    let uid_refs: Vec<&str> = all_mxids.iter().map(String::as_str).collect();
    let names = format::fetch_names(room, &uid_refs).await;

    let interval = snapshot.interval_weeks;
    let (cur_y, cur_w) = current_iso_week();

    let mut lines = vec![format!(
        "📅 **Cleaning plan** · next {n} week{} · every {interval} week{}",
        if n == 1 { "" } else { "s" },
        if interval == 1 { "" } else { "s" }
    )];

    for (dy, dw) in snapshot.weeks() {
        let is_cur = (dy, dw) == (cur_y, cur_w);
        lines.push(String::new());
        if is_cur {
            lines.push(format!(
                "📆 **Week {dw} ({})** ← this week",
                week_dates(dy, dw)
            ));
        } else {
            lines.push(format!("📆 Week {dw} ({})", week_dates(dy, dw)));
        }
        for a in snapshot.for_group_in_week(dy, dw) {
            let icon = if a.is_completed {
                "✅"
            } else if is_cur {
                "🔲"
            } else {
                "🗓"
            };
            let detail = if a.is_completed {
                if a.is_skipped {
                    "skipped ⏭️".into()
                } else {
                    let by = a.completed_by.as_deref().unwrap_or("?");
                    format!("done by {by}")
                }
            } else {
                match &a.assignee {
                    None => "nobody assigned yet".into(),
                    Some(p) => {
                        let key = p.mxid.as_deref().unwrap_or(&p.name);
                        let state = ctx.state.lock().await;
                        let away = state.is_absent(&p.id, &a.group_id, dy, dw);
                        drop(state);
                        if away {
                            format!("{key} (away)")
                        } else {
                            key.to_owned()
                        }
                    }
                }
            };
            lines.push(format!("  {icon} {} : {detail}", a.group_name));
        }
    }

    Ok(Some(format::mentionify_with_names(
        &lines.join("\n"),
        &names,
    )))
}

// ── Admin: !pdf [N] ───────────────────────────────────────────────────────────

pub(crate) async fn cmd_pdf(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
    event_id: OwnedEventId,
    thread_root: OwnedEventId,
) -> Result<Option<RoomMessageEventContent>> {
    require_admin(ctx, sender)?;
    // !pdf [weeks] [group name]
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

    // Refresh Matrix display names so the PDF shows "Thomas" not "thomas99".
    refresh_display_names(ctx, room).await;

    let (tex, file_name) = {
        let state = ctx.state.lock().await;
        let interval = ctx.config.schedule.interval_weeks;
        let mut snapshot = build_schedule(&state, interval, n);
        if let Some(ref name) = group_filter {
            match state.group_by_name(name) {
                Some(g) => {
                    let gid = g.id.clone();
                    snapshot.assignments.retain(|a| a.group_id == gid);
                }
                None => {
                    return Ok(Some(RoomMessageEventContent::text_plain(format!(
                        "Group «{name}» not found."
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
            return Ok(Some(format::mentionify(&format!(
                "❌ PDF render failed: {e}"
            ))));
        }
    };

    let mime: mime::Mime = "application/pdf".parse().expect("valid mime");
    let pdf_size = pdf_bytes.len();
    match room.client().media().upload(&mime, pdf_bytes, None).await {
        Ok(upload) => {
            let mut file_info = FileInfo::new();
            file_info.mimetype = Some("application/pdf".to_owned());
            file_info.size = UInt::new(pdf_size as u64);
            let mut fc = FileMessageEventContent::plain(file_name, upload.content_uri);
            fc.info = Some(Box::new(file_info));
            let mut file_content = RoomMessageEventContent::new(MessageType::File(fc));
            file_content.relates_to =
                Some(matrix_sdk::ruma::events::room::message::Relation::Thread(
                    Thread::reply(thread_root.clone(), event_id.clone()),
                ));
            room.send(file_content).await.ok();
            Ok(Some(format::mentionify("📄 Schedule generated.")))
        }
        Err(e) => {
            tracing::warn!("PDF upload failed: {e}");
            Ok(Some(format::mentionify("❌ Upload failed.")))
        }
    }
}

// ── !ical [N] / !ical <person> [N] ────────────────────────────────────────────

pub(crate) async fn cmd_ical(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    let sender_mxid = sender.as_str();
    let is_admin = ctx.admin_users.contains(sender);

    // Parse target person and week count.
    let (person_id, weeks): (String, usize) = {
        let state = ctx.state.lock().await;
        match args.first() {
            None => {
                let pid = match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
                    Some(id) => id,
                    None => {
                        return Ok(Some(format::mentionify(&format!(
                            "You ({sender_mxid}) are not registered. Ask an admin to use !adduser."
                        ))))
                    }
                };
                (pid, 26)
            }
            Some(first) => {
                if let Ok(n) = first.parse::<usize>() {
                    let pid = match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
                        Some(id) => id,
                        None => {
                            return Ok(Some(format::mentionify(&format!(
                                "You ({sender_mxid}) are not registered."
                            ))))
                        }
                    };
                    (pid, n.clamp(1, 104))
                } else {
                    if !is_admin {
                        return Ok(Some(format::mentionify(
                            "❌ Admin permission required to generate iCal for others.",
                        )));
                    }
                    let person = match state.find_person(first).cloned() {
                        Some(p) => p,
                        None => {
                            return Ok(Some(format::mentionify(&format!(
                                "Person «{first}» not found."
                            ))))
                        }
                    };
                    let n = args
                        .get(1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(26usize)
                        .clamp(1, 104);
                    (person.id.clone(), n)
                }
            }
        }
    };

    // If HTTP server is configured, return URL (token-based feed).
    if let Some(ical_cfg) = &ctx.config.ical_server {
        let mut state = ctx.state.lock().await;

        // Check if a non-revoked token already exists for this person.
        let has_token = state
            .calendar_tokens
            .iter()
            .any(|ct| !ct.revoked && ct.person_id == person_id);

        if has_token {
            return Ok(Some(format::mentionify(
                "📅 You already have an active calendar feed.\n\
                 Use !icalreset to get a new URL (this invalidates the old subscription).",
            )));
        }

        let (raw_token, hash) = new_calendar_token();
        state.calendar_tokens.push(CalendarToken {
            id: Uuid::new_v4().to_string(),
            token_hash: hash,
            person_id: person_id.clone(),
            created_at: Utc::now(),
            revoked: false,
        });
        state.save(&ctx.state_path).await?;
        drop(state);

        let url = format!("{}/ical/{raw_token}.ics", ical_cfg.public_url);
        return Ok(Some(format::mentionify(&format!(
            "📅 Your calendar feed URL:\n{url}\n\n\
             Add this URL to your calendar app for automatic updates.\n\
             ⚠️ This URL is shown only once — save it!"
        ))));
    }

    // Fallback: generate and upload as a Matrix file attachment.
    let ical_data = {
        let state = ctx.state.lock().await;
        let interval = ctx.config.schedule.interval_weeks;
        let snapshot = build_schedule(&state, interval, weeks);
        crate::ical::render_ics(&snapshot, &person_id)
    };

    let safe_name = person_id
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or("user")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();

    let mime: mime::Mime = "text/calendar".parse().expect("valid mime");
    match room
        .client()
        .media()
        .upload(&mime, ical_data.into_bytes(), None)
        .await
    {
        Ok(upload) => {
            use matrix_sdk::ruma::events::room::message::{FileMessageEventContent, MessageType};
            let content =
                RoomMessageEventContent::new(MessageType::File(FileMessageEventContent::plain(
                    format!("putzplan_{safe_name}.ics"),
                    upload.content_uri,
                )));
            room.send(content).await.ok();
            Ok(Some(format::mentionify(&format!(
                "📅 iCal · {weeks} Wochen · Import .ics into your calendar app.\n\
                 Tip: configure [ical_server] in config.toml for live-updating feed URLs."
            ))))
        }
        Err(e) => {
            tracing::warn!("iCal upload failed: {e}");
            Ok(Some(format::mentionify("❌ Upload failed.")))
        }
    }
}

// ── !icalreset [person] ───────────────────────────────────────────────────────

pub(crate) async fn cmd_icalreset(
    ctx: &BotContext,
    sender: &OwnedUserId,
    _room: &Room,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    let sender_mxid = sender.as_str();
    let is_admin = ctx.admin_users.contains(sender);

    let Some(ical_cfg) = &ctx.config.ical_server else {
        return Ok(Some(format::mentionify(
            "iCal HTTP server is not configured. Add [ical_server] to config.toml.",
        )));
    };

    let person_id: String = {
        let state = ctx.state.lock().await;
        match args.first() {
            None => match state.person_by_matrix_id(sender_mxid).map(|p| p.id.clone()) {
                Some(id) => id,
                None => {
                    return Ok(Some(format::mentionify(&format!(
                        "{sender_mxid} is not registered."
                    ))))
                }
            },
            Some(query) => {
                if !is_admin {
                    return Ok(Some(format::mentionify("❌ Admin permission required.")));
                }
                match state.find_person(query).map(|p| p.id.clone()) {
                    Some(id) => id,
                    None => {
                        return Ok(Some(format::mentionify(&format!(
                            "Person «{query}» not found."
                        ))))
                    }
                }
            }
        }
    };

    let mut state = ctx.state.lock().await;
    // Revoke all existing tokens for this person.
    for ct in state
        .calendar_tokens
        .iter_mut()
        .filter(|ct| ct.person_id == person_id)
    {
        ct.revoked = true;
    }

    // Issue new token.
    let (raw_token, hash) = new_calendar_token();
    state.calendar_tokens.push(CalendarToken {
        id: Uuid::new_v4().to_string(),
        token_hash: hash,
        person_id: person_id.clone(),
        created_at: Utc::now(),
        revoked: false,
    });
    state.save(&ctx.state_path).await?;
    drop(state);

    let url = format!("{}/ical/{raw_token}.ics", ical_cfg.public_url);
    Ok(Some(format::mentionify(&format!(
        "🔄 New calendar feed URL:\n{url}\n\n\
         ⚠️ Old URLs for this person are now invalid."
    ))))
}
