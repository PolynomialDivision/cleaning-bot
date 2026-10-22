//! HTTP server serving per-person iCal feeds.
//!
//! Route: `GET /ical/<64-hex-token>.ics`
//!
//! Request flow:
//!   1. Validate token (hash comparison, no name lookups)
//!   2. Build ScheduleSnapshot (pure, from locked state)
//!   3. Render ICS (pure, from snapshot)
//!   4. Set ETag (SHA-256 of ICS body) and Last-Modified (state.last_modified)
//!
//! Authentication compares hashes. Recoverable token secrets are private state.
//!
//! A browser (Accept prefers text/html over text/calendar: someone tapping
//! the link in Element) gets a page on how to subscribe instead
//! (`subscribe`), with the same status codes; calendar apps keep getting
//! the `.ics`. The page's style sheet and script are under `/assets/`.

use axum::{
    extract::{Path, RawQuery, State as AxumState},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::{net::TcpListener, sync::Mutex};
use tracing::info;

use crate::{
    config::ICalServerConfig, domain::verify_calendar_token, ical::render_ics,
    schedule::build_schedule, state::State, subscribe,
};

struct AppState {
    state: Arc<Mutex<State>>,
    /// For the feed address the subscribe page shows.
    cfg: ICalServerConfig,
}

pub async fn run(state: Arc<Mutex<State>>, cfg: ICalServerConfig) -> anyhow::Result<()> {
    let bind_addr = cfg.bind_addr.clone();
    let shared = Arc::new(AppState { state, cfg });
    let app = Router::new()
        .route("/ical/:token_ics", get(serve_ical))
        .route("/assets/subscribe.css", get(serve_style))
        .route("/assets/subscribe.js", get(serve_script))
        .with_state(shared);

    let listener = TcpListener::bind(&bind_addr).await?;
    info!("iCal HTTP server listening on {bind_addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_style() -> Response {
    asset("text/css; charset=utf-8", subscribe::STYLE)
}

async fn serve_script() -> Response {
    asset("text/javascript; charset=utf-8", subscribe::SCRIPT)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=3600"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

/// A page for a browser: never cached or indexed, no referrer, nothing
/// from elsewhere (`subscribe::CSP`).
fn html(status: StatusCode, body: String) -> Response {
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        (header::CACHE_CONTROL, "private, no-store"),
        (header::CONTENT_SECURITY_POLICY, subscribe::CSP),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::VARY, "Accept, Accept-Language, Cookie"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers.insert(
        header::HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex, nofollow"),
    );
    response
}

async fn serve_ical(
    Path(token_ics): Path<String>,
    AxumState(app): AxumState<Arc<AppState>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let browser = subscribe::wants_html(&headers);
    let not_found = |text: &'static str| {
        if browser {
            let lang = subscribe::language(&headers, query.as_deref());
            html(StatusCode::NOT_FOUND, subscribe::invalid_page(lang))
        } else {
            (StatusCode::NOT_FOUND, text).into_response()
        }
    };
    let Some(token) = token_ics
        .strip_suffix(".ics")
        .filter(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return not_found("");
    };

    let state = app.state.lock().await;

    // Token validation: strict hash comparison. No name-based fallbacks.
    let person_id = state
        .calendar_tokens
        .iter()
        .find(|ct| !ct.revoked && verify_calendar_token(token, &ct.token_hash))
        .map(|ct| ct.person_id.clone());

    let Some(person_id) = person_id else {
        info!("iCal request: token not found or revoked");
        return not_found("Token not found or revoked.\n");
    };
    if !state
        .person_by_id(&person_id)
        .is_some_and(|p| p.active && p.matrix_id.is_some())
    {
        return not_found("");
    }
    if browser {
        drop(state);
        let lang = subscribe::language(&headers, query.as_deref());
        let feed = crate::private::feed_url(&app.cfg, token);
        return html(StatusCode::OK, subscribe::subscribe_page(lang, &feed));
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
            .header(header::VARY, "Accept")
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
        .header(header::VARY, "Accept")
        .body(axum::body::Body::from(ics_body))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CleaningGroup, Person};

    const BROWSER: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

    fn app_with_alice() -> (Arc<AppState>, String) {
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
            cfg: ICalServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                public_url: "https://cal.example.org/".into(),
            },
        });
        (app, token)
    }

    async fn get(app: &Arc<AppState>, path: &str, headers: HeaderMap) -> Response {
        get_query(app, path, None, headers).await
    }

    async fn get_query(
        app: &Arc<AppState>,
        path: &str,
        query: Option<&str>,
        headers: HeaderMap,
    ) -> Response {
        serve_ical(
            Path(path.into()),
            AxumState(app.clone()),
            RawQuery(query.map(Into::into)),
            headers,
        )
        .await
    }

    async fn body(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn accept(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[tokio::test]
    async fn feed_authentication_cache_and_revocation_are_enforced() {
        let (app, token) = app_with_alice();
        let path = format!("{token}.ics");
        let response = get(&app, &path, HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let tag = response.headers()[header::ETAG].clone();
        assert!(body(response).await.contains("BEGIN:VEVENT"));
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, tag);
        assert_eq!(
            get(&app, &path, headers.clone()).await.status(),
            StatusCode::NOT_MODIFIED
        );
        app.state.lock().await.calendar_tokens[0].revoked = true;
        assert_eq!(
            get(&app, &path, headers).await.status(),
            StatusCode::NOT_FOUND
        );
        for invalid in ["alice.ics", "xyz.ics", &token, "../../state.json"] {
            assert_eq!(
                get(&app, invalid, HeaderMap::new()).await.status(),
                StatusCode::NOT_FOUND
            );
        }
    }

    #[tokio::test]
    async fn calendar_apps_keep_getting_the_feed() {
        let (app, token) = app_with_alice();
        let path = format!("{token}.ics");
        for value in ["*/*", "text/calendar", "text/calendar, text/html;q=0.5"] {
            let response = get(&app, &path, accept(value)).await;
            assert_eq!(response.status(), StatusCode::OK, "{value}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/calendar; charset=utf-8"
            );
            assert_eq!(response.headers()[header::VARY], "Accept");
            assert!(body(response).await.starts_with("BEGIN:VCALENDAR"));
        }
    }

    #[tokio::test]
    async fn a_browser_gets_the_subscribe_page_without_personal_data() {
        let (app, token) = app_with_alice();
        let response = get(&app, &format!("{token}.ics"), accept(BROWSER)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], subscribe::CSP);
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(headers[header::CACHE_CONTROL], "private, no-store");
        assert!(headers[header::VARY].to_str().unwrap().contains("Accept"));
        let page = body(response).await;
        // German by default; the feed URL from the configured public address.
        assert!(page.contains("<html lang=\"de\">"), "{page}");
        assert!(page.contains(&format!(
            "value=\"https://cal.example.org/ical/{token}.ics\""
        )));
        assert!(page.contains(&format!(
            "href=\"webcal://cal.example.org/ical/{token}.ics\""
        )));
        assert!(!page.contains("alice") && !page.contains("Kitchen"));
        assert!(!page.contains("BEGIN:VCALENDAR"));

        let mut english = accept(BROWSER);
        english.insert(header::COOKIE, HeaderValue::from_static("house_lang=en"));
        let page = body(get(&app, &format!("{token}.ics"), english).await).await;
        assert!(page.contains("Add to iPhone/Mac calendar"));
        let page = body(
            get_query(
                &app,
                &format!("{token}.ics"),
                Some("lang=uk"),
                accept(BROWSER),
            )
            .await,
        )
        .await;
        assert!(page.contains("<html lang=\"uk\">"));
    }

    #[tokio::test]
    async fn a_browser_with_a_dead_link_gets_a_friendly_404() {
        let (app, token) = app_with_alice();
        app.state.lock().await.calendar_tokens[0].revoked = true;
        let unknown = "ab".repeat(32) + ".ics";
        for path in [format!("{token}.ics"), unknown, "nope.ics".into()] {
            let response = get(&app, &path, accept(BROWSER)).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/html; charset=utf-8"
            );
            assert!(body(response).await.contains("Kalender-Link ungültig"));
        }
        // A calendar app still gets the plain answer.
        let plain = get(&app, &format!("{token}.ics"), accept("*/*")).await;
        assert_eq!(plain.status(), StatusCode::NOT_FOUND);
        assert_eq!(body(plain).await, "Token not found or revoked.\n");
    }

    #[tokio::test]
    async fn the_page_assets_are_served() {
        let css = serve_style().await;
        assert_eq!(
            css.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        assert!(body(css).await.contains("var(--h-accent"));
        let js = serve_script().await;
        assert!(body(js).await.contains("clipboard"));
    }
}
