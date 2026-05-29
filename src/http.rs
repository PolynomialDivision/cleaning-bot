//! HTTP server serving per-person iCal feeds.
//!
//! Route: `GET /ical/<64-hex-token>.ics`
//!
//! Request flow:
//!   1. Validate token (hash comparison — no name lookups)
//!   2. Build ScheduleSnapshot (pure, from locked state)
//!   3. Render ICS (pure, from snapshot)
//!   4. Set ETag (SHA-256 of ICS body) and Last-Modified (state.last_modified)
//!
//! The raw token is never stored — only its SHA-256 hash lives in state.json.

use std::sync::Arc;
use axum::{
    Router,
    extract::{Path, State as AxumState},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use sha2::{Sha256, Digest};
use tokio::{net::TcpListener, sync::Mutex};
use tracing::info;

use crate::{
    config::Config,
    domain::verify_calendar_token,
    ical::render_ics,
    schedule::build_schedule,
    state::State,
};

struct AppState {
    state:  Arc<Mutex<State>>,
    config: Arc<Config>,
}

pub async fn run(
    state:     Arc<Mutex<State>>,
    config:    Arc<Config>,
    bind_addr: &str,
) -> anyhow::Result<()> {
    let shared = Arc::new(AppState { state, config });
    let app = Router::new()
        .route("/ical/:token_ics", get(serve_ical))
        .with_state(shared);

    let listener = TcpListener::bind(bind_addr).await?;
    info!("iCal HTTP server listening on {bind_addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_ical(
    Path(token_ics): Path<String>,
    AxumState(app): AxumState<Arc<AppState>>,
) -> Response {
    let token = token_ics.trim_end_matches(".ics");

    let state = app.state.lock().await;

    // Token validation: strict hash comparison — no name-based fallbacks.
    let person_id = state.calendar_tokens.iter()
        .find(|ct| !ct.revoked && verify_calendar_token(token, &ct.token_hash))
        .map(|ct| ct.person_id.clone());

    let Some(person_id) = person_id else {
        return (StatusCode::NOT_FOUND, "Token not found or revoked.\n").into_response();
    };

    // Build schedule snapshot (pure computation, no mutations).
    let interval = app.config.schedule.interval_weeks;
    let snapshot = build_schedule(&state, interval, 52);

    // Capture Last-Modified timestamp before dropping the lock.
    let last_modified = snapshot.state_timestamp;
    drop(state);

    // Render ICS (pure function over snapshot).
    let ics_body = render_ics(&snapshot, &person_id);

    // ETag = SHA-256 of the ICS body (deterministic for unchanged state).
    let etag = format!("\"{}\"", hex::encode(Sha256::digest(ics_body.as_bytes())));
    // Last-Modified in RFC 7231 format.
    let last_mod_str = last_modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE,        "text/calendar; charset=utf-8")
        .header(header::CONTENT_DISPOSITION, "inline; filename=\"cleaning.ics\"")
        .header(header::ETAG,                etag)
        .header(header::LAST_MODIFIED,       last_mod_str)
        .header("Cache-Control",             "private, no-cache, must-revalidate")
        .body(axum::body::Body::from(ics_body))
        .unwrap()
}
