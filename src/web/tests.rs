//! The website against a synthetic house: who gets in, who is "you", and that the pages
//! show the snapshot's plan (the staircase's pairs by place, names escaped) in every
//! language.

use axum::body::{to_bytes, Body};
use axum::http::Request;
use tower::ServiceExt;

use super::*;
use crate::{
    analytics::DomainEvent,
    domain::{CleaningGroup, CleaningSlot, Person, PersonId},
    rhythm::Rhythm,
    schedule::AssignmentInstance,
};

const SECRET: &str = "a-test-proxy-secret-of-at-least-32-chars";
const EVE: &str = "<b>&\"";

/// Six residents; "2nd Floor" twice a week (Mon–Tue, Thu–Fri), a bathroom with two slots,
/// and the house-wide staircase every 2 weeks with two people each at Scharni and Colbe.
/// Tracking starts this week, so the staircase is due now. Planned ahead, and Alice's
/// first turn this week was done by Bob.
fn house() -> State {
    let mut st = State {
        created_at: Some(chrono::Utc::now()),
        ..State::default()
    };
    let mut add = |mxid: Option<&str>, name: &str| -> PersonId {
        let mut p = match mxid {
            Some(mxid) => Person::new_matrix(mxid),
            None => Person::new_named(name),
        };
        p.display_name = name.to_owned();
        let id = p.id.clone();
        st.persons.push(p);
        id
    };
    let alice = add(Some("@alice:example.org"), "Alice");
    let bob = add(Some("@bob:matrix.org"), "Bob");
    let carol = add(Some("@carol:matrix.org"), "carol");
    let dan = add(Some("@dan:example.org"), "Dan");
    let eve = add(Some("@eve:example.org"), EVE);
    let finn = add(None, "Finn");

    let mut floor = CleaningGroup::new("2nd Floor");
    floor.member_ids = vec![alice.clone(), bob.clone(), carol.clone()];
    floor.rhythm = Rhythm {
        every_weeks: Some(1),
        shift_starts: vec![0, 3],
        shift_ends: vec![1, 4],
        ..Default::default()
    };
    floor.room_names = vec!["Hallway".into()];

    let mut bath = CleaningGroup::new("Bathroom");
    bath.member_ids = vec![dan.clone(), eve.clone(), finn.clone()];
    for (slot, room) in [("Toilet", "Scharni Toilet"), ("Shower", "Shower Room")] {
        let mut s = CleaningSlot::new(slot);
        s.room_names = vec![room.into()];
        bath.slots.push(s);
    }

    let mut stairs = CleaningGroup::new("Staircase");
    stairs.house_wide = true;
    stairs.member_ids = vec![alice, bob.clone(), carol, dan, eve, finn];
    stairs.rhythm = Rhythm {
        every_weeks: Some(2),
        ..Rhythm::weekly()
    };
    for place in ["Scharni", "Colbe"] {
        for n in 1..=2 {
            let mut s = CleaningSlot::new(&format!("{place} {n}"));
            s.place = Some(place.into());
            stairs.slots.push(s);
        }
    }
    st.cleaning_groups = vec![floor, bath, stairs];

    for event in crate::resolver::materialize(&st, 26) {
        st.apply_event(event).unwrap();
    }
    let snapshot = build_schedule_from(&st, state::current_iso_week(), 1);
    let first = snapshot
        .assignments
        .iter()
        .find(|a| a.assignee.as_ref().is_some_and(|p| p.name == "Alice"))
        .expect("Alice cleans this week")
        .clone();
    st.apply_event(DomainEvent::CleaningCompleted {
        completed_on: None,
        group_id: first.group_id,
        slot_id: first.slot_id,
        person_id: bob,
        responsible_person_ids: Vec::new(),
        iso_year: first.iso_year,
        iso_week: first.iso_week,
        shift: first.shift,
    })
    .unwrap();
    st
}

struct Site {
    web: WebState,
}

impl Site {
    fn new() -> Site {
        Site::with(house(), Accounts::default())
    }

    fn with(state: State, accounts: Accounts) -> Site {
        Site::configured(state, accounts, false)
    }

    /// `guests`: the proxy asks for the login only from outside the house.
    fn configured(state: State, accounts: Accounts, guests: bool) -> Site {
        let web = Web::new(
            Arc::new(Mutex::new(state)),
            Arc::new(RwLock::new(accounts)),
            Settings {
                bind_addr: "127.0.0.1:0".into(),
                public_url: "https://putzplan.example.org".into(),
                allowed_groups: vec!["residents".into()],
                guests,
                proxy_secret: SECRET.as_bytes().to_vec(),
                home_url: None,
                home_name: None,
            },
            "@cleaningbot:example.org",
        )
        .unwrap();
        Site { web: Arc::new(web) }
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let response = router(self.web.clone()).oneshot(req).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 1 << 22).await.unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    /// `path` as `login` (in the group "residents"), in English.
    async fn get(&self, path: &str, login: &str) -> (StatusCode, String) {
        self.get_with(path, login, &[]).await
    }

    async fn get_with(
        &self,
        path: &str,
        login: &str,
        extra: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let mut req = Request::get(path)
            .header(PROXY_SECRET_HEADER, SECRET)
            .header(USER_HEADER, login)
            .header(GROUPS_HEADER, "other|residents")
            .header(NAME_HEADER, "Kim K.")
            .header(header::ACCEPT_LANGUAGE, "en");
        for (name, value) in extra {
            req = req.header(*name, *value);
        }
        let (status, _, body) = self.send(req.body(Body::empty()).unwrap()).await;
        (status, body)
    }

    async fn snapshot(&self, weeks: usize) -> ScheduleSnapshot {
        let state = self.web.state.lock().await;
        build_schedule_from(&state, state::current_iso_week(), weeks)
    }
}

use crate::schedule::ScheduleSnapshot;

/// The names at `place` in each of its rows on `page`, in order (a row ends at the next
/// place or the end of its item).
fn people_at(page: &str, place: &str) -> Vec<Vec<String>> {
    let marker = format!("<span class=\"place-name\">{place}</span>");
    page.match_indices(&marker)
        .map(|(at, _)| {
            let rest = &page[at + marker.len()..];
            let end = [rest.find("</li>"), rest.find("class=\"place-name\"")]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(rest.len());
            let row = &rest[..end];
            row.match_indices("class=\"name\">")
                .map(|(i, m)| {
                    let name = &row[i + m.len()..];
                    name[..name.find('<').unwrap()].to_owned()
                })
                .collect()
        })
        .collect()
}

fn names(list: &[&AssignmentInstance]) -> Vec<String> {
    list.iter()
        .map(|a| esc(a.assignee.as_ref().map_or("—", |p| p.name.as_str())))
        .collect()
}

#[tokio::test]
async fn only_the_house_proxy_with_a_login_in_an_allowed_group_gets_in() {
    let site = Site::new();
    let request = |secret: Option<&str>, user: Option<&str>, groups: &str, lang: &str| {
        let mut r = Request::get("/")
            .header(GROUPS_HEADER, groups)
            .header(header::ACCEPT_LANGUAGE, lang);
        if let Some(s) = secret {
            r = r.header(PROXY_SECRET_HEADER, s);
        }
        if let Some(u) = user {
            r = r.header(USER_HEADER, u);
        }
        r.body(Body::empty()).unwrap()
    };
    let wrong = "b".repeat(SECRET.len());
    for (req, status, text) in [
        (
            request(None, Some("alice"), "residents", "de"),
            StatusCode::FORBIDDEN,
            "Kein Zugang",
        ),
        (
            request(Some(&wrong), Some("alice"), "residents", "en"),
            StatusCode::FORBIDDEN,
            "No access",
        ),
        (
            request(Some("short"), Some("alice"), "residents", "en"),
            StatusCode::FORBIDDEN,
            "No access",
        ),
        (
            request(Some(SECRET), None, "residents", "en"),
            StatusCode::UNAUTHORIZED,
            "log in",
        ),
        (
            request(Some(SECRET), Some(" "), "residents", "uk"),
            StatusCode::UNAUTHORIZED,
            "увійди",
        ),
        (
            request(Some(SECRET), Some("alice"), "guests|residentsx", "en"),
            StatusCode::FORBIDDEN,
            "may not see",
        ),
        (
            request(Some(SECRET), Some("alice"), "", "en"),
            StatusCode::FORBIDDEN,
            "may not see",
        ),
    ] {
        let (got, headers, body) = site.send(req).await;
        assert_eq!(got, status, "{body}");
        assert!(body.contains(text), "{body}");
        assert_eq!(headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert!(!body.contains("Alice") && !body.contains("Staircase"));
    }
    let (status, headers, _) = site
        .send(request(Some(SECRET), Some("alice"), "residents", "en"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(headers["x-robots-tag"], "noindex, nofollow");
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(csp.starts_with("default-src 'none'") && !csp.contains("unsafe"));
}

#[tokio::test]
async fn in_the_house_a_guest_sees_the_plan_without_logging_in() {
    let guest = |path: &str, secret: &str| {
        Request::get(path)
            .header(PROXY_SECRET_HEADER, secret)
            .header(header::ACCEPT_LANGUAGE, "en")
            .body(Body::empty())
            .unwrap()
    };
    // Without `guests`, no login is no entry.
    let (status, ..) = Site::new().send(guest("/", SECRET)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let site = Site::configured(house(), Accounts::default(), true);
    let (status, _, week) = site.send(guest("/", SECRET)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        week.contains("Staircase") && week.contains("Alice"),
        "{week}"
    );
    assert!(
        !week.contains("class=\"you\"") && !week.contains("yours-title"),
        "nobody is you"
    );
    assert!(week.contains("href=\"/login\""), "a way to log in: {week}");
    let (_, _, me) = site.send(guest("/me", SECRET)).await;
    assert!(me.contains("know who you are"), "{me}");
    assert!(me.contains("href=\"/login\""), "{me}");
    // Still only through the proxy.
    let (status, ..) = site.send(guest("/", &"b".repeat(SECRET.len()))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // With a login, as always: their turns are theirs.
    let (status, me) = site.get("/me", "alice").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!me.contains("know who you are") && !me.contains("href=\"/login\""));
    let (_, _, week) = site
        .send(
            Request::get("/")
                .header(PROXY_SECRET_HEADER, SECRET)
                .header(USER_HEADER, "alice")
                .header(GROUPS_HEADER, "residents")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(
        !week.contains("href=\"/login\""),
        "logged in: no login link"
    );
}

#[tokio::test]
async fn back_from_the_login_the_visitor_goes_on_to_their_turns() {
    let site = Site::configured(house(), Accounts::default(), true);
    let login = |path: &str, user: Option<&str>| {
        let mut req = Request::get(path).header(PROXY_SECRET_HEADER, SECRET);
        if let Some(user) = user {
            req = req
                .header(USER_HEADER, user)
                .header(GROUPS_HEADER, "residents");
        }
        req.body(Body::empty()).unwrap()
    };
    for (path, user, to) in [
        ("/login", Some("alice"), "/me"),
        ("/login?lang=uk", Some("alice"), "/me?lang=uk"),
        // A proxy that let the guest through: still to their (guest) page, no loop.
        ("/login", None, "/me"),
    ] {
        let (status, headers, _) = site.send(login(path, user)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{path}");
        assert_eq!(headers[header::LOCATION], to, "{path}");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }
    // Only through the proxy, like every page.
    let (status, ..) = site
        .send(Request::get("/login").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_week_shows_every_group_and_the_staircase_by_place() {
    let site = Site::new();
    let (status, page) = site.get("/", "alice").await;
    assert_eq!(status, StatusCode::OK);
    // The house bar, the tabs, this week's hero.
    assert!(
        page.contains("<header class=\"h-bar\"><a class=\"h-home\" href=\"https://example.org/\"")
    );
    assert!(page.contains("<a href=\"/\" aria-current=\"page\">This week</a>"));
    let (year, week) = state::current_iso_week();
    assert!(page.contains(&format!("<h1 id=\"hero-title\">Week {week}</h1>")));
    for group in ["2nd Floor", "Bathroom", "Staircase"] {
        assert!(page.contains(&format!(">{group}</a></h3>")), "{group}");
    }
    assert!(page.contains("2× per week (Mon–Tue, Thu–Fri)"));
    assert!(page.contains("every 2 weeks · the whole house"));

    // This week's staircase: one row per place, two people each, never per slot.
    let snapshot = site.snapshot(1).await;
    let stairs: Vec<&AssignmentInstance> = snapshot
        .assignments
        .iter()
        .filter(|a| a.house_wide)
        .collect();
    assert_eq!(stairs.len(), 4);
    let at = |place: &str| -> Vec<&AssignmentInstance> {
        stairs
            .iter()
            .copied()
            .filter(|a| a.place.as_deref() == Some(place))
            .collect()
    };
    let cards = &page[page.find("<div class=\"cards\">").unwrap()..];
    assert_eq!(people_at(cards, "Scharni")[0], names(&at("Scharni")));
    assert_eq!(people_at(cards, "Colbe")[0], names(&at("Colbe")));
    assert!(!page.contains("Scharni 1") && !page.contains("Colbe 2"));
    assert!(page.contains("<ul class=\"places side\">"));
    // The bathroom's slots are places of their own.
    assert_eq!(people_at(cards, "Toilet")[0].len(), 1);

    // The weeks ahead in short: the staircase again in two weeks, as one line.
    let later = add_weeks(year, week, 2);
    let section = &page[page
        .find(&format!("id=\"w-{}-{}\"", later.0, later.1))
        .unwrap()..];
    let section = &section[..section.find("</section>").unwrap()];
    let snapshot = site.snapshot(3).await;
    let stairs_later: Vec<&AssignmentInstance> = snapshot
        .assignments
        .iter()
        .filter(|a| a.house_wide && (a.iso_year, a.iso_week) == later)
        .collect();
    let line = &section[section.find(">Staircase</span>").unwrap()..];
    let line = &line[..line.find("</li>").unwrap()];
    for place in ["Scharni", "Colbe"] {
        let expected: Vec<&AssignmentInstance> = stairs_later
            .iter()
            .copied()
            .filter(|a| a.place.as_deref() == Some(place))
            .collect();
        assert_eq!(people_at(line, place), vec![names(&expected)], "{line}");
    }
    assert!(line.contains("</span>: "), "{line}");
    assert!(
        line.contains("<span class=\"amp\"> &amp; </span>"),
        "{line}"
    );
    // The week in between has no staircase.
    let between = add_weeks(year, week, 1);
    let section = &page[page
        .find(&format!("id=\"w-{}-{}\"", between.0, between.1))
        .unwrap()..];
    let section = &section[..section.find("</section>").unwrap()];
    assert!(!section.contains("Staircase") && section.contains("2nd Floor"));
    // The lock is free again.
    assert!(site.web.state.try_lock().is_ok());
}

use crate::state::add_weeks;

#[tokio::test]
async fn the_visitors_own_turns_are_marked_and_nobody_elses() {
    let site = Site::new();
    let snapshot = site.snapshot(7).await;
    let alice = snapshot
        .assignments
        .iter()
        .filter(|a| a.assignee.as_ref().is_some_and(|p| p.name == "Alice"))
        .count();
    assert!(alice > 0);
    let (_, page) = site.get("/", "Alice").await;
    // Each of her duties, and only hers, is "mine" and says "you".
    let mine: Vec<&str> = page.split("<span class=\"duty mine\">").skip(1).collect();
    assert_eq!(mine.len(), alice, "{page}");
    for duty in &mine {
        assert!(duty
            .starts_with("<strong class=\"name\">Alice</strong> <span class=\"you\">you</span>"));
    }
    assert!(page.contains("<h2 id=\"yours-title\">Your next turns</h2>"));
    assert!(page.contains("class=\"h-card duty-card mine\""));
    // Her first turn was done by Bob.
    assert!(page.contains("done by Bob"), "{page}");

    // Someone the plan doesn't know sees the same plan, without "you".
    let (status, page) = site.get("/", "zoe").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains("class=\"you\"") && !page.contains(" mine"));
    assert!(!page.contains("yours-title"));
    assert!(page.contains("Staircase"));
    let (_, me) = site.get("/me", "zoe").await;
    assert!(me.contains("Logged in as Kim K."));
    assert!(me.contains("can&#39;t find you"));
}

#[tokio::test]
async fn other_accounts_find_the_person_too() {
    // Bob's plan account is on matrix.org; the directory lists it for his login.
    let site = Site::new();
    let (_, page) = site
        .get_with(
            "/me",
            "bobby",
            &[(MATRIX_IDS_HEADER, "@nobody:x.org, @bob:matrix.org")],
        )
        .await;
    assert!(!page.contains("can&#39;t find you"), "{page}");
    assert!(page.contains("class=\"h-card duty-card mine\""));
    // Carol's house account is linked to her plan account (`directory::person_account`).
    let (accounts, _) = Accounts::new(vec![vec![
        UserId::parse("@carol:example.org").unwrap(),
        UserId::parse("@carol:matrix.org").unwrap(),
    ]]);
    let site = Site::with(house(), accounts);
    let (_, page) = site.get("/people", "Carol").await;
    assert!(
        page.contains("<h2>carol <span class=\"you\">you</span></h2>"),
        "{page}"
    );
    // A login that can't be a Matrix ID is nobody in the plan, but gets in.
    let (status, page) = site.get("/me", "a b").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("can&#39;t find you"));
}

#[tokio::test]
async fn names_are_escaped_and_no_matrix_id_or_calendar_link_shows() {
    let site = Site::new();
    let escaped = "&lt;b&gt;&amp;&quot;";
    for path in ["/", "/groups", "/people", "/me"] {
        let (status, page) = site.get(path, "eve").await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(!page.contains(EVE), "{path}");
        assert!(
            !page.contains(":example.org") && !page.contains(":matrix.org"),
            "{path}"
        );
        assert!(
            !page.contains("/ical/") && !page.contains("webcal"),
            "{path}"
        );
        assert!(
            !page.contains("<script") && !page.contains("style="),
            "{path}"
        );
        if path != "/me" {
            assert!(page.contains(escaped), "{path}");
        }
    }
    let (_, me) = site.get("/me", "eve").await;
    assert!(me.contains("your own calendar in the house cloud"));
    // A display name with markup is plain text in every language too.
    let mut state = house();
    state.persons[0].display_name = "<script>x</script>".into();
    let site = Site::with(state, Accounts::default());
    let (_, page) = site.get("/people", "dan").await;
    assert!(page.contains("&lt;script&gt;x&lt;/script&gt;") && !page.contains("<script>"));
}

#[tokio::test]
async fn every_page_renders_in_every_language() {
    let site = Site::new();
    let group = site.web.state.lock().await.cleaning_groups[2].id.clone();
    let group_path = format!("/g/{group}");
    for (lang, home) in [
        (Lang::De, "Diese Woche"),
        (Lang::En, "This week"),
        (Lang::Uk, "Цей тиждень"),
    ] {
        for path in ["/", "/groups", group_path.as_str(), "/people", "/me"] {
            let (status, page) = site
                .get(&format!("{path}?lang={}", lang.code()), "alice")
                .await;
            assert_eq!(status, StatusCode::OK, "{path} {lang:?}");
            assert!(
                page.contains(&format!("<html lang=\"{}\">", lang.code())),
                "{path}"
            );
            assert!(page.contains(home), "{path} {lang:?}");
            assert!(page.contains(&esc(t(lang, "page.footer"))));
            // Chosen by ?lang= against the browser's English: the links keep it.
            if lang != Lang::En {
                assert!(
                    page.contains(&format!("href=\"/people?lang={}\"", lang.code())),
                    "{path}"
                );
            }
        }
    }
    // The house cookie decides before the browser.
    let (_, page) = site
        .get_with("/", "alice", &[("cookie", "house_lang=uk")])
        .await;
    assert!(page.contains("<html lang=\"uk\">") && page.contains("href=\"/people\""));
    let (_, page) = site
        .get_with("/", "alice", &[("cookie", "house_lang=de")])
        .await;
    assert!(page.contains("Wer diese Woche dran ist"));
    // The language switch keeps the page and its weeks.
    let (_, page) = site.get("/groups?weeks=3", "alice").await;
    assert!(page.contains("href=\"/groups?weeks=3&amp;lang=de\" hreflang=\"de\""));
    assert!(page.contains("hreflang=\"en\" lang=\"en\" aria-current=\"true\""));
}

#[tokio::test]
async fn weeks_are_clamped() {
    let site = Site::new();
    let count = |page: &str| page.matches("<section class=\"week-compact\"").count();
    for (query, ahead) in [
        ("", 6),
        ("?weeks=100", 26),
        ("?weeks=0", 1),
        ("?weeks=-5", 1),
        ("?weeks=abc", 6),
        ("?weeks=3", 3),
    ] {
        let (status, page) = site.get(&format!("/{query}"), "alice").await;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert_eq!(count(&page), ahead, "{query}");
    }
    let rows = |page: &str| page.matches("<li class=\"week").count();
    let (_, page) = site.get("/groups?weeks=999", "alice").await;
    // 26 weeks of the floor and the bathroom, 13 of the staircase.
    assert_eq!(rows(&page), 26 + 26 + 13);
    let (_, page) = site.get("/groups?weeks=1", "alice").await;
    assert_eq!(rows(&page), 3);
}

#[tokio::test]
async fn groups_people_and_me() {
    let site = Site::new();
    let (state, stairs) = {
        let state = site.web.state.lock().await;
        let stairs = state.cleaning_groups[2].id.clone();
        (state.clone(), stairs)
    };

    let (_, page) = site.get("/groups", "alice").await;
    assert!(page.contains("aria-current=\"date\""));
    assert!(page.contains("<section class=\"h-card timeline house\""));
    let (status, page) = site.get(&format!("/g/{stairs}"), "alice").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<h1 id=\"g-") && page.contains(">Staircase</h1>"));
    assert!(!page.contains("2nd Floor"));
    let (status, page) = site.get("/g/nope", "alice").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(page.contains("There is no such group"));

    // Everyone, by name, with their count and load of the next 8 weeks.
    let (_, page) = site.get("/people", "alice").await;
    let order: Vec<usize> = ["Alice", "Bob", "carol", "Dan", "Finn"]
        .iter()
        .map(|n| {
            page.find(&format!("<h2>{n}"))
                .unwrap_or_else(|| panic!("{n}"))
        })
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    let snapshot = build_schedule_from(&state, state::current_iso_week(), 8);
    let dan: Vec<&AssignmentInstance> = snapshot
        .assignments
        .iter()
        .filter(|a| a.assignee.as_ref().is_some_and(|p| p.name == "Dan"))
        .collect();
    let load: f64 = dan.iter().map(|a| a.load).sum();
    let card = &page[page.find("<h2>Dan").unwrap()..];
    assert!(card.contains(&format!(
        "<dt>Turns</dt><dd>{}</dd></div><div><dt>Load</dt><dd>{}</dd>",
        dan.len(),
        i18n::number(Lang::En, load)
    )));

    // Mine: every turn of the next 12 weeks not over yet, with who shares the place.
    let (_, page) = site.get("/me", "alice").await;
    let snapshot = build_schedule_from(&state, state::current_iso_week(), 12);
    let upcoming = snapshot
        .for_person(&state.persons[0].id)
        .into_iter()
        .filter(|a| a.end >= state::today())
        .count();
    assert_eq!(
        page.matches("class=\"h-card duty-card mine\"").count(),
        upcoming
    );
    let shared = snapshot
        .for_person(&state.persons[0].id)
        .into_iter()
        .find(|a| a.house_wide && a.end >= state::today());
    if shared.is_some() {
        assert!(page.contains("with "), "{page}");
    }
}

#[tokio::test]
async fn the_style_sheet_is_served() {
    let site = Site::new();
    let (status, headers, css) = site
        .send(Request::get("/style.css").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/css");
    assert!(css.contains("var(--h-accent)"));
    let (_, page) = site.get("/", "alice").await;
    let kit = page.find("/_house/ui.css").unwrap();
    assert!(
        kit < page
            .find(&format!("/style.css?v={}", asset_version()))
            .unwrap()
    );
}

#[test]
fn logins_become_house_accounts_and_the_house_bar_finds_home() {
    assert_eq!(
        matrix_id(" Alice ", "example.org").unwrap().as_str(),
        "@alice:example.org"
    );
    assert!(matrix_id("a b", "example.org").is_none());
    assert!(matrix_id("", "example.org").is_none());
    assert_eq!(
        home_link("https://putzplan.example.org", None, None),
        Some(("https://example.org/".into(), "Example".into()))
    );
    assert_eq!(home_link("http://192.168.1.2:8080", None, None), None);
    assert_eq!(
        home_link(
            "https://p.example.org",
            Some("https://home.example.org/"),
            Some("Haus")
        ),
        Some(("https://home.example.org/".into(), "Haus".into()))
    );
    let mut headers = HeaderMap::new();
    headers.insert(PROXY_SECRET_HEADER, HeaderValue::from_static(SECRET));
    assert!(proxy_secret_ok(&headers, SECRET.as_bytes()));
    assert!(!proxy_secret_ok(
        &headers,
        b"another-secret-of-at-least-32-chars!"
    ));
    assert!(!proxy_secret_ok(&HeaderMap::new(), SECRET.as_bytes()));
    assert_eq!(weeks_param(Some("lang=en&weeks=4")), Some(4));
    assert_eq!(weeks_param(Some("weeks=40")), Some(26));
    assert_eq!(weeks_param(Some("weeks=")), None);
}
