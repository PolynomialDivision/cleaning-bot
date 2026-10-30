//! The website's texts in German, English and Ukrainian: a key table, `t(lang, key)`, and
//! dates, rhythms and numbers as each language writes them.
//!
//! The language is chosen as on the house's other pages (`crate::lang::language`):
//! `?lang=`, the house's `house_lang` cookie, Accept-Language, else German.

use chrono::{Datelike, NaiveDate, Weekday};
use mxbot_common::web::lang;

pub use crate::lang::Lang;

/// The text for `key`; empty when there is none (a test checks every key used).
pub fn t(lang: Lang, key: &str) -> &'static str {
    lang::text(TEXTS, lang, key)
}

/// `t` with each `{}` replaced by the next of `args`.
pub fn tf(lang: Lang, key: &str, args: &[&str]) -> String {
    lang::fill(TEXTS, lang, key, args)
}

pub fn weekday(lang: Lang, day: Weekday) -> &'static str {
    let i = day.num_days_from_monday() as usize;
    match lang {
        Lang::De => ["Mo", "Di", "Mi", "Do", "Fr", "Sa", "So"][i],
        Lang::En => ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][i],
        Lang::Uk => ["пн", "вт", "ср", "чт", "пт", "сб", "нд"][i],
    }
}

fn month(lang: Lang, date: NaiveDate) -> &'static str {
    let i = date.month0() as usize;
    match lang {
        Lang::De => [
            "Jan", "Feb", "März", "Apr", "Mai", "Juni", "Juli", "Aug", "Sep", "Okt", "Nov", "Dez",
        ][i],
        Lang::En => [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ][i],
        Lang::Uk => [
            "січ", "лют", "бер", "квіт", "трав", "черв", "лип", "серп", "вер", "жовт", "лист",
            "груд",
        ][i],
    }
}

/// "12." in German, "12" elsewhere.
fn day(lang: Lang, date: NaiveDate) -> String {
    match lang {
        Lang::De => format!("{}.", date.day()),
        _ => date.day().to_string(),
    }
}

/// "Mo 12. Okt", "Mon 12 Oct", "пн 12 жовт".
pub fn short_date(lang: Lang, date: NaiveDate) -> String {
    format!(
        "{} {} {}",
        weekday(lang, date.weekday()),
        day(lang, date),
        month(lang, date)
    )
}

/// "12. – 18. Okt", "28 Sep – 4 Oct", or one day ("12. Okt").
pub fn date_range(lang: Lang, from: NaiveDate, until: NaiveDate) -> String {
    if from == until {
        format!("{} {}", day(lang, from), month(lang, from))
    } else if from.month() == until.month() {
        format!(
            "{} – {} {}",
            day(lang, from),
            day(lang, until),
            month(lang, from)
        )
    } else {
        format!(
            "{} {} – {} {}",
            day(lang, from),
            month(lang, from),
            day(lang, until),
            month(lang, until)
        )
    }
}

/// The days of a shift: "Mo–Di", or "Do" for one day.
pub fn days(lang: Lang, from: NaiveDate, until: NaiveDate) -> String {
    if from == until {
        weekday(lang, from.weekday()).to_owned()
    } else {
        format!(
            "{}–{}",
            weekday(lang, from.weekday()),
            weekday(lang, until.weekday())
        )
    }
}

/// "KW 43", with the year once it isn't `this_year`: "KW 1 · 2027".
pub fn week_label(lang: Lang, (year, week): (i32, u32), this_year: i32) -> String {
    let label = tf(lang, "week.label", &[&week.to_string()]);
    if year == this_year {
        label
    } else {
        format!("{label} · {year}")
    }
}

/// A group's rhythm: "wöchentlich", "alle 2 Wochen", "2× pro Woche (Mo–Di, Do–Fr)",
/// "alle 2 Wochen · 2× pro Woche (…)". `shifts`: each shift's first and last weekday.
pub fn rhythm(lang: Lang, every_weeks: u32, shifts: &[(Weekday, Weekday)]) -> String {
    let every = (every_weeks > 1).then(|| tf(lang, "rhythm.every", &[&every_weeks.to_string()]));
    let whole_week = matches!(shifts, [] | [(Weekday::Mon, Weekday::Sun)]);
    if whole_week {
        return every.unwrap_or_else(|| t(lang, "rhythm.weekly").to_owned());
    }
    let labels: Vec<String> = shifts
        .iter()
        .map(|(from, until)| {
            if from == until {
                weekday(lang, *from).to_owned()
            } else {
                format!("{}–{}", weekday(lang, *from), weekday(lang, *until))
            }
        })
        .collect();
    let times = format!(
        "{} ({})",
        tf(lang, "rhythm.times", &[&shifts.len().to_string()]),
        labels.join(", ")
    );
    match every {
        Some(every) => format!("{every} · {times}"),
        None => times,
    }
}

/// How far off a turn is, from `today`: "jetzt, bis Di", "heute", "morgen", "in 3 Tagen".
pub fn relative(lang: Lang, from: NaiveDate, until: NaiveDate, today: NaiveDate) -> String {
    if from <= today && today <= until {
        return if until == today {
            t(lang, "rel.today").to_owned()
        } else {
            tf(lang, "rel.now", &[weekday(lang, until.weekday())])
        };
    }
    let ahead = (from - today).num_days();
    match ahead {
        n if n < 0 => String::new(),
        1 => t(lang, "rel.tomorrow").to_owned(),
        n => tf(lang, in_days_key(lang, n), &[&n.to_string()]),
    }
}

/// "in N days" in the form `n` takes: Ukrainian has three (1, 21 день; 2–4, 22 дні; 5–20
/// днів), German and English two (`_few` is the same as `_many` there).
fn in_days_key(lang: Lang, n: i64) -> &'static str {
    match lang {
        Lang::Uk if n % 10 == 1 && n % 100 != 11 => "rel.in_days_one",
        Lang::Uk if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) => {
            "rel.in_days_few"
        }
        Lang::Uk => "rel.in_days_many",
        _ if n == 1 => "rel.in_days_one",
        _ => "rel.in_days_many",
    }
}

/// A load figure: "4", "4,5" (German, Ukrainian) or "4.5".
pub fn number(lang: Lang, value: f64) -> String {
    let text = format!("{:.1}", value);
    let text = text.strip_suffix(".0").unwrap_or(&text).to_owned();
    match lang {
        Lang::En => text,
        _ => text.replace('.', ","),
    }
}

/// (key, German, English, Ukrainian).
pub const TEXTS: &lang::Texts = &[
    ("app.name", "Putzplan", "Cleaning plan", "Графік прибирання"),
    ("home.title", "Zur Startseite", "To the start page", "На головну сторінку"),
    ("lang.label", "Sprache", "Language", "Мова"),
    ("nav.label", "Ansichten", "Views", "Розділи"),
    ("nav.week", "Diese Woche", "This week", "Цей тиждень"),
    ("nav.groups", "Gruppen", "Groups", "Групи"),
    ("nav.people", "Alle", "People", "Усі"),
    ("nav.me", "Meine", "Mine", "Мої"),
    (
        "refused.proxy",
        "Kein Zugang: Diese Seite ist nur über die Anmeldung des Hauses erreichbar.",
        "No access: this page is only reachable through the house's login.",
        "Немає доступу: ця сторінка доступна лише через вхід будинку.",
    ),
    (
        "refused.login",
        "Bitte melde dich zuerst mit deinem Hauskonto an.",
        "Please log in with your house account first.",
        "Спершу увійди зі своїм обліковим записом будинку.",
    ),
    (
        "refused.group",
        "Dein Konto darf den Putzplan nicht sehen. Frag die Admins des Hauses.",
        "Your account may not see the cleaning plan. Ask the house's admins.",
        "Твоєму обліковому запису не можна переглядати графік. Звернися до адмінів будинку.",
    ),
    (
        "page.footer",
        "Nur zum Ansehen: derselbe Plan wie im Putzraum auf Matrix. Ändern geht dort: ✅ wenn erledigt, !sos wenn du nicht kannst, Tauschen.",
        "View only: the same plan as in the cleaning room on Matrix. Changes happen there: ✅ when done, !sos if you can't make it, swaps.",
        "Лише для перегляду: той самий графік, що й у кімнаті прибирання в Matrix. Зміни там: ✅ коли готово, !sos якщо не можеш, обміни.",
    ),
    ("week.label", "KW {}", "Week {}", "Тиждень {}"),
    ("week.this", "Diese Woche", "This week", "Цей тиждень"),
    ("week.today", "Heute ist {}.", "Today is {}.", "Сьогодні {}."),
    ("week.groups", "Wer diese Woche dran ist", "Who's on this week", "Хто чергує цього тижня"),
    ("week.ahead", "Die nächsten Wochen", "The weeks ahead", "Наступні тижні"),
    ("week.more", "{} Wochen zeigen", "Show {} weeks", "Показати {} тиж."),
    ("week.empty", "Nichts zu putzen.", "Nothing to clean.", "Нічого прибирати."),
    ("week.now", "diese Woche", "this week", "цей тиждень"),
    ("turn.now", "läuft", "now", "зараз"),
    ("turn.you", "du", "you", "ти"),
    ("yours.title", "Deine nächsten Termine", "Your next turns", "Твої наступні чергування"),
    (
        "yours.none",
        "In den nächsten {} Wochen hast du keinen Termin.",
        "You have no turn in the next {} weeks.",
        "У найближчі {} тиж. у тебе немає чергувань.",
    ),
    ("yours.all", "Alle deine Termine", "All your turns", "Усі твої чергування"),
    (
        "week.signin",
        "Anmelden und deine Termine sehen",
        "Log in to see your turns",
        "Увійди, щоб бачити свої чергування",
    ),
    ("status.open", "offen", "open", "відкрито"),
    ("status.done", "erledigt", "done", "готово"),
    ("status.done_by", "erledigt von {}", "done by {}", "зробив(-ла) {}"),
    ("status.missed", "verpasst", "missed", "пропущено"),
    ("status.skipped", "ausgelassen", "skipped", "скасовано"),
    ("status.away", "weg", "away", "немає вдома"),
    ("status.nobody", "niemand eingeteilt", "nobody assigned", "нікого не призначено"),
    ("rel.today", "heute", "today", "сьогодні"),
    ("rel.now", "jetzt, bis {}", "now, until {}", "зараз, до {}"),
    ("rel.tomorrow", "morgen", "tomorrow", "завтра"),
    ("rel.in_days_one", "in {} Tag", "in {} day", "через {} день"),
    ("rel.in_days_few", "in {} Tagen", "in {} days", "через {} дні"),
    ("rel.in_days_many", "in {} Tagen", "in {} days", "через {} днів"),
    ("rhythm.weekly", "wöchentlich", "weekly", "щотижня"),
    ("rhythm.every", "alle {} Wochen", "every {} weeks", "раз на {} тиж."),
    ("rhythm.times", "{}× pro Woche", "{}× per week", "{}× на тиждень"),
    ("group.house", "das ganze Haus", "the whole house", "увесь будинок"),
    ("group.not_due", "Diese Woche nicht. Als Nächstes: {}", "Not this week. Next: {}", "Цього тижня ні. Далі: {}"),
    (
        "group.none_ahead",
        "Keine Termine in den nächsten {} Wochen.",
        "No turns in the next {} weeks.",
        "Немає чергувань у найближчі {} тиж.",
    ),
    ("group.timeline", "Die nächsten {} Wochen", "The next {} weeks", "Наступні {} тиж."),
    ("groups.title", "Gruppen", "Groups", "Групи"),
    ("groups.unknown", "Diese Gruppe gibt es nicht (mehr).", "There is no such group (any more).", "Такої групи немає (вже)."),
    ("groups.all", "Alle Gruppen", "All groups", "Усі групи"),
    ("people.title", "Alle im Plan", "Everyone in the plan", "Усі в графіку"),
    (
        "people.intro",
        "Wer in den nächsten {} Wochen (ab dieser) wann dran ist. Aufwand: die Räume eines Termins, gewichtet, wie die Rotation sie ausgleicht.",
        "Who cleans when in the next {} weeks (from this one). Load: a turn's rooms, weighted, as the rotation balances them.",
        "Хто й коли прибирає в найближчі {} тиж. (від цього). Навантаження: кімнати чергування з вагою, як їх вирівнює ротація.",
    ),
    ("people.turns", "Termine", "Turns", "Чергування"),
    ("people.load", "Aufwand", "Load", "Навантаження"),
    ("people.none", "Keine Termine in diesen Wochen.", "No turns in these weeks.", "Немає чергувань у ці тижні."),
    ("people.empty", "Noch niemand im Plan.", "Nobody in the plan yet.", "У графіку ще нікого немає."),
    ("me.title", "Deine Termine", "Your turns", "Твої чергування"),
    (
        "me.unknown",
        "Wir finden dich nicht im Putzplan. Bist du schon in einer Gruppe? Schreib dem Putzbot !mygroups. Ist dein Matrix-Konto ein anderes als dein Hauskonto, können die Admins es im Verzeichnis eintragen.",
        "We can't find you in the cleaning plan. Are you in a group yet? Send !mygroups to the cleaning bot. If your Matrix account isn't your house account, the admins can add it to the directory.",
        "Ми не знаходимо тебе в графіку прибирання. Ти вже в групі? Напиши боту прибирання !mygroups. Якщо твій обліковий запис Matrix не збігається з обліковим записом будинку, адміни можуть додати його в каталог.",
    ),
    (
        "me.none",
        "In den nächsten {} Wochen hast du keinen Termin.",
        "You have no turn in the next {} weeks.",
        "У найближчі {} тиж. у тебе немає чергувань.",
    ),
    ("me.login", "Angemeldet als {}.", "Logged in as {}.", "Ти увійшов(-ла) як {}."),
    (
        "me.guest",
        "Im Hausnetz siehst du den Plan ohne Anmeldung, aber wir wissen nicht, wer du bist. Melde dich mit deinem Hauskonto an, dann stehen deine Termine hier, auch beim nächsten Mal. Ohne Konto schickt sie dir der Putzbot auf !next.",
        "In the house you see the plan without logging in, but we don't know who you are. Log in with your house account and your turns show here, next time too. Without an account, the cleaning bot sends them to you on !next.",
        "У мережі будинку ти бачиш графік без входу, але ми не знаємо, хто ти. Увійди з обліковим записом будинку, і твої чергування будуть тут, і наступного разу теж. Без облікового запису їх надішле бот прибирання на !next.",
    ),
    ("me.signin", "Anmelden", "Log in", "Увійти"),
    ("me.with", "mit {}", "with {}", "разом із {}"),
    (
        "me.calendar",
        "Deine Termine stehen in deinem eigenen Kalender in der Hauscloud (App „Kalender“, auf dem Handy DAVx⁵). Wo genau, sagt dir der Putzbot auf !ical.",
        "Your turns are in your own calendar in the house cloud (its Calendar app, or DAVx⁵ on your phone). Send !ical to the cleaning bot to see which.",
        "Твої чергування — у твоєму власному календарі в хмарі будинку (застосунок «Календар» або DAVx⁵ на телефоні). Який саме, підкаже бот прибирання на !ical.",
    ),
    (
        "me.change",
        "Tauschen oder Hilfe holen: !next oder 🆘 im Putzraum.",
        "To swap or ask for cover: !next or 🆘 in the cleaning room.",
        "Помінятися або попросити заміну: !next або 🆘 у кімнаті прибирання.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_once_every_language_filled_and_the_same_placeholders() {
        let mut seen = std::collections::HashSet::new();
        for (key, de, en, uk) in TEXTS {
            assert!(seen.insert(key), "{key} twice");
            for text in [de, en, uk] {
                assert!(!text.trim().is_empty(), "{key}");
                assert_eq!(
                    text.matches("{}").count(),
                    en.matches("{}").count(),
                    "{key}: {text}"
                );
            }
        }
    }

    /// Every `"group.key"` literal in the website is a text, and every text is used.
    #[test]
    fn every_key_used_exists_and_every_key_is_used() {
        let this = include_str!("i18n.rs");
        let code = &this[..this.find("pub const TEXTS").unwrap()];
        let source = format!(
            "{}{}{code}",
            include_str!("../web.rs"),
            include_str!("render.rs")
        );
        let groups: std::collections::HashSet<&str> = TEXTS
            .iter()
            .map(|(k, ..)| k.split('.').next().unwrap())
            .collect();
        let mut checked = 0;
        for (start, _) in source.match_indices('"') {
            let rest = &source[start + 1..];
            let len = rest
                .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || "._".contains(c)))
                .unwrap_or(rest.len());
            let (word, after) = rest.split_at(len);
            if !after.starts_with('"') {
                continue;
            }
            let group = word.split('.').next().unwrap();
            if word.contains('.') && groups.contains(group) {
                assert!(TEXTS.iter().any(|(k, ..)| *k == word), "no text for {word}");
                checked += 1;
            }
        }
        assert!(checked > 40, "{checked}");
        for (key, ..) in TEXTS {
            assert!(source.contains(&format!("\"{key}\"")), "{key} is not used");
        }
    }

    #[test]
    fn dates_rhythms_and_numbers_in_each_language() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        assert_eq!(short_date(Lang::De, d(10, 12)), "Mo 12. Okt");
        assert_eq!(short_date(Lang::En, d(10, 12)), "Mon 12 Oct");
        assert_eq!(short_date(Lang::Uk, d(10, 12)), "пн 12 жовт");
        assert_eq!(date_range(Lang::De, d(10, 12), d(10, 18)), "12. – 18. Okt");
        assert_eq!(date_range(Lang::En, d(9, 28), d(10, 4)), "28 Sep – 4 Oct");
        assert_eq!(date_range(Lang::En, d(9, 28), d(9, 28)), "28 Sep");
        assert_eq!(days(Lang::De, d(10, 12), d(10, 13)), "Mo–Di");
        assert_eq!(days(Lang::Uk, d(10, 15), d(10, 15)), "чт");
        assert_eq!(week_label(Lang::De, (2026, 43), 2026), "KW 43");
        assert_eq!(week_label(Lang::En, (2027, 1), 2026), "Week 1 · 2027");

        use Weekday::*;
        assert_eq!(rhythm(Lang::De, 1, &[(Mon, Sun)]), "wöchentlich");
        assert_eq!(rhythm(Lang::En, 2, &[(Mon, Sun)]), "every 2 weeks");
        assert_eq!(
            rhythm(Lang::De, 1, &[(Mon, Tue), (Thu, Fri)]),
            "2× pro Woche (Mo–Di, Do–Fr)"
        );
        assert_eq!(
            rhythm(Lang::En, 2, &[(Mon, Wed), (Thu, Sun)]),
            "every 2 weeks · 2× per week (Mon–Wed, Thu–Sun)"
        );

        assert_eq!(number(Lang::De, 4.5), "4,5");
        assert_eq!(number(Lang::En, 4.5), "4.5");
        assert_eq!(number(Lang::En, 3.0), "3");
    }

    #[test]
    fn how_far_off_a_turn_is() {
        let d = |day| NaiveDate::from_ymd_opt(2026, 10, day).unwrap();
        let today = d(20); // a Tuesday
        assert_eq!(relative(Lang::En, d(19), d(20), today), "today");
        assert_eq!(relative(Lang::De, d(19), d(25), today), "jetzt, bis So");
        assert_eq!(relative(Lang::En, d(21), d(21), today), "tomorrow");
        assert_eq!(relative(Lang::En, d(23), d(24), today), "in 3 days");
        assert_eq!(relative(Lang::De, d(23), d(24), today), "in 3 Tagen");
        assert_eq!(relative(Lang::Uk, d(23), d(24), today), "через 3 дні");
        let later = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        assert_eq!(relative(Lang::Uk, later, later, today), "через 21 день");
        let later = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        assert_eq!(relative(Lang::Uk, later, later, today), "через 12 днів");
        assert_eq!(relative(Lang::En, d(12), d(13), today), "");
    }
}
