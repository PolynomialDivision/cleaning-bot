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
    Ok(Some(format::mentionify(&plan_text(&state, n))))
}

/// The next `n` weeks, week by week — everyone by name, so looking at the
/// plan never pings anyone. This week's lines show their status
/// (⬜ ✅ ⏭️ ❌); later weeks are plain bullets.
pub(crate) fn plan_text(state: &crate::state::State, n: usize) -> String {
    let groups: Vec<&CleaningGroup> = state
        .cleaning_groups
        .iter()
        .filter(|g| g.is_active)
        .collect();
    if groups.is_empty() {
        return "No cleaning groups configured yet.".into();
    }
    let current = current_iso_week();
    let mut lines = vec![format!(
        "📅 **Next {}**",
        crate::view::plural(n, "week", "weeks")
    )];
    for i in 0..n as i64 {
        let (y, w) = add_weeks(current.0, current.1, i);
        let mut week_lines = Vec::new();
        for group in &groups {
            for turn in state.turns_in_week(group, y, w) {
                for (slot_index, assignee) in state.turn_assignees(group, turn) {
                    let what = Duty {
                        group: (*group).clone(),
                        slot_index,
                        turn,
                    }
                    .label();
                    let who = assignee.map_or("nobody assigned", crate::view::name);
                    let away = assignee
                        .filter(|p| state.is_absent(&p.id, &group.id, y, w))
                        .map_or("", |_| " 🌴 away");
                    let line = match state.completion_for(group, slot_index, turn) {
                        Some(c) if c.skipped => format!("⏭️ {what}: {who} · skipped"),
                        Some(c) => {
                            let by = (assignee.map(|p| &p.id) != Some(&c.completed_by_id))
                                .then(|| state.person_by_id(&c.completed_by_id))
                                .flatten()
                                .map(|p| format!(" · done by {}", p.display_name))
                                .unwrap_or_default();
                            format!("✅ {what}: {who}{by}")
                        }
                        None if state.turn_over(group, turn) => {
                            format!("❌ {what}: {who} · missed")
                        }
                        None if i == 0 => format!("⬜ {what}: {who}{away}"),
                        None => format!("• {what}: {who}{away}"),
                    };
                    week_lines.push(line);
                }
            }
        }
        if week_lines.is_empty() {
            continue;
        }
        lines.push(String::new());
        let now = if i == 0 { " · this week" } else { "" };
        lines.push(format!("**{}**{now}", crate::view::week_label(y, w)));
        lines.extend(week_lines);
    }
    lines.join("\n")
}

// ── Admin: !plan pdf [N] ───────────────────────────────────────────────────────────

pub(crate) async fn cmd_pdf(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
    event_id: OwnedEventId,
    thread_root: OwnedEventId,
) -> Result<Option<RoomMessageEventContent>> {
    require_admin(ctx, sender)?;
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

    // Refresh Matrix display names so the PDF shows "Thomas" not "thomas99".
    refresh_display_names(ctx, room).await;

    let (tex, file_name) = {
        let state = ctx.state.lock().await;
        let mut snapshot = build_schedule(&state, n);
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
                        "You ({sender_mxid}) are not registered. Join a group with !join <group>."
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
                    let person = match lookup_person(&state, first) {
                        Ok(Some(p)) => p.clone(),
                        Err(ambiguous) => return Ok(Some(format::mentionify(&ambiguous))),
                        Ok(None) => {
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
                 Use !ical reset to get a new URL (this invalidates the old subscription).",
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
        let snapshot = build_schedule(&state, weeks);
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

// ── !ical reset [person] ───────────────────────────────────────────────────────

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
                match lookup_person(&state, query) {
                    Ok(Some(p)) => p.id.clone(),
                    Err(ambiguous) => return Ok(Some(format::mentionify(&ambiguous))),
                    Ok(None) => {
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
