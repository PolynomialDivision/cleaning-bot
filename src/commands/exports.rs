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

// ── !plan pdf style [group] [days | tick | default] ──────────────────────

/// Which sheet `!plan pdf` prints, for everyone or one group: anyone may
/// ask, administrators change it.
pub(crate) async fn cmd_pdf_style(
    ctx: &BotContext,
    sender: &OwnedUserId,
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
    use crate::paper::Style;
    const USAGE: &str = "!plan pdf style [group] days | tick | default";
    if args.is_empty() {
        let state = ctx.state.lock().await;
        let mut lines = vec![
            format!("📄 Printable plan: {}", state.paper_style.describe()),
            String::new(),
        ];
        for g in &state.cleaning_groups {
            let own = state.paper_styles.get(&g.id);
            lines.push(format!(
                "• {}: {}{}",
                g.name,
                state.paper_style_for(&g.id).name(),
                if own.is_some() { "" } else { " (as everyone)" }
            ));
        }
        lines.push(String::new());
        lines.push(format!("days · {}", Style::Days.describe()));
        lines.push(format!("tick · {}", Style::Tick.describe()));
        lines.push(USAGE.into());
        return Ok(Some(format::mentionify(&lines.join("\n"))));
    }
    // The style is the last word; anything before it names a group.
    let (choice, group) = args.split_last().expect("not empty");
    let style = match choice.to_lowercase().as_str() {
        "default" | "reset" if !group.is_empty() => None,
        word => match Style::parse(word) {
            Some(style) => Some(style),
            None => {
                return Ok(Some(format::mentionify(&format!(
                    "📄 Unknown style «{choice}». Use {USAGE}."
                ))))
            }
        },
    };
    require_admin(ctx, sender)?;
    let mut state = ctx.state.lock().await;
    let reply = if group.is_empty() {
        let style = style.expect("a style for everyone");
        state.paper_style = style;
        let own: Vec<&str> = state
            .cleaning_groups
            .iter()
            .filter(|g| state.paper_styles.contains_key(&g.id))
            .map(|g| g.name.as_str())
            .collect();
        format!(
            "📄 Printable plan from now on: {}{}{}",
            style.describe(),
            if own.is_empty() {
                String::new()
            } else {
                format!("\nExcept for {}, which have their own.", own.join(", "))
            },
            tick_note(style)
        )
    } else {
        let name = group.join(" ");
        let Some(g) = state.group_by_name(&name) else {
            return Ok(Some(format::mentionify(&group_not_found(&name))));
        };
        let (id, name) = (g.id.clone(), g.name.clone());
        match style {
            Some(style) => {
                state.paper_styles.insert(id, style);
                format!(
                    "📄 Printable plan for {name} from now on: {}{}",
                    style.describe(),
                    tick_note(style)
                )
            }
            None => {
                state.paper_styles.remove(&id);
                format!(
                    "📄 {name} prints like everyone again: {}",
                    state.paper_style.describe()
                )
            }
        }
    };
    state.save(&ctx.state_path).await?;
    Ok(Some(format::mentionify(&reply)))
}

fn tick_note(style: crate::paper::Style) -> &'static str {
    match style {
        crate::paper::Style::Tick => {
            "\nA tick counts as done in the middle of its days \
             (Thursday for a whole week), or the day of the photo if that is earlier."
        }
        crate::paper::Style::Days => "",
    }
}

// ── !plan pdf [view] [history | next] [N] [group] ─────────────────────────────────────────────

pub(crate) async fn cmd_pdf(
    ctx: &BotContext,
    sender: &OwnedUserId,
    room: &Room,
    args: &[&str],
    answer_to: Option<Answer>,
) -> Result<Option<RoomMessageEventContent>> {
    if args
        .first()
        .is_some_and(|a| a.eq_ignore_ascii_case("style"))
    {
        return cmd_pdf_style(ctx, sender, &args[1..]).await;
    }
    // `view`: the same plan only to look at, nothing to tick.
    let view = args.first().is_some_and(|a| a.eq_ignore_ascii_case("view"));
    let args = if view { &args[1..] } else { args };
    // `history`: the weeks up to this one; `next`: from next week on.
    let history = args
        .first()
        .is_some_and(|a| a.eq_ignore_ascii_case("history"));
    let from_next = args.first().is_some_and(|a| a.eq_ignore_ascii_case("next"));
    let args = if history || from_next {
        &args[1..]
    } else {
        args
    };
    // !plan pdf [history | next] [weeks] [group name]
    // First arg: either a number (weeks) or start of group name.
    let weeks_given = args.first().is_some_and(|s| s.parse::<usize>().is_ok());
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
            // Printable plan: one full page per group (see below);
            // history: the last 8 weeks.
            (
                if history {
                    8
                } else {
                    crate::paper::ROWS_PER_PAGE
                },
                name,
            )
        }
    };

    // Refresh Matrix display names so the PDF shows "Thomas" not "thomas99"
    // — as the cleaning room knows them, also when asked in a private chat.
    crate::names::refresh(
        ctx,
        &room.client(),
        room.client().get_room(&ctx.room_id).as_ref(),
    )
    .await;

    let (document, history_tex, file_name) = {
        let state = ctx.state.lock().await;
        let (y, w) = current_iso_week();
        let mut snapshot = if history {
            crate::schedule::build_schedule_from(&state, add_weeks(y, w, -(n as i64 - 1)), n)
        } else if from_next {
            crate::schedule::build_schedule_from(&state, add_weeks(y, w, 1), n)
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
        if !history && !weeks_given {
            crate::paper::one_page_per_group(&mut snapshot, &state);
        }
        if snapshot.is_empty() {
            return Ok(Some(format::mentionify("📄 No duties in this date range.")));
        }
        let document = if view {
            crate::paper::view(&state, &snapshot)
        } else {
            crate::paper::document(&state, &snapshot)
        };
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
        let kind = if view && !history {
            "cleaning-plan-view"
        } else {
            "cleaning-plan"
        };
        let file_name = match &group_filter {
            Some(name) => format!(
                "{kind}-{}_{}.pdf",
                name.to_lowercase().replace(' ', "_"),
                weeks_range
            ),
            None => format!("{kind}-{weeks_range}.pdf"),
        };
        (
            document,
            history.then(|| crate::pdf::render_tex(&snapshot)),
            file_name,
        )
    };

    // Render .tex → PDF via tectonic.
    let rendered = if let Some(tex) = history_tex {
        crate::pdf_renderer::tex_to_pdf(&tex).await
    } else {
        crate::paper::pdf(&document).await
    };
    let pdf_bytes = match rendered {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("tectonic render failed: {e}");
            return Ok(Some(format::mentionify(
                "❌ PDF could not be generated. Please ask an admin to check the renderer.",
            )));
        }
    };

    // Persist mapping before a scannable PDF can leave the bot (a view
    // can't be scanned: nothing to keep).
    if !history && !document.view_only {
        let mut state = ctx.state.lock().await;
        state.paper_documents.insert(document.id.clone(), document);
        state.save(&ctx.state_path).await?;
    }

    let mime: mime::Mime = "application/pdf".parse().expect("valid mime");
    room.send_attachment(
        file_name,
        &mime,
        pdf_bytes,
        matrix_sdk::attachment::AttachmentConfig::new()
            .mentions(Some(matrix_sdk::ruma::events::Mentions::new()))
            .extra_content(answer_to.map(relation_content)),
    )
    .await?;
    Ok(None)
}

/// `m.relates_to` making a file the same kind of answer as a text reply
/// (see `answer_relation` in `main`). Passed as extra content rather than an
/// SDK `Reply`, which would first fetch the event answered — and lose the
/// file if that fails. The SDK still encrypts the event and the upload in
/// encrypted rooms.
fn relation_content(relation: Answer) -> serde_json::Map<String, serde_json::Value> {
    let mut content = RoomMessageEventContent::text_plain("");
    content.relates_to = Some(relation);
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
            "🗓 !ical [weeks] shows only your own calendar. !ical reset replaces its link.",
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
             !ical shows it again; !ical reset replaces it."
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
    args: &[&str],
) -> Result<Option<RoomMessageEventContent>> {
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
    use matrix_sdk::ruma::{events::relation::Reply, owned_event_id};

    #[test]
    fn a_pdf_is_the_same_kind_of_answer_as_a_text_reply() {
        let reply = relation_content(Relation::Reply(Reply::with_event_id(owned_event_id!(
            "$command"
        ))));
        assert_eq!(
            serde_json::Value::Object(reply),
            serde_json::json!({ "m.relates_to": { "m.in_reply_to": { "event_id": "$command" } } })
        );
        let thread = relation_content(Relation::Thread(
            matrix_sdk::ruma::events::relation::Thread::reply(
                owned_event_id!("$root"),
                owned_event_id!("$command"),
            ),
        ));
        assert_eq!(
            serde_json::Value::Object(thread),
            serde_json::json!({ "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": "$root",
                "m.in_reply_to": { "event_id": "$command" },
            }})
        );
    }
}
