//! The website's pages as HTML, pure functions over a `Plan`: every turn comes from the
//! schedule snapshot (`schedule::build_schedule_from`), the single source of every export.
//!
//! A turn is one group's shift of one week; its duties (one per slot) are grouped by place,
//! so two people cleaning one place together read as "Scharni: Alice & Bob". Every name and
//! text from the plan is escaped (`esc`). Status is always a word too, never only a colour;
//! the visitor's own duties carry a "you" label besides their highlight.

use chrono::{Datelike, NaiveDate, Weekday};

use super::i18n::{self, t, tf, Lang};
use super::{esc, Frame};
use crate::{
    domain::{CleaningGroup, GroupId, PersonId},
    schedule::{AssignmentInstance, ScheduleSnapshot},
};

/// What a page shows, taken under the state lock and rendered after it is released.
pub struct Plan {
    pub today: NaiveDate,
    /// ISO week of today.
    pub week: (i32, u32),
    /// From this week on.
    pub snapshot: ScheduleSnapshot,
    /// The active groups, in the plan's order.
    pub groups: Vec<GroupInfo>,
    /// Everyone active in an active group: (person, display name).
    pub people: Vec<(PersonId, String)>,
    /// The visitor, when the plan knows them.
    pub me: Option<PersonId>,
    /// Their name at the house's login (X-House-Name), for "logged in as".
    pub login_name: String,
    /// Not logged in (in the house, `[web] guests`): the plan, but nobody is "you".
    pub guest: bool,
}

/// A group as its card names it: the turns themselves come from the snapshot.
pub struct GroupInfo {
    pub id: GroupId,
    pub name: String,
    pub house_wide: bool,
    /// Its rhythm today (a change for a later week shows in that week's turns).
    pub every_weeks: u32,
    pub shifts: Vec<(Weekday, Weekday)>,
    /// All its rooms, of every slot.
    pub rooms: Vec<String>,
}

impl GroupInfo {
    pub fn of(group: &CleaningGroup, today: NaiveDate) -> GroupInfo {
        let rhythm = group.rhythm.at(today);
        let day = |d: u8| Weekday::try_from(d).unwrap_or(Weekday::Mon);
        let mut rooms = group.room_names.clone();
        for room in group.slots.iter().flat_map(|s| &s.room_names) {
            if !rooms.contains(room) {
                rooms.push(room.clone());
            }
        }
        GroupInfo {
            id: group.id.clone(),
            name: group.name.clone(),
            house_wide: group.house_wide,
            every_weeks: rhythm.every_weeks(),
            shifts: rhythm
                .shifts()
                .iter()
                .map(|s| (day(s.start), day(s.end)))
                .collect(),
            rooms,
        }
    }

    fn rhythm(&self, lang: Lang) -> String {
        i18n::rhythm(lang, self.every_weeks, &self.shifts)
    }

    /// "2× pro Woche (Mo–Di, Do–Fr) · 🚽 Toilet", "… · das ganze Haus".
    fn meta(&self, lang: Lang) -> String {
        let mut parts = vec![esc(&self.rhythm(lang))];
        if self.house_wide {
            parts.push(esc(t(lang, "group.house")));
        }
        if !self.rooms.is_empty() {
            parts.push(esc(&crate::view::rooms(&self.rooms)));
        }
        parts.join(" · ")
    }
}

impl Plan {
    fn is_me(&self, a: &AssignmentInstance) -> bool {
        self.me
            .as_ref()
            .is_some_and(|me| a.assignee.as_ref().is_some_and(|p| &p.id == me))
    }

    fn this_year(&self) -> i32 {
        self.week.0
    }

    fn week_label(&self, lang: Lang, week: (i32, u32)) -> String {
        i18n::week_label(lang, week, self.this_year())
    }

    /// The visitor's duties that aren't over yet, in order.
    fn my_upcoming(&self) -> Vec<&AssignmentInstance> {
        self.snapshot
            .assignments
            .iter()
            .filter(|a| self.is_me(a) && a.end >= self.today)
            .collect()
    }

    fn group(&self, id: &str) -> Option<&GroupInfo> {
        self.groups.iter().find(|g| g.id == id)
    }
}

// ── Turns and places ────────────────────────────────────────────────────────

/// One shift of one group in one week, with its duties (one per slot).
struct Turn<'a> {
    week: (i32, u32),
    start: NaiveDate,
    end: NaiveDate,
    /// The group's week is one shift: the dates alone say when.
    whole_week: bool,
    duties: Vec<&'a AssignmentInstance>,
}

/// `items` (in the snapshot's order) as turns.
fn turns<'a>(items: impl IntoIterator<Item = &'a AssignmentInstance>) -> Vec<Turn<'a>> {
    let mut out: Vec<(String, u8, Turn<'a>)> = Vec::new();
    for a in items {
        let week = (a.iso_year, a.iso_week);
        match out
            .iter_mut()
            .find(|(g, shift, t)| *g == a.group_id && *shift == a.shift && t.week == week)
        {
            Some((.., turn)) => turn.duties.push(a),
            None => out.push((
                a.group_id.clone(),
                a.shift,
                Turn {
                    week,
                    start: a.start,
                    end: a.end,
                    whole_week: a.shift_label.is_none(),
                    duties: vec![a],
                },
            )),
        }
    }
    out.into_iter().map(|(.., turn)| turn).collect()
}

impl Turn<'_> {
    /// The duties by place, in slot order: (place, duties); `None` for a group without slots.
    fn places(&self) -> Vec<(Option<&str>, Vec<&AssignmentInstance>)> {
        let mut out: Vec<(Option<&str>, Vec<&AssignmentInstance>)> = Vec::new();
        for a in &self.duties {
            let place = a.place.as_deref();
            match out.iter_mut().find(|(p, _)| *p == place) {
                Some((_, same)) => same.push(a),
                None => out.push((place, vec![a])),
            }
        }
        out
    }

    fn running(&self, today: NaiveDate) -> bool {
        self.start <= today && today <= self.end
    }

    fn house_wide(&self) -> bool {
        self.duties.iter().any(|a| a.house_wide)
    }

    fn group_name(&self) -> &str {
        &self.duties[0].group_name
    }
}

// ── Pieces ──────────────────────────────────────────────────────────────────

/// A turn's when: "Mo–Di 19. – 20. Okt", or the dates of a whole week.
fn when(lang: Lang, start: NaiveDate, end: NaiveDate, whole_week: bool) -> String {
    let dates = esc(&i18n::date_range(lang, start, end));
    if whole_week {
        format!("<span class=\"dates\">{dates}</span>")
    } else {
        format!(
            "<strong class=\"days\">{}</strong> <span class=\"dates\">{dates}</span>",
            esc(&i18n::days(lang, start, end))
        )
    }
}

/// A duty's state as a badge with an icon and a word. `quiet`: nothing for a plain open
/// duty (the compact lists).
fn status(lang: Lang, a: &AssignmentInstance, today: NaiveDate, quiet: bool) -> String {
    let badge = |class: &str, icon: &str, text: &str| {
        format!(
            "<span class=\"h-badge st {class}\"><span aria-hidden=\"true\">{icon}</span> {}</span>",
            esc(text)
        )
    };
    if a.assignee.is_none() {
        return badge("warn", "—", t(lang, "status.nobody"));
    }
    let mut out = if a.is_skipped {
        badge("skipped", "⏭️", t(lang, "status.skipped"))
    } else if a.is_completed {
        let by = a
            .completed_by
            .as_deref()
            .filter(|by| *by != a.assignee_name())
            .map(|by| tf(lang, "status.done_by", &[by]))
            .unwrap_or_else(|| t(lang, "status.done").to_owned());
        let on = a
            .completed_at
            .map(|on| format!(" · {}", i18n::short_date(lang, on)))
            .unwrap_or_default();
        badge("ok", "✅", &format!("{by}{on}"))
    } else if a.end < today {
        badge("bad", "❌", t(lang, "status.missed"))
    } else if quiet {
        String::new()
    } else {
        badge("open", "⬜", t(lang, "status.open"))
    };
    if a.is_away && !a.is_completed {
        out.push_str(&badge("warn", "🌴", t(lang, "status.away")));
    }
    out
}

/// "du" next to the visitor's own name: their highlight is not only a colour.
fn you(lang: Lang) -> String {
    format!("<span class=\"you\">{}</span>", esc(t(lang, "turn.you")))
}

fn name(plan: &Plan, lang: Lang, a: &AssignmentInstance) -> String {
    match &a.assignee {
        Some(p) if plan.is_me(a) => format!(
            "<strong class=\"name\">{}</strong> {}",
            esc(&p.name),
            you(lang)
        ),
        Some(p) => format!("<span class=\"name\">{}</span>", esc(&p.name)),
        None => "<span class=\"name nobody\">—</span>".to_owned(),
    }
}

/// A place's people, each with their state: "Alice ✅ & Bob ⬜".
fn duties(plan: &Plan, lang: Lang, duties: &[&AssignmentInstance], quiet: bool) -> String {
    duties
        .iter()
        .map(|a| {
            let chip = status(lang, a, plan.today, quiet);
            format!(
                "<span class=\"duty{}\">{}{}{chip}</span>",
                if plan.is_me(a) { " mine" } else { "" },
                name(plan, lang, a),
                if chip.is_empty() { "" } else { " " },
            )
        })
        .collect::<Vec<_>>()
        .join("<span class=\"amp\"> &amp; </span>")
}

/// A turn in full: when, then each place with its people (side by side for the house-wide
/// group). `in_week`: under its week's label, where a whole week's dates say nothing new.
fn turn_item(plan: &Plan, lang: Lang, turn: &Turn, in_week: bool) -> String {
    let mine = turn.duties.iter().any(|a| plan.is_me(a));
    let running = turn.running(plan.today);
    let mut classes = vec!["turn"];
    if mine {
        classes.push("mine");
    }
    if running {
        classes.push("now");
    }
    let now = if running {
        format!(
            " <span class=\"h-badge now\">{}</span>",
            esc(t(lang, "turn.now"))
        )
    } else {
        String::new()
    };
    let places: String = turn
        .places()
        .iter()
        .map(|(place, list)| {
            let label = place
                .map(|p| format!("<span class=\"place-name\">{}</span>", esc(p)))
                .unwrap_or_default();
            format!(
                "<li class=\"place\">{label}<span class=\"people\">{}</span></li>",
                duties(plan, lang, list, false)
            )
        })
        .collect();
    let when = if in_week && turn.whole_week {
        if now.is_empty() {
            String::new()
        } else {
            format!("<div class=\"when\">{}</div>", now.trim_start())
        }
    } else {
        format!(
            "<div class=\"when\">{}{now}</div>",
            when(lang, turn.start, turn.end, turn.whole_week)
        )
    };
    format!(
        "<li class=\"{}\">{when}<ul class=\"places{}\">{places}</ul></li>",
        classes.join(" "),
        if turn.house_wide() { " side" } else { "" },
    )
}

/// A turn in one line: "Staircase · Scharni: Alice & Bob · Colbe: Carol & Dan".
fn compact_item(plan: &Plan, lang: Lang, turn: &Turn) -> String {
    let mine = turn.duties.iter().any(|a| plan.is_me(a));
    let places: Vec<String> = turn
        .places()
        .iter()
        .map(|(place, list)| {
            let label = place
                .map(|p| format!("<span class=\"place-name\">{}</span>: ", esc(p)))
                .unwrap_or_default();
            format!(
                "<span class=\"place\">{label}{}</span>",
                duties(plan, lang, list, true)
            )
        })
        .collect();
    let days = if turn.whole_week {
        String::new()
    } else {
        format!(
            " <span class=\"days\">{}</span>",
            esc(&i18n::days(lang, turn.start, turn.end))
        )
    };
    format!(
        "<li{}><span class=\"g\">{}</span>{days}<span class=\"sep\" aria-hidden=\"true\"> · </span>{}</li>",
        if mine { " class=\"mine\"" } else { "" },
        esc(turn.group_name()),
        places.join("<span class=\"sep\"> · </span>")
    )
}

/// One of the visitor's duties as a card: group, when and how far off, the place and who
/// cleans it with them, its state.
fn my_duty(plan: &Plan, f: &Frame, a: &AssignmentInstance) -> String {
    let lang = f.lang;
    let partners: Vec<&str> = plan
        .snapshot
        .assignments
        .iter()
        .filter(|b| {
            b.group_id == a.group_id
                && (b.iso_year, b.iso_week, b.shift) == (a.iso_year, a.iso_week, a.shift)
                && b.place == a.place
                && b.slot_index != a.slot_index
        })
        .filter_map(|b| b.assignee.as_ref().map(|p| p.name.as_str()))
        .collect();
    let mut place = Vec::new();
    if let Some(p) = &a.place {
        place.push(format!("<span class=\"place-name\">{}</span>", esc(p)));
    }
    if !partners.is_empty() {
        place.push(esc(&tf(lang, "me.with", &[&partners.join(" & ")])));
    }
    let place = if place.is_empty() {
        String::new()
    } else {
        format!("<p class=\"where\">{}</p>", place.join(" · "))
    };
    let relative = i18n::relative(lang, a.start, a.end, plan.today);
    format!(
        "<li class=\"h-card duty-card mine\"><p class=\"top\"><a class=\"g\" href=\"{href}\">{group}</a>\
         <span class=\"rel\">{relative}</span></p>\
         <p class=\"when\"><span class=\"wk\">{week}</span> · {when}</p>{place}<p class=\"state\">{state}</p></li>",
        href = esc(&f.href(&format!("/g/{}", a.group_id), None)),
        group = esc(&a.group_name),
        relative = esc(&relative),
        week = esc(&plan.week_label(lang, (a.iso_year, a.iso_week))),
        when = when(lang, a.start, a.end, a.shift_label.is_none()),
        state = status(lang, a, plan.today, false),
    )
}

fn weeks_from(first: (i32, u32), n: usize) -> impl Iterator<Item = (i32, u32)> {
    (0..n as i64).map(move |i| crate::state::add_weeks(first.0, first.1, i))
}

fn week_dates(lang: Lang, (year, week): (i32, u32)) -> String {
    let monday = crate::rhythm::week_monday(year, week);
    i18n::date_range(lang, monday, monday + chrono::Duration::days(6))
}

// ── Pages ───────────────────────────────────────────────────────────────────

/// `/`: this week (the visitor's next turns, every group's turns), then `ahead` more weeks.
pub fn week_page(f: &Frame, plan: &Plan, ahead: usize) -> String {
    let lang = f.lang;
    let monday = crate::rhythm::week_monday(plan.week.0, plan.week.1);
    let mut out = format!(
        "<section class=\"hero h-card\" aria-labelledby=\"hero-title\"><p class=\"eyebrow\">{}</p>\
         <h1 id=\"hero-title\">{}</h1><p class=\"dates\">{} {}</p><p class=\"today\">{}</p></section>",
        esc(t(lang, "week.this")),
        esc(&plan.week_label(lang, plan.week)),
        esc(&week_dates(lang, plan.week)),
        (monday + chrono::Duration::days(6)).year(),
        esc(&tf(lang, "week.today", &[&i18n::short_date(lang, plan.today)])),
    );

    if plan.me.is_some() {
        let mine = plan.my_upcoming();
        out.push_str(&format!(
            "<section class=\"yours\" aria-labelledby=\"yours-title\"><h2 id=\"yours-title\">{}</h2>",
            esc(t(lang, "yours.title"))
        ));
        if mine.is_empty() {
            out.push_str(&format!(
                "<p class=\"muted\">{}</p>",
                esc(&tf(lang, "yours.none", &[&(ahead + 1).to_string()]))
            ));
        } else {
            out.push_str("<ol class=\"strip\">");
            for a in mine.iter().take(3) {
                out.push_str(&my_duty(plan, f, a));
            }
            out.push_str("</ol>");
            out.push_str(&format!(
                "<p class=\"more\"><a href=\"{}\">{} →</a></p>",
                esc(&f.href("/me", None)),
                esc(t(lang, "yours.all"))
            ));
        }
        out.push_str("</section>");
    }

    out.push_str(&format!(
        "<h2 class=\"section\">{}</h2><div class=\"cards\">",
        esc(t(lang, "week.groups"))
    ));
    for group in &plan.groups {
        out.push_str(&group_card(f, plan, group, ahead + 1));
    }
    out.push_str("</div>");

    if ahead > 0 {
        out.push_str(&format!(
            "<h2 class=\"section\">{}</h2><div class=\"weeks\">",
            esc(t(lang, "week.ahead"))
        ));
        for week in weeks_from(plan.week, ahead + 1).skip(1) {
            out.push_str(&compact_week(plan, lang, week));
        }
        out.push_str("</div>");
    }
    let more = [12, 26].into_iter().find(|n| *n > ahead);
    if let Some(n) = more {
        out.push_str(&format!(
            "<p class=\"more\"><a href=\"{}\">{}</a></p>",
            esc(&f.href("/", Some(n))),
            esc(&tf(lang, "week.more", &[&n.to_string()]))
        ));
    }
    out
}

/// A group's card for this week: its turns, or when it is next due.
fn group_card(f: &Frame, plan: &Plan, group: &GroupInfo, window: usize) -> String {
    let lang = f.lang;
    let of_group = || {
        plan.snapshot
            .assignments
            .iter()
            .filter(move |a| a.group_id == group.id)
    };
    let this_week = turns(of_group().filter(|a| (a.iso_year, a.iso_week) == plan.week));
    let mine = this_week
        .iter()
        .any(|turn| turn.duties.iter().any(|a| plan.is_me(a)));
    let body = if this_week.is_empty() {
        let next = of_group().find(|a| (a.iso_year, a.iso_week) > plan.week);
        let text = match next {
            Some(a) => {
                let week = (a.iso_year, a.iso_week);
                tf(
                    lang,
                    "group.not_due",
                    &[&format!(
                        "{} · {}",
                        plan.week_label(lang, week),
                        week_dates(lang, week)
                    )],
                )
            }
            None => tf(lang, "group.none_ahead", &[&window.to_string()]),
        };
        format!("<p class=\"muted not-due\">{}</p>", esc(&text))
    } else {
        let items: String = this_week
            .iter()
            .map(|turn| turn_item(plan, lang, turn, false))
            .collect();
        format!("<ul class=\"turns\">{items}</ul>")
    };
    format!(
        "<article class=\"h-card group{}\" aria-labelledby=\"c-{id}\"><h3 id=\"c-{id}\"><a href=\"{href}\">{name}</a></h3>\
         <p class=\"meta\">{meta}</p>{body}</article>",
        if mine { " mine" } else { "" },
        id = esc(&group.id),
        href = esc(&f.href(&format!("/g/{}", group.id), None)),
        name = esc(&group.name),
        meta = group.meta(lang),
    )
}

/// A later week in short: one line per turn.
fn compact_week(plan: &Plan, lang: Lang, week: (i32, u32)) -> String {
    let list = turns(
        plan.snapshot
            .assignments
            .iter()
            .filter(|a| (a.iso_year, a.iso_week) == week),
    );
    let items: String = if list.is_empty() {
        format!("<li class=\"muted\">{}</li>", esc(t(lang, "week.empty")))
    } else {
        list.iter()
            .map(|turn| compact_item(plan, lang, turn))
            .collect()
    };
    format!(
        "<section class=\"week-compact\" id=\"w-{}-{}\"><h3>{} <span class=\"dates\">{}</span></h3><ul class=\"compact\">{items}</ul></section>",
        week.0,
        week.1,
        esc(&plan.week_label(lang, week)),
        esc(&week_dates(lang, week)),
    )
}

/// `/groups` and `/g/<id>` (`only`): each group's due weeks of the next `weeks`. `None` for
/// a group that isn't (or no longer is) active.
pub fn groups_page(f: &Frame, plan: &Plan, weeks: usize, only: Option<&str>) -> Option<String> {
    let lang = f.lang;
    let groups: Vec<&GroupInfo> = match only {
        Some(id) => vec![plan.group(id)?],
        None => plan.groups.iter().collect(),
    };
    let mut out = match only {
        Some(_) => format!(
            "<p class=\"crumb\"><a href=\"{}\">← {}</a></p>",
            esc(&f.href("/groups", None)),
            esc(t(lang, "groups.all"))
        ),
        None => format!("<h1>{}</h1>", esc(t(lang, "groups.title"))),
    };
    out.push_str(&format!(
        "<p class=\"muted\">{}</p><div class=\"cards timelines\">",
        esc(&tf(lang, "group.timeline", &[&weeks.to_string()]))
    ));
    for group in groups {
        let heading = if only.is_some() {
            format!(
                "<h1 id=\"g-{0}-title\">{1}</h1>",
                esc(&group.id),
                esc(&group.name)
            )
        } else {
            format!(
                "<h2 id=\"g-{0}-title\"><a href=\"{1}\">{2}</a></h2>",
                esc(&group.id),
                esc(&f.href(&format!("/g/{}", group.id), None)),
                esc(&group.name)
            )
        };
        out.push_str(&format!(
            "<section class=\"h-card timeline{}\" id=\"g-{}\" aria-labelledby=\"g-{1}-title\">{heading}<p class=\"meta\">{}</p>",
            if group.house_wide { " house" } else { "" },
            esc(&group.id),
            group.meta(lang)
        ));
        let mut any = false;
        out.push_str("<ol class=\"weeks\">");
        for week in weeks_from(plan.week, weeks) {
            let list = turns(
                plan.snapshot
                    .assignments
                    .iter()
                    .filter(|a| a.group_id == group.id && (a.iso_year, a.iso_week) == week),
            );
            if list.is_empty() {
                continue;
            }
            any = true;
            let now = week == plan.week;
            let items: String = list
                .iter()
                .map(|turn| turn_item(plan, lang, turn, true))
                .collect();
            out.push_str(&format!(
                "<li class=\"week{}\"{}><div class=\"wk\"><strong>{}</strong> <span class=\"dates\">{}</span>{}</div><ul class=\"turns\">{items}</ul></li>",
                if now { " now" } else { "" },
                if now { " aria-current=\"date\"" } else { "" },
                esc(&plan.week_label(lang, week)),
                esc(&week_dates(lang, week)),
                if now {
                    format!(" <span class=\"h-badge\">{}</span>", esc(t(lang, "week.now")))
                } else {
                    String::new()
                },
            ));
        }
        out.push_str("</ol>");
        if !any {
            out.push_str(&format!(
                "<p class=\"muted\">{}</p>",
                esc(&tf(lang, "group.none_ahead", &[&weeks.to_string()]))
            ));
        }
        out.push_str("</section>");
    }
    out.push_str("</div>");
    Some(out)
}

/// `/people`: everyone in the plan by name, with their turns of the next `weeks` and what
/// they add up to.
pub fn people_page(f: &Frame, plan: &Plan, weeks: usize) -> String {
    let lang = f.lang;
    let mut people: Vec<(&str, &str)> = plan
        .people
        .iter()
        .map(|(id, name)| (id.as_str(), name.as_str()))
        .collect();
    for p in plan
        .snapshot
        .assignments
        .iter()
        .filter_map(|a| a.assignee.as_ref())
    {
        if !people.iter().any(|(id, _)| *id == p.id) {
            people.push((&p.id, &p.name));
        }
    }
    people.sort_by(|a, b| {
        a.1.to_lowercase()
            .cmp(&b.1.to_lowercase())
            .then(a.0.cmp(b.0))
    });
    let mut out = format!(
        "<h1>{}</h1><p class=\"muted\">{}</p>",
        esc(t(lang, "people.title")),
        esc(&tf(lang, "people.intro", &[&weeks.to_string()]))
    );
    if people.is_empty() {
        out.push_str(&format!(
            "<p class=\"empty\">{}</p>",
            esc(t(lang, "people.empty"))
        ));
        return out;
    }
    out.push_str("<ul class=\"cards people\">");
    for (id, person) in people {
        let duties: Vec<&AssignmentInstance> = plan
            .snapshot
            .assignments
            .iter()
            .filter(|a| a.assignee.as_ref().is_some_and(|p| p.id == id))
            .collect();
        let load: f64 = duties.iter().map(|a| a.load).sum();
        let me = plan.me.as_deref() == Some(id);
        let list: String = if duties.is_empty() {
            format!("<li class=\"muted\">{}</li>", esc(t(lang, "people.none")))
        } else {
            duties
                .iter()
                .map(|a| {
                    let place = a
                        .place
                        .as_deref()
                        .map(|p| format!(" <span class=\"place-name\">{}</span>", esc(p)))
                        .unwrap_or_default();
                    let chip = status(lang, a, plan.today, true);
                    format!(
                        "<li><span class=\"wk\">{}</span> <span class=\"g\">{}</span>{place} <span class=\"when\">{}</span>{}{chip}</li>",
                        esc(&plan.week_label(lang, (a.iso_year, a.iso_week))),
                        esc(&a.group_name),
                        when(lang, a.start, a.end, a.shift_label.is_none()),
                        if chip.is_empty() { "" } else { " " },
                    )
                })
                .collect()
        };
        out.push_str(&format!(
            "<li class=\"h-card person{}\"><h2>{}{}</h2>\
             <dl class=\"figures\"><div><dt>{}</dt><dd>{}</dd></div><div><dt>{}</dt><dd>{}</dd></div></dl>\
             <ul class=\"compact\">{list}</ul></li>",
            if me { " mine" } else { "" },
            esc(person),
            if me { format!(" {}", you(lang)) } else { String::new() },
            esc(t(lang, "people.turns")),
            duties.len(),
            esc(t(lang, "people.load")),
            esc(&i18n::number(lang, load)),
        ));
    }
    out.push_str("</ul>");
    out
}

/// `/me`: the visitor's turns of the next `weeks`, and where their calendar is.
pub fn me_page(f: &Frame, plan: &Plan, weeks: usize) -> String {
    let lang = f.lang;
    let mut out = format!("<h1>{}</h1>", esc(t(lang, "me.title")));
    if plan.guest {
        out.push_str(&format!(
            "<div class=\"h-note\" role=\"note\"><p>{}</p></div>",
            esc(t(lang, "me.guest"))
        ));
    } else if plan.me.is_none() {
        out.push_str(&format!(
            "<div class=\"h-note warn\" role=\"note\"><p>{}</p><p>{}</p></div>",
            esc(&tf(lang, "me.login", &[&plan.login_name])),
            esc(t(lang, "me.unknown"))
        ));
    } else {
        let mine = plan.my_upcoming();
        if mine.is_empty() {
            out.push_str(&format!(
                "<p class=\"empty\">{}</p>",
                esc(&tf(lang, "me.none", &[&weeks.to_string()]))
            ));
        } else {
            out.push_str("<ol class=\"strip all\">");
            for a in mine {
                out.push_str(&my_duty(plan, f, a));
            }
            out.push_str("</ol>");
        }
    }
    out.push_str(&format!(
        "<div class=\"h-note\" role=\"note\"><p>🗓 {}</p><p>{}</p></div>",
        esc(t(lang, "me.calendar")),
        esc(t(lang, "me.change"))
    ));
    out
}
