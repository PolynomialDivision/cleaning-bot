//! LaTeX schedule renderer.
//!
//! `render_tex` is a pure function over a `ScheduleSnapshot`.
//!
//! Layout: one section per cleaning group, separated by `\clearpage`.
//! Uses `longtable` so tables that exceed one A4 page break automatically
//! with a repeated column header on every continuation page.
//! Font size scales down automatically when a group has many rows.
//!
//! Determinism guarantee: identical snapshot → identical .tex bytes.

use crate::schedule::ScheduleSnapshot;

// ── LaTeX preamble ────────────────────────────────────────────────────────────

const PREAMBLE: &str = r#"\documentclass[a4paper]{article}
\usepackage[a4paper, top=14mm, bottom=14mm, left=14mm, right=14mm]{geometry}
\usepackage{longtable}
\usepackage{array}
\usepackage{multirow}
\usepackage[table]{xcolor}
\usepackage{arydshln}
\usepackage{tikz}
\usepackage{graphicx}
\usepackage{fontawesome5}
\usepackage[T1]{fontenc}
\usepackage[utf8]{inputenc}
\usepackage{lmodern}
\usepackage{helvet}
\renewcommand{\familydefault}{\sfdefault}
\usepackage{microtype}
\usepackage{amssymb}
\pagenumbering{gobble}
\setlength{\parindent}{0pt}
\setlength{\tabcolsep}{4.5pt}
\renewcommand{\arraystretch}{1.6}
\setlength{\LTpre}{0pt}
\setlength{\LTpost}{0pt}
\setlength{\arrayrulewidth}{0.4pt}
\definecolor{accent}{HTML}{2F6F73}
\definecolor{ink}{HTML}{1F2933}
\definecolor{muted}{HTML}{6B7785}
\definecolor{weekrule}{HTML}{7D8B96}
\arrayrulecolor{black}
% Horizontal rules take no height of their own: then nothing between two rows
% of a week is a place to break the page (see `group_section`).
\ADLnullwidehline
\setlength{\dashlinedash}{3pt}
\setlength{\dashlinegap}{2pt}
\color{ink}
% The week number in a ring; small enough not to make its row taller.
\newcommand{\weekno}[1]{\tikz[baseline=(n.base)]\node[circle, draw=accent,
  line width=0.7pt, inner sep=0pt, minimum size=5.6mm, font=\small\bfseries] (n) {#1};}
% An empty box to tick by hand on the printed plan.
\newcommand{\tickbox}{{\color{weekrule}\faSquare[regular]}}
"#;

// ── Public entry point ────────────────────────────────────────────────────────

pub fn render_tex(snapshot: &ScheduleSnapshot) -> String {
    let generated = snapshot.state_timestamp.format("%-d %b %Y").to_string();

    // Groups in first-appearance order.
    let mut group_order: Vec<(String, String)> = Vec::new();
    for a in &snapshot.assignments {
        if !group_order.iter().any(|(id, _)| id == &a.group_id) {
            group_order.push((a.group_id.clone(), a.group_name.clone()));
        }
    }

    let mut body = String::new();
    for (gi, (group_id, group_name)) in group_order.iter().enumerate() {
        let rows: Vec<_> = snapshot
            .assignments
            .iter()
            .filter(|a| &a.group_id == group_id)
            .collect();

        if rows.is_empty() {
            continue;
        }

        let date_range = match (rows.first(), rows.last()) {
            (Some(f), Some(l)) if f.start != l.end => format!(
                "{} -- {}",
                f.start.format("%-d %b %Y"),
                l.end.format("%-d %b %Y")
            ),
            (Some(f), _) => f.period_label.clone(),
            _ => String::new(),
        };

        if gi > 0 {
            body.push_str("\n\\clearpage\n\n");
        }
        body.push_str(&group_section(
            group_name,
            &date_range,
            &rows[0].rhythm,
            &generated,
            &rows,
        ));
    }

    format!("{PREAMBLE}\n\n\\begin{{document}}\n{body}\n\\end{{document}}\n")
}

// ── Per-group section ─────────────────────────────────────────────────────────

/// The heavy outer frame of the table (also between weeks, as a rule).
const FRAME: &str = "!{\\color{black}\\vrule width 1.2pt}";
/// The fine line between two columns.
const COLUMN_RULE: &str = "!{\\color{black!45}\\vrule width 0.4pt}";
/// A heavy black rule across the table: around the header, between weeks.
const HEAVY_RULE: &str =
    "\\noalign{\\global\\arrayrulewidth=1.2pt}\\hline\n\\noalign{\\global\\arrayrulewidth=0.5pt}\n";

// Made to be printed — in black and white, too: no dark fills, and nothing
// told by colour alone (done is a tick, skipped a dash).
//
// Printed in black and white, the structure has to come from the lines: a
// heavy frame, heavy rules around the header and between weeks, dashed
// rules between the shifts of a week and dotted ones between the slots of a
// shift, fine lines between columns. Colour is only a bonus on screens —
// and rows are never filled: cell colour would paint over the dashed lines. The week number and the dates are centred over their rows
// (`\multirow` with a negative count, placed in the block's last row so the
// shading of later rows can't paint over it), so every row has the same
// height. Rows inside a week end in `\\*`: a week never breaks across pages.
// An open turn has an empty box to tick, done ones a tick and the date.

fn group_section(
    group_name: &str,
    date_range: &str,
    rhythm: &str,
    generated: &str,
    rows: &[&crate::schedule::AssignmentInstance],
) -> String {
    let (fsize, fskip) = font_size_for_rows(rows.len());
    let with_slot = rows.iter().any(|a| a.slot_name.is_some());
    let cols = if with_slot { 5 } else { 4 };
    let mut s = String::new();

    // Set section font size without a grouping wrapper (longtable cannot be
    // inside a TeX group).  The change persists until \clearpage or the next
    // \fontsize\selectfont, which is fine since each section is on its own page.
    s.push_str(&format!("\\fontsize{{{fsize}}}{{{fskip}}}\\selectfont\n"));

    // Title, subtitle, and the rooms with their icons.
    s.push_str(&format!(
        "{{\\fontsize{{20}}{{24}}\\selectfont\\bfseries\\color{{accent}} \\faBroom\\enspace {}}}\\par\n\\vspace{{1.5mm}}\n",
        tex_esc(group_name),
    ));
    s.push_str(&format!(
        "{{\\small\\color{{muted}} {} \\enspace$\\cdot$\\enspace {}}}\\par\n",
        tex_esc(date_range),
        tex_esc(&capitalized(rhythm)),
    ));
    // A slot's rooms are the same every week: listed once, up here, so each
    // row stays one line.
    let mut room_lines: Vec<String> = Vec::new();
    for a in rows {
        if a.room_names.is_empty() {
            continue;
        }
        let rooms = rooms_with_icons(&a.room_names);
        let line = match &a.slot_name {
            Some(slot) => format!("\\textbf{{{}}}\\enspace {rooms}", tex_esc(slot)),
            None => rooms,
        };
        if !room_lines.contains(&line) {
            room_lines.push(line);
        }
    }
    if !room_lines.is_empty() {
        s.push_str(&format!(
            "\\vspace{{1mm}}{{\\small {}}}\\par\n",
            room_lines.join("\\qquad "),
        ));
    }
    s.push_str("\\vspace{4mm}\n");

    // Column widths (A4 182 mm text width, 4.5 pt padding each side), in a
    // heavy frame with fine lines between the columns.
    let mut widths = vec![("c", 12), ("l", 41)];
    if with_slot {
        widths.extend([("l", 25), ("l", 63)]);
    } else {
        widths.push(("l", 91));
    }
    widths.push(("l", 20));
    let columns: Vec<String> = widths
        .iter()
        .map(|(align, mm)| {
            let align = if *align == "c" {
                "centering"
            } else {
                "raggedright"
            };
            format!(">{{\\{align}\\arraybackslash}}m{{{mm}mm}}")
        })
        .collect();
    let spec = format!("{FRAME}{}{FRAME}", columns.join(COLUMN_RULE));
    s.push_str(&format!("\\begin{{longtable}}{{{spec}}}\n"));

    // The header, between two heavy rules.
    let header = {
        let mut cells = vec!["Week", "Dates"];
        if with_slot {
            cells.push("Slot");
        }
        cells.extend(["Responsible", "Done on"]);
        let cells: Vec<String> = cells
            .iter()
            .map(|c| format!("\\textcolor{{accent}}{{\\bfseries {c}}}"))
            .collect();
        format!("{HEAVY_RULE}{} \\\\\n{HEAVY_RULE}", cells.join(" & "))
    };
    s.push_str(&header);
    s.push_str("\\endfirsthead\n");
    s.push_str(&format!(
        "\\multicolumn{{{cols}}}{{l}}{{\\small\\color{{muted}} {} (continued)}} \\\\[1mm]\n",
        tex_esc(group_name),
    ));
    s.push_str(&header);
    s.push_str("\\endhead\n");
    // A page that ends mid-table closes its frame; the last week closes
    // the table's.
    s.push_str(&format!("{HEAVY_RULE}\\endfoot\n\\endlastfoot\n"));

    // ── One block per week ────────────────────────────────────────────────────
    let mut i = 0;
    let mut year = rows.first().map(|a| a.iso_year);
    while i < rows.len() {
        let key = (rows[i].iso_year, rows[i].iso_week);
        let mut j = i + 1;
        while j < rows.len() && (rows[j].iso_year, rows[j].iso_week) == key {
            j += 1;
        }
        let week = &rows[i..j];

        // A new year gets its own small heading row.
        if year != Some(key.0) {
            year = Some(key.0);
            s.push_str(&format!(
                "\\multicolumn{{{cols}}}{{{FRAME}l{FRAME}}}{{\\bfseries\\color{{accent}} {}}} \\\\\n{HEAVY_RULE}",
                key.0
            ));
        }

        for (k, &a) in week.iter().enumerate() {
            let last_of_week = k + 1 == week.len();
            let last_of_shift = last_of_week || week[k + 1].shift != a.shift;
            let shift_rows = week[..=k]
                .iter()
                .rev()
                .take_while(|r| r.shift == a.shift)
                .count();

            // Week number, centred over the whole week.
            if last_of_week {
                s.push_str(&merged(
                    week.len(),
                    "*",
                    &format!("\\weekno{{{}}}", a.iso_week),
                ));
            }
            s.push_str(" & ");
            // Dates, centred over the shift's rows.
            if last_of_shift {
                s.push_str(&merged(shift_rows, "=", &tex_esc(&a.period_label)));
            }
            s.push_str(" & ");
            if with_slot {
                if let Some(slot) = &a.slot_name {
                    s.push_str(&tex_esc(slot));
                }
                s.push_str(" & ");
            }
            // Who, and anything worth knowing about how.
            s.push_str(&tex_esc(a.assignee_name()));
            let mut notes = Vec::new();
            if let Some(by) = a
                .completed_by
                .as_ref()
                .filter(|by| !a.is_skipped && by.as_str() != a.assignee_name())
            {
                notes.push(format!("done by {by}"));
            }
            notes.extend(crate::view::source_note(&a.source).map(str::to_owned));
            if !notes.is_empty() {
                s.push_str(&format!(
                    " {{\\scriptsize\\color{{muted}} $\\cdot$ {}}}",
                    tex_esc(&notes.join(" · "))
                ));
            }
            s.push_str(" & ");
            // Done on: the date with a tick, "skipped", or a box to tick.
            if a.is_skipped {
                s.push_str("{\\small\\color{muted}\\itshape -- skipped}");
            } else if let Some(date) = a.completed_at {
                s.push_str(&format!(
                    "{{\\color{{accent}}\\faCheck}}\\enspace{{\\small {}}}",
                    tex_esc(&date.format("%-d %b").to_string())
                ));
            } else if a.is_completed {
                s.push_str("{\\color{accent}\\faCheck}");
            } else {
                s.push_str("\\tickbox");
            }

            if last_of_week {
                s.push_str(&format!(" \\\\\n{HEAVY_RULE}"));
            } else {
                // Inside a week: dashed between its shifts, dotted between the
                // slots of a shift — under the cells that change only, so the
                // merged week (and shift) cells stay one box.
                let (from, pattern) = if last_of_shift {
                    (2, "[3pt/2pt]")
                } else {
                    (3, "[0.8pt/1.6pt]")
                };
                // No page break inside a week — after the row, nor after its
                // line (itself a row of its own).
                s.push_str(&format!(
                    " \\\\*\n\\cdashline{{{from}-{cols}}}{pattern}\n\\noalign{{\\penalty10000}}\n"
                ));
            }
        }
        i = j;
    }

    s.push_str("\\end{longtable}\n\n");

    // How to use it, a thank-you, and when it was made.
    s.push_str(&format!(
        "\\vspace{{2.5mm}}{{\\scriptsize\\color{{muted}} \\tickbox\\ tick it when it's done \
         \\enspace {{\\color{{accent}}\\faCheck}}\\ done \\enspace -- skipped \
         \\hfill Thanks for keeping the house lovely \\faHeart[regular] \\enspace$\\cdot$\\enspace Generated {}}}\n",
        tex_esc(generated)
    ));

    s
}

/// "\faToilet Scharni Toilet \faShower Shower Room" — each room with the
/// icon of its kind, as in the chat.
fn rooms_with_icons(rooms: &[String]) -> String {
    use crate::view::RoomKind;
    rooms
        .iter()
        .map(|room| {
            let icon = match crate::view::room_kind(room) {
                RoomKind::Toilet => "\\faToilet",
                RoomKind::Shower => "\\faShower",
                RoomKind::Kitchen => "\\faUtensils",
                RoomKind::Other => "\\faBroom",
            };
            format!(
                "{{\\color{{accent}}{icon}}}\\hspace{{0.3em}}{}",
                tex_esc(room)
            )
        })
        .collect::<Vec<_>>()
        .join("\\quad ")
}

/// `content` centred over the `rows` rows ending here (a negative
/// `\multirow`, see above) — or just the content in a row of its own,
/// which `\multirow` would set a little off the line.
fn merged(rows: usize, width: &str, content: &str) -> String {
    if rows > 1 {
        format!("\\multirow{{-{rows}}}{{{width}}}{{{content}}}")
    } else {
        content.to_owned()
    }
}

/// "weekly" → "Weekly".
fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// The table's font size for a group of `n` rows.
///
/// Returns (font_size_pt, baseline_skip_pt).
fn font_size_for_rows(n: usize) -> (&'static str, &'static str) {
    // Long plans go onto more pages rather than into ever smaller print.
    match n {
        0..=30 => ("10", "13"),
        _ => ("9", "11"),
    }
}

/// Escape a string for safe use in LaTeX body text.
fn tex_esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str(r"\&"),
            '%' => out.push_str(r"\%"),
            '$' => out.push_str(r"\$"),
            '#' => out.push_str(r"\#"),
            '_' => out.push_str(r"\_"),
            '{' => out.push_str(r"\{"),
            '}' => out.push_str(r"\}"),
            '~' => out.push_str(r"\textasciitilde{}"),
            '^' => out.push_str(r"\textasciicircum{}"),
            '\\' => out.push_str(r"\textbackslash{}"),
            // Unicode dashes: map to LaTeX ligatures (pdfLaTeX drops raw U+2013/U+2014).
            '\u{2013}' => out.push_str("--"),  // en dash
            '\u{2014}' => out.push_str("---"), // em dash
            // The T1 font encoding agrees with Latin-1 on accented letters,
            // but not on these: as raw characters, XeTeX (tectonic) prints
            // the T1 glyph in that slot — "2×" came out as "2Œ", "ß" as "SS".
            // Anything else outside Latin-1 has no glyph in these fonts.
            '×' => out.push_str(r"$\times$"),
            '÷' => out.push_str(r"$\div$"),
            'ß' => out.push_str(r"{\ss}"),
            'ÿ' => out.push_str("\\\"y"),
            '\u{a0}' => out.push('~'),
            '¡' => out.push_str(r"\textexclamdown{}"),
            '£' => out.push_str(r"\pounds{}"),
            '§' => out.push_str(r"\S{}"),
            '©' => out.push_str(r"\textcopyright{}"),
            '«' => out.push_str(r"\guillemotleft{}"),
            '»' => out.push_str(r"\guillemotright{}"),
            '°' => out.push_str(r"\textdegree{}"),
            '·' => out.push_str(r"\textperiodcentered{}"),
            '¿' => out.push_str(r"\textquestiondown{}"),
            '€' => out.push_str(r"\texteuro{}"),
            '…' => out.push_str(r"\dots{}"),
            '‘' => out.push('`'),
            '’' => out.push('\''),
            '‚' => out.push_str(r"\quotesinglbase{}"),
            '“' => out.push_str("``"),
            '”' => out.push_str("''"),
            '„' => out.push_str(r"\quotedblbase{}"),
            c => out.push(c),
        }
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CleaningGroup, Person},
        schedule::build_schedule,
        state::State,
    };
    use chrono::Utc;

    fn make_state() -> State {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        st.last_modified = st.created_at;
        let p = Person::new_matrix("@bob:example.org");
        let id = p.id.clone();
        st.persons.push(p);
        let mut g = CleaningGroup::new("Hallway");
        g.member_ids.push(id);
        st.cleaning_groups.push(g);
        st
    }

    #[test]
    fn characters_the_t1_fonts_put_elsewhere_are_spelled_out() {
        assert_eq!(tex_esc("2× per week"), r"2$\times$ per week");
        assert_eq!(tex_esc("Straße"), r"Stra{\ss}e");
        assert_eq!(
            tex_esc("„Küche“ · 5 €"),
            r"\quotedblbase{}Küche`` \textperiodcentered{} 5 \texteuro{}"
        );
        // Accented letters sit where Latin-1 has them, so they stay.
        assert_eq!(tex_esc("Zoë Ångström"), "Zoë Ångström");
    }

    #[test]
    fn tex_output_is_deterministic() {
        let st = make_state();
        let sn1 = build_schedule(&st, 4);
        let sn2 = build_schedule(&st, 4);
        assert_eq!(render_tex(&sn1), render_tex(&sn2));
    }

    #[test]
    fn tex_contains_group_name() {
        let st = make_state();
        let sn = build_schedule(&st, 2);
        let tex = render_tex(&sn);
        assert!(
            tex.contains("Hallway"),
            "group name must appear in .tex output"
        );
    }

    #[test]
    fn tex_column_headers_are_english() {
        let st = make_state();
        let sn = build_schedule(&st, 2);
        let tex = render_tex(&sn);
        assert!(
            tex.contains("Responsible"),
            "column headers must be in English"
        );
        assert!(tex.contains("Done on"));
        assert!(!tex.contains("Putzplan"));
    }

    #[test]
    fn each_group_gets_its_own_section() {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        for name in ["Floor A", "Floor B"] {
            let p = Person::new_named(name);
            let pid = p.id.clone();
            st.persons.push(p);
            let mut g = CleaningGroup::new(name);
            g.member_ids.push(pid);
            st.cleaning_groups.push(g);
        }
        let sn = build_schedule(&st, 2);
        let tex = render_tex(&sn);
        assert!(tex.contains("Floor A"));
        assert!(tex.contains("Floor B"));
        assert!(
            tex.contains(r"\clearpage"),
            "groups must be separated by \\clearpage"
        );
    }

    #[test]
    fn special_chars_are_escaped() {
        assert_eq!(tex_esc("a & b"), r"a \& b");
        assert_eq!(tex_esc("100%"), r"100\%");
        assert_eq!(tex_esc("$price"), r"\$price");
        assert_eq!(tex_esc("a_b"), r"a\_b");
        assert_eq!(tex_esc("a#b"), r"a\#b");
        assert_eq!(tex_esc("25 \u{2013} 31 May"), "25 -- 31 May");
        assert_eq!(tex_esc("Mon\u{2014}Fri"), "Mon---Fri");
    }

    #[test]
    fn a_week_is_one_block_with_its_number_centred_and_never_split() {
        let mut st = State::default();
        st.created_at = Some(Utc::now());
        let mut g = CleaningGroup::new("Bath");
        for name in ["Ann", "Ben", "Cat"] {
            let p = Person::new_named(name);
            g.member_ids.push(p.id.clone());
            st.persons.push(p);
        }
        g.rhythm.shift_starts = vec![0, 3];
        st.cleaning_groups.push(g);
        let tex = render_tex(&build_schedule(&st, 3));
        // Two shifts a week: the week number centred over both rows, each
        // shift's dates on its own row, no line break making a row taller.
        assert_eq!(
            tex.matches(r"\multirow{-2}{*}{\weekno{").count(),
            3,
            "{tex}"
        );
        assert!(!tex.contains(r"\newline"));
        // No page break between the shifts of a week; weeks shaded in turn.
        assert_eq!(tex.matches(r"\\*").count(), 3);
        assert_eq!(tex.matches(r"\noalign{\penalty10000}").count(), 3);
        // Heavy rules between weeks, dashed between shifts, no filled rows.
        assert!(tex.contains(r"\cdashline{2-4}[3pt/2pt]"), "{tex}");
        assert!(!tex.contains(r"\rowcolor"));
        // Open turns get a box to tick on paper; nothing is told by colour
        // alone, and the header is no dark bar.
        assert_eq!(tex.matches(r"\tickbox").count(), 6 + 2, "{tex}");
    }

    #[test]
    fn the_docker_warmup_uses_what_the_renderer_uses() {
        // The image renders offline from a cache filled by compiling
        // docker/tex-warmup.tex: it must load the same packages and use
        // every icon, or PDFs fail in production only.
        let warmup = include_str!("../docker/tex-warmup.tex");
        assert!(warmup.starts_with(PREAMBLE), "warm-up preamble differs");
        let source = include_str!("pdf.rs");
        for icon in source.split("\\\\fa").skip(1).map(|rest| {
            rest.split(|c: char| !c.is_ascii_alphabetic())
                .next()
                .unwrap()
        }) {
            assert!(
                warmup.contains(&format!("\\fa{icon}")),
                "warm-up lacks \\fa{icon}"
            );
        }
        for used in [r"\weekno{", r"\tickbox", r"\multirow", r"\cdashline"] {
            assert!(warmup.contains(used), "warm-up lacks {used}");
        }
    }

    #[test]
    fn uses_longtable_not_tabularx() {
        let st = make_state();
        let sn = build_schedule(&st, 2);
        let tex = render_tex(&sn);
        assert!(
            tex.contains("longtable"),
            "must use longtable for page-breaking"
        );
        assert!(
            !tex.contains("tabularx"),
            "tabularx cannot break across pages"
        );
    }
}
