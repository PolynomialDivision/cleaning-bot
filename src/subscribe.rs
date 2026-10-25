//! The page a person sees when they open their feed link in a browser
//! (tapping it in Element, say), instead of a downloaded `.ics` file: how to
//! add the feed to their calendar app, in German, English or Ukrainian.
//!
//! Pure functions; `http` decides when to answer with them. The pages show
//! no name or other personal data: only the feed address, which the person
//! already has. They need no inline script or style (strict CSP): the house
//! UI kit (`/_house/ui.css`, same origin) and this bot's own small style
//! sheet and copy script under `/assets/`. Without the kit (or without
//! script) the page still works: the styles fall back, and the address is a
//! plain read-only field to select and copy.

use axum::http::{header, HeaderMap};

/// Response headers of both pages: nothing but this origin, no referrer
/// (the address carries the token), never cached or indexed.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
                       base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub const STYLE: &str = include_str!("subscribe/subscribe.css");
pub const SCRIPT: &str = include_str!("subscribe/subscribe.js");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    De,
    En,
    Uk,
}

impl Lang {
    fn parse(tag: &str) -> Option<Lang> {
        let primary = tag.trim().split(['-', '_']).next()?.to_ascii_lowercase();
        match primary.as_str() {
            "de" => Some(Lang::De),
            "en" => Some(Lang::En),
            "uk" | "ua" => Some(Lang::Uk),
            _ => None,
        }
    }

    /// For `<html lang>` and `?lang=`.
    pub fn code(self) -> &'static str {
        match self {
            Lang::De => "de",
            Lang::En => "en",
            Lang::Uk => "uk",
        }
    }
}

/// Whether the client would rather have HTML than a calendar: a browser
/// (`text/html,…,*/*;q=0.8`), not a calendar app (`text/calendar`, `*/*`
/// or no Accept at all, which keep getting the `.ics`).
pub fn wants_html(headers: &HeaderMap) -> bool {
    let accept: Vec<&str> = headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    if accept.is_empty() {
        return false;
    }
    let ranges = media_ranges(&accept.join(","));
    let html = quality(&ranges, "text", "html");
    html > 0.0 && html > quality(&ranges, "text", "calendar")
}

/// `(type, subtype, q)` of each media range in an Accept header.
fn media_ranges(accept: &str) -> Vec<(String, String, f32)> {
    accept
        .split(',')
        .filter_map(|range| {
            let mut parts = range.split(';');
            let (kind, sub) = parts.next()?.trim().split_once('/')?;
            let q = parts
                .filter_map(|p| p.trim().strip_prefix("q="))
                .find_map(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            Some((
                kind.trim().to_ascii_lowercase(),
                sub.trim().to_ascii_lowercase(),
                q,
            ))
        })
        .collect()
}

/// The q of `kind/sub` by the most specific range that matches it (RFC 9110
/// 12.5.1); 0 if none does.
fn quality(ranges: &[(String, String, f32)], kind: &str, sub: &str) -> f32 {
    let specificity = |k: &str, s: &str| match (k, s) {
        (k, s) if k == kind && s == sub => Some(2),
        (k, "*") if k == kind => Some(1),
        ("*", "*") => Some(0),
        _ => None,
    };
    ranges
        .iter()
        .filter_map(|(k, s, q)| specificity(k, s).map(|level| (level, *q)))
        .max_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)))
        .map_or(0.0, |(_, q)| q)
}

/// The page's language: `?lang=` (the page's own switcher), the house-wide
/// `house_lang` cookie the start page sets, the browser's Accept-Language,
/// else German.
pub fn language(headers: &HeaderMap, query: Option<&str>) -> Lang {
    if let Some(lang) = query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|pair| pair.strip_prefix("lang="))
        .find_map(Lang::parse)
    {
        return lang;
    }
    let cookie = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().strip_prefix("house_lang="))
        .find_map(Lang::parse);
    if let Some(lang) = cookie {
        return lang;
    }
    let mut wanted: Vec<(f32, usize, Lang)> = headers
        .get_all(header::ACCEPT_LANGUAGE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .enumerate()
        .filter_map(|(i, item)| {
            let mut parts = item.split(';');
            let lang = Lang::parse(parts.next()?)?;
            let q = parts
                .filter_map(|p| p.trim().strip_prefix("q="))
                .find_map(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            (q > 0.0).then_some((q, i, lang))
        })
        .collect();
    // Highest q first; the earlier one among equals.
    wanted.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    wanted.first().map_or(Lang::De, |w| w.2)
}

struct Texts {
    title: &'static str,
    intro: &'static str,
    apple: &'static str,
    google: &'static str,
    android: &'static str,
    address: &'static str,
    copy: &'static str,
    copied: &'static str,
    private: &'static str,
    invalid_title: &'static str,
    invalid: &'static str,
}

fn texts(lang: Lang) -> Texts {
    match lang {
        Lang::De => Texts {
            title: "Putzplan im Kalender",
            intro: "Abonniere deine Putztermine: Neue und geänderte Termine erscheinen \
                    dann von selbst in deinem Kalender.",
            apple: "Zum iPhone-/Mac-Kalender hinzufügen",
            google: "Zu Google Kalender hinzufügen",
            android: "Android: Füge den Kalender oben bei Google Kalender hinzu, oder \
                      installiere die App ICSx⁵ (oder DAVx⁵), tippe auf „+“ und füge die \
                      Adresse unten ein.",
            address: "Kalender-Adresse",
            copy: "Kopieren",
            copied: "Kopiert ✓",
            private: "Die Adresse ist nur für dich: Gib sie nicht weiter. Eine neue \
                      bekommst du mit !ical reset im privaten Chat mit dem Putzbot.",
            invalid_title: "Kalender-Link ungültig",
            invalid: "Dieser Kalender-Link gilt nicht (mehr), vielleicht wurde er \
                      erneuert. Schreib dem Putzbot im privaten Chat !ical, dann bekommst \
                      du deinen aktuellen Link.",
        },
        Lang::En => Texts {
            title: "Cleaning plan in your calendar",
            intro: "Subscribe to your cleaning turns: new and changed turns then show up \
                    in your calendar by themselves.",
            apple: "Add to iPhone/Mac calendar",
            google: "Add to Google Calendar",
            android: "Android: add the calendar to Google Calendar above, or install the \
                      app ICSx⁵ (or DAVx⁵), tap “+” and paste the address below.",
            address: "Calendar address",
            copy: "Copy",
            copied: "Copied ✓",
            private: "This address is just for you: don't pass it on. Get a new one with \
                      !ical reset in your private chat with the cleaning bot.",
            invalid_title: "Calendar link not valid",
            invalid: "This calendar link is not valid (any more); it may have been \
                      replaced. Send !ical to the cleaning bot in a private chat to get \
                      your current link.",
        },
        Lang::Uk => Texts {
            title: "Графік прибирання в календарі",
            intro: "Підпишися на свої чергування: нові та змінені дати самі з'являтимуться \
                    у твоєму календарі.",
            apple: "Додати в календар iPhone/Mac",
            google: "Додати в Google Календар",
            android: "Android: додай календар у Google Календар вище або встанови \
                      застосунок ICSx⁵ (чи DAVx⁵), натисни «+» і встав адресу нижче.",
            address: "Адреса календаря",
            copy: "Копіювати",
            copied: "Скопійовано ✓",
            private: "Ця адреса лише для тебе: не передавай її іншим. Нову адресу дає \
                      команда !ical reset у приватному чаті з ботом прибирання.",
            invalid_title: "Посилання на календар недійсне",
            invalid: "Це посилання на календар недійсне (вже), можливо, його замінили. \
                      Напиши боту прибирання !ical у приватному чаті, щоб отримати \
                      актуальне посилання.",
        },
    }
}

/// The page for a valid feed at `feed_url` (`https://…/ical/<token>.ics`).
pub fn subscribe_page(lang: Lang, feed_url: &str) -> String {
    let t = texts(lang);
    let webcal = webcal_url(feed_url);
    let google = format!(
        "https://calendar.google.com/calendar/render?cid={}",
        percent_encode(&webcal)
    );
    let body = format!(
        "<h1>{title}</h1>\n\
         <p>{intro}</p>\n\
         <p class=\"actions\">\
         <a class=\"h-btn primary\" href=\"{webcal}\">{apple}</a>\
         <a class=\"h-btn\" href=\"{google}\" target=\"_blank\" rel=\"noopener noreferrer\">{google_label}</a>\
         </p>\n\
         <p class=\"h-note\">{android}</p>\n\
         <div class=\"h-field\">\
         <label for=\"feed\">{address}</label>\
         <div class=\"copy-row\">\
         <input id=\"feed\" type=\"text\" readonly value=\"{feed}\" autocomplete=\"off\" spellcheck=\"false\">\
         <button id=\"copy\" class=\"h-btn\" type=\"button\" data-done=\"{copied}\" hidden>{copy}</button>\
         </div></div>\n\
         <p class=\"muted\">{private}</p>",
        title = esc(t.title),
        intro = esc(t.intro),
        webcal = esc(&webcal),
        apple = esc(t.apple),
        google = esc(&google),
        google_label = esc(t.google),
        android = esc(t.android),
        address = esc(t.address),
        feed = esc(feed_url),
        copied = esc(t.copied),
        copy = esc(t.copy),
        private = esc(t.private),
    );
    page(lang, t.title, &body)
}

/// The page for a link that is unknown, revoked or malformed.
pub fn invalid_page(lang: Lang) -> String {
    let t = texts(lang);
    let body = format!(
        "<h1>{}</h1>\n<p>{}</p>",
        esc(t.invalid_title),
        esc(t.invalid)
    );
    page(lang, t.invalid_title, &body)
}

fn page(lang: Lang, title: &str, body: &str) -> String {
    let switcher: Vec<String> = [(Lang::De, "DE"), (Lang::En, "EN"), (Lang::Uk, "УКР")]
        .into_iter()
        .map(|(l, label)| {
            if l == lang {
                format!("<strong>{label}</strong>")
            } else {
                format!(
                    "<a href=\"?lang={}\" hreflang=\"{0}\">{label}</a>",
                    l.code()
                )
            }
        })
        .collect();
    format!(
        "<!doctype html>\n\
         <html lang=\"{code}\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"robots\" content=\"noindex, nofollow\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
         <title>{title}</title>\n\
         <link rel=\"stylesheet\" href=\"/_house/ui.css\">\n\
         <link rel=\"stylesheet\" href=\"/assets/subscribe.css\">\n\
         <script src=\"/assets/subscribe.js\" defer></script>\n\
         </head>\n\
         <body>\n\
         <header class=\"h-bar\"><span class=\"brand\">🧹 🗓</span><nav class=\"lang\">{switcher}</nav></header>\n\
         <main class=\"h-card\">\n{body}\n</main>\n\
         </body>\n\
         </html>\n",
        code = lang.code(),
        title = esc(title),
        switcher = switcher.join(" · "),
    )
}

/// The feed as a `webcal://` link, which iOS and macOS open in Calendar as
/// a subscription.
pub fn webcal_url(feed_url: &str) -> String {
    let rest = feed_url
        .strip_prefix("https://")
        .or_else(|| feed_url.strip_prefix("http://"))
        .unwrap_or(feed_url);
    format!("webcal://{rest}")
}

/// RFC 3986 percent-encoding of everything but the unreserved characters.
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn with(name: header::HeaderName, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn browsers_get_the_page_and_calendar_apps_the_feed() {
        let accept = |v: &str| wants_html(&with(header::ACCEPT, v));
        // Mobile Safari, Chrome on Android, Firefox.
        assert!(accept(
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        ));
        assert!(accept("text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7"));
        // Calendar apps and fetchers.
        assert!(!wants_html(&HeaderMap::new()));
        assert!(!accept("*/*"));
        assert!(!accept("text/calendar"));
        assert!(!accept("text/calendar, */*;q=0.5"));
        assert!(!accept("text/html;q=0.5, text/calendar"));
        assert!(!accept("text/*"));
        assert!(!accept("text/html;q=0, */*"));
        assert!(!accept("application/json"));
        assert!(accept("text/html, text/calendar;q=0.9"));
    }

    #[test]
    fn language_from_switcher_cookie_browser_or_german() {
        assert_eq!(language(&HeaderMap::new(), None), Lang::De);
        let cookie = with(header::COOKIE, "theme=dark; house_lang=uk");
        assert_eq!(language(&cookie, None), Lang::Uk);
        assert_eq!(language(&cookie, Some("lang=en")), Lang::En);
        assert_eq!(language(&cookie, Some("lang=fr")), Lang::Uk);
        let browser = with(header::ACCEPT_LANGUAGE, "fr-FR, en-GB;q=0.8, de;q=0.7");
        assert_eq!(language(&browser, None), Lang::En);
        let browser = with(header::ACCEPT_LANGUAGE, "uk-UA;q=0.9, de-DE");
        assert_eq!(language(&browser, None), Lang::De);
        let browser = with(header::ACCEPT_LANGUAGE, "uk-UA, ru;q=0.8");
        assert_eq!(language(&browser, None), Lang::Uk);
        let mut both = with(header::ACCEPT_LANGUAGE, "en");
        both.insert(header::COOKIE, HeaderValue::from_static("house_lang=de"));
        assert_eq!(language(&both, None), Lang::De);
        assert_eq!(
            language(&with(header::ACCEPT_LANGUAGE, "fr"), None),
            Lang::De
        );
    }

    #[test]
    fn the_page_offers_every_way_to_subscribe_without_inline_code() {
        let url = "https://cal.example.org/ical/0123abcd.ics";
        for lang in [Lang::De, Lang::En, Lang::Uk] {
            let html = subscribe_page(lang, url);
            assert!(html.contains(&format!("<html lang=\"{}\">", lang.code())));
            assert!(html.contains("href=\"webcal://cal.example.org/ical/0123abcd.ics\""));
            assert!(html.contains(
                "href=\"https://calendar.google.com/calendar/render?cid=webcal%3A%2F%2Fcal.example.org%2Fical%2F0123abcd.ics\""
            ));
            assert!(html.contains("ICSx⁵") && html.contains("DAVx⁵"));
            assert!(html.contains(&format!("readonly value=\"{url}\"")));
            // The house kit first, then the bot's own sheet.
            let kit = html.find("/_house/ui.css").unwrap();
            assert!(kit < html.find("/assets/subscribe.css").unwrap());
            // Nothing inline (CSP): no <style>, no script body, no handlers.
            assert!(!html.contains("<style") && !html.contains("style="));
            assert!(!html.contains("<script>") && !html.contains("onclick="));
        }
        assert!(subscribe_page(Lang::De, url).contains("Zum iPhone-/Mac-Kalender hinzufügen"));
        assert!(subscribe_page(Lang::En, url).contains("Add to iPhone/Mac calendar"));
        assert!(subscribe_page(Lang::Uk, url).contains("Додати в календар iPhone/Mac"));
    }

    #[test]
    fn the_invalid_page_is_translated_and_escapes_nothing_it_did_not_write() {
        assert!(invalid_page(Lang::De).contains("Kalender-Link ungültig"));
        assert!(invalid_page(Lang::En).contains("Calendar link not valid"));
        assert!(invalid_page(Lang::Uk).contains("Посилання на календар недійсне"));
        let html = subscribe_page(Lang::En, "https://x.example.org/ical/\"><b>.ics");
        assert!(!html.contains("\"><b>"));
        assert_eq!(webcal_url("http://a.example/x"), "webcal://a.example/x");
    }
}
