//! The website: the cleaning plan in a browser for the house's residents, read only. Off
//! unless `[web] enabled`; its own listener, never the public iCal server's (`http`).
//!
//! Who asks: the house's login at the reverse proxy (forward auth). The proxy asks the
//! identity provider and passes the person on as the neutral X-House-* headers, after
//! dropping a client's own copies. On a shared Docker network other containers could send
//! those headers too, so a request must also carry the proxy's secret in
//! X-Cleaning-Proxy-Secret (CLEANING_PROXY_SECRET), which only the proxy knows. The check
//! is mxbot-common's (`mxbot_common::web::ForwardAuth`): the secret in constant time, then
//! the login, then `[web] allowed_groups` (empty: nobody).
//!
//! Who they are in the plan: their house account (`@<login>:<the bot's server>`,
//! `mxbot_common::web::house_account`) or one of their linked accounts in the directory
//! (`matrix_ids`, X-House-Matrix-Ids), each resolved like a Matrix event from it with the bot's
//! last read of the directory (`directory::person_account`: their person by directory id,
//! else by a recognised account). Someone the plan doesn't know sees every page, just
//! without "you".
//!
//! Every page is built from one schedule snapshot (`schedule::build_schedule_from`), taken
//! under the state lock that the Matrix side uses too, so the website always shows the
//! plan the room does. Rendering (`render`) happens after the lock is released. Nothing can
//! be changed here: ✅, !sos and swaps stay in Matrix. Pages are never cached or indexed,
//! load nothing from elsewhere and need no script; names are escaped.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    extract::{Path, RawQuery, State as AxumState},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use mxbot_common::{
    config::AllowList,
    format::html_escape,
    matrix_sdk::ruma::{OwnedUserId, UserId},
    web::{self as forward_auth, Access, ForwardAuth, ProxySecret, Refusal},
};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::info;

use crate::{
    config::WebConfig,
    directory::{self, Shared},
    schedule::build_schedule_from,
    state::{self, State},
};

mod i18n;
mod render;
use i18n::{t, Lang};
use render::{GroupInfo, Plan};

/// The proxy's headers (`mxbot_common::web`), for the tests' requests.
#[cfg(test)]
use forward_auth::{
    house_account as matrix_id, GROUPS_HEADER, MATRIX_IDS_HEADER, NAME_HEADER, USER_HEADER,
};
pub const PROXY_SECRET_HEADER: &str = "x-cleaning-proxy-secret";
/// The environment variable with the proxy's secret.
pub const PROXY_SECRET_ENV: &str = "CLEANING_PROXY_SECRET";

/// `?weeks=` on any page: at least one, at most half a year.
const MAX_WEEKS: usize = 26;

pub struct Settings {
    pub bind_addr: String,
    pub public_url: String,
    /// Groups (X-House-Groups) that may see it; empty: nobody.
    pub allowed_groups: Vec<String>,
    /// Without a login (the proxy asks for it only from outside the house): a guest.
    pub guests: bool,
    pub proxy_secret: Vec<u8>,
    /// The house's start page for the house bar; None: public_url's parent domain.
    pub home_url: Option<String>,
    /// Its name in the house bar; None: from its address ("example.org": "Example").
    pub home_name: Option<String>,
}

impl Settings {
    pub fn new(cfg: &WebConfig, proxy_secret: Vec<u8>) -> Settings {
        Settings {
            bind_addr: cfg.bind_addr.clone(),
            public_url: cfg.public_url.clone(),
            allowed_groups: cfg.allowed_groups.clone(),
            guests: cfg.guests,
            proxy_secret,
            home_url: cfg.home_url.clone(),
            home_name: cfg.home_name.clone(),
        }
    }
}

pub struct Web {
    state: Arc<Mutex<State>>,
    /// The bot's last read of the house's directory (`BotContext::directory`).
    directory: Shared,
    settings: Settings,
    /// The proxy's headers: the secret, the login, the groups, the accounts.
    auth: ForwardAuth,
    /// The house bar's link home: (address, name), or none (no parent domain to go to).
    home: Option<(String, String)>,
}

type WebState = Arc<Web>;

/// The proxy's secret from the environment: required, at least 32 characters.
pub fn proxy_secret_from_env() -> Result<Vec<u8>> {
    Ok(forward_auth::secret_from_env(PROXY_SECRET_ENV)?.into_bytes())
}

impl Web {
    /// `bot_user_id`: the bot's own Matrix ID, whose server the house accounts are on.
    pub fn new(
        state: Arc<Mutex<State>>,
        directory: Shared,
        mut settings: Settings,
        bot_user_id: &str,
    ) -> Result<Web> {
        let server_name = UserId::parse(bot_user_id)
            .with_context(|| format!("[matrix] user_id {bot_user_id:?}"))?
            .server_name()
            .to_string();
        let secret = ProxySecret::new(
            PROXY_SECRET_HEADER,
            std::mem::take(&mut settings.proxy_secret),
        )?;
        let access = Access {
            allowed_groups: AllowList::Only(settings.allowed_groups.iter().cloned().collect()),
            admin_groups: Vec::new(),
            guests: settings.guests,
        };
        let auth = ForwardAuth::new(secret, access, &server_name)?;
        let home = home_link(
            &settings.public_url,
            settings.home_url.as_deref(),
            settings.home_name.as_deref(),
        );
        Ok(Web {
            state,
            directory,
            settings,
            auth,
            home,
        })
    }
}

pub fn router(web: WebState) -> Router {
    Router::new()
        .route("/", get(week_page))
        .route("/groups", get(groups_page))
        .route("/g/:id", get(group_page))
        .route("/people", get(people_page))
        .route("/me", get(me_page))
        .route("/login", get(login))
        .route("/style.css", get(|| async { asset("text/css", STYLE) }))
        .with_state(web)
}

pub async fn serve(web: Web) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(&web.settings.bind_addr)
        .await
        .with_context(|| format!("[web] binding {}", web.settings.bind_addr))?;
    info!(addr = %web.settings.bind_addr, url = %web.settings.public_url, "Website listening");
    axum::serve(listener, router(Arc::new(web))).await?;
    Ok(())
}

// ── Who asks ────────────────────────────────────────────────────────────────

/// Someone the proxy let through.
#[derive(Clone, Debug, PartialEq)]
pub struct Visitor {
    /// Their house account first, then their other accounts (X-House-Matrix-Ids).
    pub accounts: Vec<OwnedUserId>,
    /// Their name at the login (X-House-Name, else the login); empty for a guest.
    pub name: String,
    /// No login: in the house, where the proxy lets everyone through (`[web] guests`).
    pub guest: bool,
    pub frame: Frame,
}

/// Why a request is turned away, and the language to say it in.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not through the house's proxy (no or a wrong secret).
    Proxy(Lang),
    NoLogin(Lang),
    NotAllowed(Lang),
}

impl Refused {
    fn of(refusal: Refusal, lang: Lang) -> Refused {
        match refusal {
            Refusal::Proxy => Refused::Proxy(lang),
            Refusal::NoLogin => Refused::NoLogin(lang),
            Refusal::NotAllowed => Refused::NotAllowed(lang),
        }
    }
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let (status, lang, key) = match self {
            Refused::Proxy(l) => (StatusCode::FORBIDDEN, l, "refused.proxy"),
            Refused::NoLogin(l) => (StatusCode::UNAUTHORIZED, l, "refused.login"),
            Refused::NotAllowed(l) => (StatusCode::FORBIDDEN, l, "refused.group"),
        };
        secure(
            (
                status,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                t(lang, key),
            )
                .into_response(),
        )
    }
}

/// Compared in constant time (`mxbot_common::web::secret_eq`).
#[cfg(test)]
fn proxy_secret_ok(headers: &HeaderMap, secret: &[u8]) -> bool {
    headers
        .get(PROXY_SECRET_HEADER)
        .is_some_and(|sent| forward_auth::secret_eq(sent.as_bytes(), secret))
}

fn visitor(headers: &HeaderMap, query: Option<&str>, web: &Web) -> Result<Visitor, Refused> {
    let frame = Frame::of(headers, query);
    match web.auth.identify(headers) {
        Err(refusal) => Err(Refused::of(refusal, frame.lang)),
        Ok(forward_auth::Visitor::Guest) => Ok(Visitor {
            accounts: Vec::new(),
            name: String::new(),
            guest: true,
            frame,
        }),
        Ok(forward_auth::Visitor::Person(person)) => Ok(Visitor {
            accounts: person.accounts,
            name: person.name,
            guest: false,
            frame,
        }),
    }
}

// ── The house bar ───────────────────────────────────────────────────────────

/// The house bar's link home: `home_url` (and `home_name`) when set, else the parent
/// domain of `public_url` (`https://putzplan.example.org` → `https://example.org/`,
/// "Example"). None without a parent domain (an IP address, "localhost:8080").
pub fn home_link(
    public_url: &str,
    home_url: Option<&str>,
    home_name: Option<&str>,
) -> Option<(String, String)> {
    let url = match home_url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(url) => url.to_owned(),
        None => {
            let (scheme, rest) = public_url.split_once("://")?;
            let host = rest.split(['/', '?', '#', ':']).next()?;
            let labels: Vec<&str> = host.split('.').collect();
            if labels.len() < 3 || host.parse::<std::net::IpAddr>().is_ok() {
                return None;
            }
            format!("{scheme}://{}/", labels[1..].join("."))
        }
    };
    let name = match home_name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_owned(),
        None => {
            let host = url.split_once("://")?.1.split(['/', ':']).next()?;
            let first = host.split('.').next().unwrap_or(host);
            let mut chars = first.chars();
            let initial = chars.next()?;
            initial.to_uppercase().chain(chars).collect()
        }
    };
    Some((url, name))
}

// ── Page frame ──────────────────────────────────────────────────────────────

/// Text for HTML content and quoted attribute values (`&`, `<`, `>`, `"` and `'` escaped,
/// mxbot_common::format::html_escape). Control characters other than a newline, which have
/// no place in a page, become spaces.
pub fn esc(text: &str) -> String {
    if text.chars().any(is_stray_control) {
        let cleaned: String = text
            .chars()
            .map(|c| if is_stray_control(c) { ' ' } else { c })
            .collect();
        html_escape(&cleaned)
    } else {
        html_escape(text)
    }
}

fn is_stray_control(c: char) -> bool {
    c.is_control() && c != '\n'
}

/// The language a page is in, and whether its links must carry it (`?lang=` chose another
/// than the cookie or the browser would).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub lang: Lang,
    pub lang_in_links: bool,
}

impl Frame {
    fn of(headers: &HeaderMap, query: Option<&str>) -> Frame {
        let lang = crate::lang::language(headers, query);
        Frame {
            lang,
            lang_in_links: lang != crate::lang::language(headers, None),
        }
    }

    /// A link to `path` on this website, with `?weeks=` and the language as needed.
    pub fn href(&self, path: &str, weeks: Option<usize>) -> String {
        let mut params = Vec::new();
        if let Some(n) = weeks {
            params.push(format!("weeks={n}"));
        }
        if self.lang_in_links {
            params.push(format!("lang={}", self.lang.code()));
        }
        if params.is_empty() {
            path.to_owned()
        } else {
            format!("{path}?{}", params.join("&"))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Week,
    Groups,
    People,
    Me,
}

const CSP: &str = "default-src 'none'; style-src 'self'; img-src 'self'; form-action 'none'; \
                   frame-ancestors 'none'; base-uri 'none'";

fn secure(mut response: Response) -> Response {
    let h = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_SECURITY_POLICY, CSP),
        (header::CACHE_CONTROL, "no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "same-origin"),
    ] {
        h.insert(name, HeaderValue::from_static(value));
    }
    h.insert(
        header::HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex, nofollow"),
    );
    response
}

/// Changes with the style, so a browser never keeps an old one.
fn asset_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| hex::encode(&Sha256::digest(STYLE)[..6]))
}

fn asset(kind: &'static str, body: &'static str) -> Response {
    let mut r = (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        body,
    )
        .into_response();
    r.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    r
}

impl Web {
    /// A whole page around `body`: the house bar with the language switch (back to `path`
    /// with `weeks`), the tabs, the footer.
    fn page(
        &self,
        f: &Frame,
        tab: Tab,
        title: &str,
        (path, weeks): (&str, Option<usize>),
        status: StatusCode,
        body: &str,
    ) -> Response {
        let l = f.lang;
        let nav: String = [
            (Tab::Week, "/", "nav.week"),
            (Tab::Groups, "/groups", "nav.groups"),
            (Tab::People, "/people", "nav.people"),
            (Tab::Me, "/me", "nav.me"),
        ]
        .iter()
        .map(|(which, href, label)| {
            format!(
                "<a href=\"{}\"{}>{}</a>",
                esc(&f.href(href, None)),
                if *which == tab {
                    " aria-current=\"page\""
                } else {
                    ""
                },
                esc(t(l, label))
            )
        })
        .collect();
        let langs: String = [(Lang::De, "DE"), (Lang::En, "EN"), (Lang::Uk, "УКР")]
            .iter()
            .map(|(lang, label)| {
                let switch = Frame {
                    lang: *lang,
                    lang_in_links: true,
                };
                format!(
                    "<a href=\"{}\" hreflang=\"{code}\" lang=\"{code}\"{}>{label}</a>",
                    esc(&switch.href(path, weeks)),
                    if *lang == l {
                        " aria-current=\"true\""
                    } else {
                        ""
                    },
                    code = lang.code(),
                )
            })
            .collect();
        let app = esc(t(l, "app.name"));
        let home = match &self.home {
            Some((url, name)) => format!(
                "<a class=\"h-home\" href=\"{}\" title=\"{}\"><span aria-hidden=\"true\">⌂</span> {}</a>\
                 <span class=\"h-bar-app\">{app}</span>",
                esc(url),
                esc(t(l, "home.title")),
                esc(name)
            ),
            None => format!("<span class=\"brand\">{app}</span>"),
        };
        let html = format!(
            "<!doctype html>\n<html lang=\"{lang}\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<meta name=\"robots\" content=\"noindex, nofollow\">\
<title>{title} · {app}</title><link rel=\"stylesheet\" href=\"/_house/ui.css\">\
<link rel=\"stylesheet\" href=\"/style.css?v={v}\"></head>\
<body><header class=\"h-bar\">{home}<nav class=\"h-lang\" aria-label=\"{lang_label}\">{langs}</nav></header>\
<nav class=\"tabs\" aria-label=\"{nav_label}\">{nav}</nav><main>{body}</main>\
<footer><p>{footer}</p></footer></body></html>\n",
            lang = l.code(),
            title = esc(title),
            v = asset_version(),
            lang_label = esc(t(l, "lang.label")),
            nav_label = esc(t(l, "nav.label")),
            footer = esc(t(l, "page.footer")),
        );
        secure(
            (
                status,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                html,
            )
                .into_response(),
        )
    }

    /// The plan of `weeks` weeks from this one, for `visitor`. The snapshot, the groups and
    /// who the visitor is are all taken under one lock of the state; rendering comes after.
    async fn plan(&self, visitor: &Visitor, weeks: usize) -> Plan {
        let snapshot = directory::read(&self.directory);
        let people = snapshot.as_deref().map(|s| &s.people);
        let state = self.state.lock().await;
        let today = state::today();
        let week = state::current_iso_week();
        let me = visitor.accounts.iter().find_map(|account| {
            let known = directory::person_account(people, &state, account);
            state
                .person_by_matrix_id(known.as_str())
                .filter(|p| p.active)
                .map(|p| p.id.clone())
        });
        let snapshot = build_schedule_from(&state, week, weeks);
        let active: Vec<_> = state
            .cleaning_groups
            .iter()
            .filter(|g| g.is_active)
            .collect();
        let groups = active.iter().map(|g| GroupInfo::of(g, today)).collect();
        let people = state
            .persons
            .iter()
            .filter(|p| p.active && active.iter().any(|g| g.member_ids.contains(&p.id)))
            .map(|p| (p.id.clone(), p.display_name.clone()))
            .collect();
        drop(state);
        Plan {
            today,
            week,
            snapshot,
            groups,
            people,
            me,
            login_name: visitor.name.clone(),
            guest: visitor.guest,
        }
    }
}

/// `?weeks=N`, clamped to 1–26; `None` without (or with something that isn't a number).
fn weeks_param(query: Option<&str>) -> Option<usize> {
    query?
        .split('&')
        .filter_map(|pair| pair.strip_prefix("weeks="))
        .find_map(|n| n.trim().parse::<i64>().ok())
        .map(|n| n.clamp(1, MAX_WEEKS as i64) as usize)
}

// ── Pages ───────────────────────────────────────────────────────────────────

async fn week_page(
    AxumState(web): AxumState<WebState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let visitor = match visitor(&headers, query.as_deref(), &web) {
        Ok(v) => v,
        Err(no) => return no.into_response(),
    };
    let asked = weeks_param(query.as_deref());
    let ahead = asked.unwrap_or(6);
    let plan = web.plan(&visitor, ahead + 1).await;
    let f = visitor.frame;
    let body = render::week_page(&f, &plan, ahead);
    web.page(
        &f,
        Tab::Week,
        t(f.lang, "nav.week"),
        ("/", asked),
        StatusCode::OK,
        &body,
    )
}

async fn groups_page(
    AxumState(web): AxumState<WebState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let visitor = match visitor(&headers, query.as_deref(), &web) {
        Ok(v) => v,
        Err(no) => return no.into_response(),
    };
    let asked = weeks_param(query.as_deref());
    let weeks = asked.unwrap_or(8);
    let plan = web.plan(&visitor, weeks).await;
    let f = visitor.frame;
    let body = render::groups_page(&f, &plan, weeks, None).unwrap_or_default();
    web.page(
        &f,
        Tab::Groups,
        t(f.lang, "groups.title"),
        ("/groups", asked),
        StatusCode::OK,
        &body,
    )
}

async fn group_page(
    AxumState(web): AxumState<WebState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let visitor = match visitor(&headers, query.as_deref(), &web) {
        Ok(v) => v,
        Err(no) => return no.into_response(),
    };
    let asked = weeks_param(query.as_deref());
    let weeks = asked.unwrap_or(12);
    let plan = web.plan(&visitor, weeks).await;
    let f = visitor.frame;
    let path = format!("/g/{id}");
    match render::groups_page(&f, &plan, weeks, Some(&id)) {
        Some(body) => {
            let title = plan
                .groups
                .iter()
                .find(|g| g.id == id)
                .map_or("", |g| g.name.as_str());
            web.page(
                &f,
                Tab::Groups,
                title,
                (&path, asked),
                StatusCode::OK,
                &body,
            )
        }
        None => {
            let body = format!(
                "<h1>{}</h1><p class=\"empty\">{}</p><p><a href=\"{}\">{}</a></p>",
                esc(t(f.lang, "groups.title")),
                esc(t(f.lang, "groups.unknown")),
                esc(&f.href("/groups", None)),
                esc(t(f.lang, "groups.all"))
            );
            web.page(
                &f,
                Tab::Groups,
                t(f.lang, "groups.title"),
                (&path, asked),
                StatusCode::NOT_FOUND,
                &body,
            )
        }
    }
}

async fn people_page(
    AxumState(web): AxumState<WebState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let visitor = match visitor(&headers, query.as_deref(), &web) {
        Ok(v) => v,
        Err(no) => return no.into_response(),
    };
    let asked = weeks_param(query.as_deref());
    let weeks = asked.unwrap_or(8);
    let plan = web.plan(&visitor, weeks).await;
    let f = visitor.frame;
    let body = render::people_page(&f, &plan, weeks);
    web.page(
        &f,
        Tab::People,
        t(f.lang, "people.title"),
        ("/people", asked),
        StatusCode::OK,
        &body,
    )
}

async fn me_page(
    AxumState(web): AxumState<WebState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let visitor = match visitor(&headers, query.as_deref(), &web) {
        Ok(v) => v,
        Err(no) => return no.into_response(),
    };
    let asked = weeks_param(query.as_deref());
    let weeks = asked.unwrap_or(12);
    let plan = web.plan(&visitor, weeks).await;
    let f = visitor.frame;
    let body = render::me_page(&f, &plan, weeks);
    web.page(
        &f,
        Tab::Me,
        t(f.lang, "me.title"),
        ("/me", asked),
        StatusCode::OK,
        &body,
    )
}

/// `/login`: the guests' way to log in in the house. The proxy asks for the login on this
/// path from everywhere (and from then on for every request with the identity provider's
/// session cookie); once back here, the visitor goes on to their turns. A guest who still
/// arrives without a login (a proxy that lets this path through) lands there too.
async fn login(
    AxumState(web): AxumState<WebState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let visitor = match visitor(&headers, query.as_deref(), &web) {
        Ok(v) => v,
        Err(no) => return no.into_response(),
    };
    let to = visitor.frame.href("/me", None);
    secure(
        (
            StatusCode::SEE_OTHER,
            [(
                header::LOCATION,
                HeaderValue::from_str(&to).unwrap_or(HeaderValue::from_static("/me")),
            )],
        )
            .into_response(),
    )
}

// ── Style ───────────────────────────────────────────────────────────────────

/// Layout only: the house kit (`/_house/ui.css`, linked first) sets colours, type, cards,
/// badges and the house bar.
const STYLE: &str = include_str!("web/style.css");

#[cfg(test)]
mod tests;
