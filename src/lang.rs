//! The language of the website's pages: German, English or Ukrainian, as
//! the house's other pages choose it.

use axum::http::{header, HeaderMap};

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

/// A page's language: `?lang=` (the page's own switcher), the house-wide
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
