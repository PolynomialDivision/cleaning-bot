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
\usepackage{hhline}
\usepackage[table]{xcolor}
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
\definecolor{shade}{HTML}{EEF3F5}
\definecolor{hair}{HTML}{D3DCE1}
\definecolor{weekrule}{HTML}{7D8B96}
\color{ink}
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

// Separators, from strong to faint: a darker rule between weeks (which also
// alternate white and shaded), a hairline between the shifts of a week, and
// one between the slots of a shift that leaves the shift's dates merged.
// The week number and the dates are centred over their rows (`\multirow`
// with a negative count, placed in the block's last row so the shading of
// later rows can't paint over it), so every row has the same height.
// Rows inside a week end in `\\*`: a week never breaks across pages.

fn group_section(
    group_name: &str,
    date_range: &str,
    rhythm: &str,
    generated: &str,
    rows: &[&crate::schedule::AssignmentInstance],
) -> String {
    let (fsize, fskip) = font_size_for_rows(rows.len());
    let with_area = rows.iter().any(|a| a.slot_name.is_some());
    let cols = if with_area { 6 } else { 5 };
    let mut s = String::new();

    // Set section font size without a grouping wrapper (longtable cannot be
    // inside a TeX group).  The change persists until \clearpage or the next
    // \fontsize\selectfont, which is fine since each section is on its own page.
    s.push_str(&format!("\\fontsize{{{fsize}}}{{{fskip}}}\\selectfont\n"));

    // Title and subtitle.
    s.push_str(&format!(
        "{{\\fontsize{{20}}{{24}}\\selectfont\\bfseries\\color{{accent}} {}}}\\par\n\\vspace{{1.5mm}}\n",
        tex_esc(group_name),
    ));
    let mut subtitle = vec![tex_esc(date_range), tex_esc(&capitalized(rhythm))];
    let rooms = rows
        .first()
        .map(|a| a.room_names.clone())
        .unwrap_or_default();
    if !with_area && !rooms.is_empty() {
        subtitle.push(tex_esc(&rooms.join(", ")));
    }
    s.push_str(&format!(
        "{{\\small\\color{{muted}} {}}}\\par\n",
        subtitle.join(" \\enspace$\\cdot$\\enspace "),
    ));
    // A slot's rooms are the same every week: listed once, up here, so each
    // row stays one line.
    let mut slot_rooms: Vec<String> = Vec::new();
    for a in rows {
        if let Some(slot) = &a.slot_name {
            let line = format!(
                "\\textbf{{{}}} {}",
                tex_esc(slot),
                tex_esc(&a.room_names.join(", "))
            );
            if !a.room_names.is_empty() && !slot_rooms.contains(&line) {
                slot_rooms.push(line);
            }
        }
    }
    if !slot_rooms.is_empty() {
        s.push_str(&format!(
            "\\vspace{{0.5mm}}{{\\small\\color{{muted}} {}}}\\par\n",
            slot_rooms.join(" \\enspace$\\cdot$\\enspace "),
        ));
    }
    s.push_str("\\vspace{4mm}\n");

    // Column widths (A4 182 mm text width, 4.5 pt padding each side).
    let mut spec = String::from(
        ">{\\centering\\arraybackslash}m{11mm}>{\\raggedright\\arraybackslash}m{42mm}",
    );
    if with_area {
        spec.push_str(
            ">{\\raggedright\\arraybackslash}m{26mm}>{\\raggedright\\arraybackslash}m{56mm}",
        );
    } else {
        spec.push_str(">{\\raggedright\\arraybackslash}m{85mm}");
    }
    spec.push_str(">{\\centering\\arraybackslash}m{6mm}>{\\raggedright\\arraybackslash}m{17mm}");
    s.push_str(&format!("\\begin{{longtable}}{{{spec}}}\n"));

    let header = {
        let mut cells = vec!["Week", "Dates"];
        if with_area {
            cells.push("Slot");
        }
        cells.extend(["Responsible", "$\\checkmark$", "Done on"]);
        let cells: Vec<String> = cells
            .iter()
            .map(|c| format!("\\textcolor{{white}}{{\\bfseries {c}}}"))
            .collect();
        format!("\\rowcolor{{accent}}\n{} \\\\\n", cells.join(" & "))
    };
    s.push_str(&header);
    s.push_str("\\endfirsthead\n");
    s.push_str(&format!(
        "\\multicolumn{{{cols}}}{{l}}{{\\small\\color{{muted}} {} (continued)}} \\\\[1mm]\n",
        tex_esc(group_name),
    ));
    s.push_str(&header);
    s.push_str("\\endhead\n");

    // ── One block per week ────────────────────────────────────────────────────
    let mut i = 0;
    let mut block = 0;
    let mut year = rows.first().map(|a| a.iso_year);
    while i < rows.len() {
        let key = (rows[i].iso_year, rows[i].iso_week);
        let mut j = i + 1;
        while j < rows.len() && (rows[j].iso_year, rows[j].iso_week) == key {
            j += 1;
        }
        let week = &rows[i..j];
        let shade = if block % 2 == 1 { "shade" } else { "white" };

        // A new year gets its own small heading row.
        if year != Some(key.0) {
            year = Some(key.0);
            s.push_str(&format!(
                "\\multicolumn{{{cols}}}{{l}}{{\\cellcolor{{white}}\\bfseries\\color{{accent}} {}}} \\\\\n",
                key.0
            ));
            s.push_str("\\arrayrulecolor{weekrule}\\hline\n");
        }

        for (k, &a) in week.iter().enumerate() {
            let last_of_week = k + 1 == week.len();
            let last_of_shift = last_of_week || week[k + 1].shift != a.shift;
            let shift_rows = week[..=k]
                .iter()
                .rev()
                .take_while(|r| r.shift == a.shift)
                .count();

            s.push_str(&format!("\\rowcolor{{{shade}}}\n"));
            // Week number, centred over the whole week.
            if last_of_week {
                s.push_str(&merged(
                    week.len(),
                    "*",
                    &format!("\\large\\bfseries {}", a.iso_week),
                ));
            }
            s.push_str(" & ");
            // Dates, centred over the shift's rows.
            if last_of_shift {
                s.push_str(&merged(shift_rows, "=", &tex_esc(&a.period_label)));
            }
            s.push_str(" & ");
            if with_area {
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
            if a.is_skipped {
                s.push_str("{\\color{muted}--}");
            } else if a.is_completed {
                s.push_str("{\\color{accent}$\\checkmark$}");
            }
            s.push_str(" & ");
            if a.is_skipped {
                s.push_str("{\\small\\color{muted}\\itshape skipped}");
            } else if let Some(date) = a.completed_at {
                s.push_str(&format!(
                    "{{\\small {}}}",
                    tex_esc(&date.format("%-d %b").to_string())
                ));
            }

            if last_of_week {
                s.push_str(" \\\\\n\\arrayrulecolor{weekrule}\\hline\n");
            } else {
                // A hairline under the cells that change; the merged
                // cells get one in their own shade, so it doesn't show.
                let merged = if last_of_shift { 1 } else { 2 };
                let hidden = format!(">{{\\arrayrulecolor{{{shade}}}}}-").repeat(merged);
                let visible = ">{\\arrayrulecolor{hair}}-".repeat(cols - merged);
                // No page break inside a week — after the row, nor after its
                // hairline (itself a row of its own).
                s.push_str(&format!(
                    " \\\\*\n\\hhline{{{hidden}{visible}}}\n\\noalign{{\\penalty10000}}\n"
                ));
            }
        }
        block += 1;
        i = j;
    }

    s.push_str("\\end{longtable}\n\n");

    // Legend.
    s.push_str(&format!(
        "\\vspace{{2mm}}{{\\scriptsize\\color{{muted}} $\\checkmark$ done \\enspace -- skipped \\hfill Generated {}}}\n",
        tex_esc(generated)
    ));

    s
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
            tex.matches(r"\multirow{-2}{*}{\large\bfseries").count(),
            3,
            "{tex}"
        );
        assert!(!tex.contains(r"\newline"));
        // No page break between the shifts of a week; weeks shaded in turn.
        assert_eq!(tex.matches(r"\\*").count(), 3);
        assert_eq!(tex.matches(r"\noalign{\penalty10000}").count(), 3);
        assert!(tex.contains(r"\rowcolor{shade}") && tex.contains(r"\rowcolor{white}"));
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
