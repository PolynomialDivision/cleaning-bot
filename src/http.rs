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
//! Authentication compares hashes. Recoverable token secrets are private state.

use axum::{
    extract::{Path, State as AxumState},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::{net::TcpListener, sync::Mutex};
use tracing::info;

use crate::{
    domain::verify_calendar_token, ical::render_ics, schedule::build_schedule, state::State,
};

struct AppState {
    state: Arc<Mutex<State>>,
}

pub async fn run(state: Arc<Mutex<State>>, bind_addr: &str) -> anyhow::Result<()> {
    let shared = Arc::new(AppState { state });
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
    headers: HeaderMap,
) -> Response {
    let Some(token) = token_ics
        .strip_suffix(".ics")
        .filter(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let state = app.state.lock().await;

    // Token validation: strict hash comparison — no name-based fallbacks.
    let person_id = state
        .calendar_tokens
        .iter()
        .find(|ct| !ct.revoked && verify_calendar_token(token, &ct.token_hash))
        .map(|ct| ct.person_id.clone());

    let Some(person_id) = person_id else {
        info!("iCal request: token not found or revoked");
        return (StatusCode::NOT_FOUND, "Token not found or revoked.\n").into_response();
    };
    if !state
        .person_by_id(&person_id)
        .is_some_and(|p| p.active && p.matrix_id.is_some())
    {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Build schedule snapshot (pure computation, no mutations).
    let snapshot = build_schedule(&state, 52);

    // Capture Last-Modified timestamp before dropping the lock.
    let last_modified = snapshot.state_timestamp;
    drop(state);

    // Render ICS (pure function over snapshot).
    let ics_body = render_ics(&snapshot, &person_id);

    // ETag = SHA-256 of the ICS body (deterministic for unchanged state).
    let etag = format!("\"{}\"", hex::encode(Sha256::digest(ics_body.as_bytes())));
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|tag| tag.trim() == etag || tag.trim() == "*")
        })
    {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, "private, no-cache, must-revalidate")
            .body(axum::body::Body::empty())
            .unwrap();
    }
    // Last-Modified in RFC 7231 format.
    let last_mod_str = last_modified
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/calendar; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            "inline; filename=\"cleaning.ics\"",
        )
        .header(header::ETAG, etag)
        .header(header::LAST_MODIFIED, last_mod_str)
        .header("Cache-Control", "private, no-cache, must-revalidate")
        .body(axum::body::Body::from(ics_body))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CleaningGroup, Person};
    #[tokio::test]
    async fn feed_authentication_cache_and_revocation_are_enforced() {
        let mut state = State::default();
        let person = Person::new_matrix("@alice:example.org");
        let pid = person.id.clone();
        state.persons.push(person);
        let mut group = CleaningGroup::new("Kitchen");
        group.member_ids.push(pid.clone());
        state.cleaning_groups.push(group);
        let token = crate::private::calendar_token(&mut state, &pid);
        let app = Arc::new(AppState {
            state: Arc::new(Mutex::new(state)),
        });
        let path = format!("{token}.ics");
        let response =
            serve_ical(Path(path.clone()), AxumState(app.clone()), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let tag = response.headers()[header::ETAG].clone();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert!(String::from_utf8(body.to_vec())
            .unwrap()
            .contains("BEGIN:VEVENT"));
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, tag);
        assert_eq!(
            serve_ical(Path(path.clone()), AxumState(app.clone()), headers.clone())
                .await
                .status(),
            StatusCode::NOT_MODIFIED
        );
        app.state.lock().await.calendar_tokens[0].revoked = true;
        assert_eq!(
            serve_ical(Path(path), AxumState(app.clone()), headers)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        for invalid in ["alice.ics", "xyz.ics", &token, "../../state.json"] {
            assert_eq!(
                serve_ical(
                    Path(invalid.into()),
                    AxumState(app.clone()),
                    HeaderMap::new()
                )
                .await
                .status(),
                StatusCode::NOT_FOUND
            );
        }
    }
}
