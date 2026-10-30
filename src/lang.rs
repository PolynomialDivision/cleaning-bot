//! The language of the website's pages: German, English or Ukrainian, as
//! the house's other pages choose it.

#[cfg(test)]
use axum::http::header;
use axum::http::HeaderMap;

pub use mxbot_common::web::lang::Lang;

/// A page's language: `?lang=` (the page's own switcher), the house-wide
/// `house_lang` cookie the start page sets, the browser's Accept-Language,
/// else German (`mxbot_common::web::lang::pick`).
pub fn language(headers: &HeaderMap, query: Option<&str>) -> Lang {
    mxbot_common::web::lang::pick(headers, query, Lang::De)
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
}
