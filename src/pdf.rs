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
\usepackage[a4paper, top=12mm, bottom=12mm, left=14mm, right=14mm]{geometry}
\usepackage{longtable}
\usepackage{array}
\usepackage[T1]{fontenc}
\usepackage[utf8]{inputenc}
\usepackage{lmodern}
\usepackage{helvet}
\renewcommand{\familydefault}{\sfdefault}
\usepackage{microtype}
\usepackage{amssymb}
\pagenumbering{gobble}
\setlength{\parindent}{0pt}
\setlength{\tabcolsep}{4pt}
\renewcommand{\arraystretch}{1.3}
% Remove longtable's default \bigskip spacing around the table.
\setlength{\LTpre}{2pt}
\setlength{\LTpost}{0pt}

"#;

// ── Public entry point ────────────────────────────────────────────────────────

pub fn render_tex(snapshot: &ScheduleSnapshot) -> String {
    let generated = snapshot.state_timestamp.format("%d %b %Y").to_string();

    // Groups in first-appearance order.
    let mut group_order: Vec<(String, String)> = Vec::new();
    for a in &snapshot.assignments {
        if !group_order.iter().any(|(id, _)| id == &a.group_id) {
            group_order.push((a.group_id.clone(), a.group_name.clone()));
        }
    }

    let mut body = String::new();
    for (gi, (group_id, group_name)) in group_order.iter().enumerate() {
        let rows: Vec<_> = snapshot.assignments.iter()
            .filter(|a| &a.group_id == group_id)
            .collect();

        if rows.is_empty() { continue; }

        let date_range = match (rows.first(), rows.last()) {
            (Some(f), Some(l)) if f.week_monday != l.week_sunday =>
                format!("{} -- {}",
                    f.week_monday.format("%d %b %Y"),
                    l.week_sunday.format("%d %b %Y")),
            (Some(f), _) => f.week_label.clone(),
            _ => String::new(),
        };

        if gi > 0 { body.push_str("\n\\clearpage\n\n"); }
        body.push_str(&group_section(
            group_name, &date_range, snapshot.interval_weeks, &generated, &rows,
        ));
    }

    format!("{PREAMBLE}\n\n\\begin{{document}}\n{body}\n\\end{{document}}\n")
}

// ── Per-group section ─────────────────────────────────────────────────────────

fn group_section(
    group_name: &str,
    date_range: &str,
    interval:   u32,
    generated:  &str,
    rows:       &[&crate::schedule::AssignmentInstance],
) -> String {
    let (fsize, fskip) = font_size_for_rows(rows.len());
    let mut s = String::new();

    // Set section font size without a grouping wrapper (longtable cannot be
    // inside a TeX group).  The change persists until \clearpage or the next
    // \fontsize\selectfont, which is fine since each section is on its own page.
    s.push_str(&format!("\\fontsize{{{fsize}}}{{{fskip}}}\\selectfont\n"));

    // Section heading: bold, with a rule underneath.
    s.push_str(&format!(
        "{{\\fontsize{{14}}{{17}}\\selectfont\\bfseries {}}}\\\\\n",
        tex_esc(group_name),
    ));
    s.push_str("\\vspace{0.5mm}\\rule{\\linewidth}{0.6pt}\\\\\n");

    // Subtitle: date range · interval · generated (small, italic).
    s.push_str(&format!(
        "{{\\fontsize{{7.5}}{{9.5}}\\selectfont\\itshape \
         {} $\\cdot$ Every {} week(s) $\\cdot$ Generated {}}}\n",
        tex_esc(date_range), interval, tex_esc(generated),
    ));
    s.push_str("\\vspace{2mm}\n\n");

    // Column widths (total ≈ 182 mm = A4 210 mm − 28 mm margins):
    //   13 + 28 + 45 + 45 + 9 + 24 = 164 mm content
    //   + 6 cols × 2 × 4 pt tabcolsep ≈ 17 mm padding  ≈ 181 mm
    let col_spec = concat!(
        "|>{\\centering\\arraybackslash}p{13mm}",
        "|>{\\raggedright\\arraybackslash}p{28mm}",
        "|>{\\raggedright\\arraybackslash}p{45mm}",
        "|>{\\raggedright\\arraybackslash}p{45mm}",
        "|>{\\centering\\arraybackslash}p{9mm}",
        "|p{24mm}|"
    );
    s.push_str(&format!("\\begin{{longtable}}{{{col_spec}}}\n"));

    // ── First-page header ────────────────────────────────────────────────────
    s.push_str("\\hline\n");
    s.push_str("\\textbf{Week} & \\textbf{Dates} & \\textbf{Area / Rooms} & \
                \\textbf{Responsible} & $\\checkmark$ & \\textbf{Date} \\\\\n");
    s.push_str("\\hline\\hline\n");
    s.push_str("\\endfirsthead\n");

    // ── Continuation header (repeated on every subsequent page) ──────────────
    s.push_str(&format!(
        "\\multicolumn{{6}}{{l}}{{\\small\\itshape {} (continued)}} \\\\\n",
        tex_esc(group_name),
    ));
    s.push_str("\\hline\n");
    s.push_str("\\textbf{Week} & \\textbf{Dates} & \\textbf{Area / Rooms} & \
                \\textbf{Responsible} & $\\checkmark$ & \\textbf{Date} \\\\\n");
    s.push_str("\\hline\\hline\n");
    s.push_str("\\endhead\n");

    // ── Empty footer / last-footer (rows already end with \hline) ────────────
    s.push_str("\\endfoot\n");
    s.push_str("\\endlastfoot\n");

    // ── Data rows ─────────────────────────────────────────────────────────────
    // Group consecutive rows that share the same (iso_year, iso_week).
    // The week number and date range are printed only on the first row of each
    // group; subsequent rows leave those cells blank so the group reads as a
    // visual unit.  A full \hline closes the group; within the group only
    // columns 3–6 get a \cline so columns 1–2 appear merged across the rows.
    let mut i = 0;
    while i < rows.len() {
        let key = (rows[i].iso_year, rows[i].iso_week);
        let mut j = i + 1;
        while j < rows.len() && (rows[j].iso_year, rows[j].iso_week) == key {
            j += 1;
        }

        for k in i..j {
            let a = rows[k];
            if k == i {
                s.push_str(&format!(
                    "\\textbf{{{}}}{{\\newline{{\\tiny {}}}}}",
                    a.iso_week, a.iso_year,
                ));
                s.push_str(" & ");
                s.push_str(&tex_esc(&a.week_label));
            } else {
                // Blank week and date cells — visual merge with the row above.
                s.push_str(" & ");
            }
            s.push_str(" & ");

            // Area / rooms cell.
            let area = match &a.slot_name {
                Some(slot) if !a.room_names.is_empty() => format!(
                    "{}\\newline{{\\tiny {}}}",
                    tex_esc(slot),
                    tex_esc(&a.room_names.join(", ")),
                ),
                // No sub-rooms: render at a readable size regardless of the
                // table's scaled-down font.
                Some(slot) => format!(
                    "{{\\fontsize{{10}}{{13}}\\selectfont {}}}",
                    tex_esc(slot),
                ),
                None if !a.room_names.is_empty() => format!(
                    "{{\\tiny {}}}",
                    tex_esc(&a.room_names.join(", ")),
                ),
                None => String::new(),
            };
            s.push_str(&area);
            s.push_str(" & ");

            // Responsible.
            s.push_str(&tex_esc(a.assignee_name()));
            s.push_str(" & ");

            // ✓ cell: show checkmark when done, leave empty when pending.
            if a.is_completed {
                s.push_str("$\\checkmark$");
            }
            s.push_str(" & ");

            // Date cell: completion date when known, otherwise blank for manual entry.
            if let Some(date) = a.completed_at {
                s.push_str(&tex_esc(&date.format("%-d %b %Y").to_string()));
            }
            s.push_str(" \\\\\n");

            if k == j - 1 {
                s.push_str("\\hline\n");
            } else {
                // Partial rule: separate the slot rows but keep Week/Dates
                // visually grouped (no line under columns 1–2).
                s.push_str("\\cline{3-6}\n");
            }
        }

        i = j;
    }

    s.push_str("\\end{longtable}\n\n");

    // Legend.
    s.push_str(
        "{\\fontsize{6.5}{8}\\selectfont\\hfill \
         $\\checkmark$ = done}\n"
    );

    s
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Select LaTeX font size based on row count so all rows fit on one A4 page.
///
/// Returns (font_size_pt, baseline_skip_pt).
fn font_size_for_rows(n: usize) -> (&'static str, &'static str) {
    match n {
        0..=25 => ("10", "13"),
        26..=34 => ("9",  "11"),
        35..=45 => ("8",  "10"),
        _       => ("7",  "9"),
    }
}

/// Escape a string for safe use in LaTeX body text.
fn tex_esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&'  => out.push_str(r"\&"),
            '%'  => out.push_str(r"\%"),
            '$'  => out.push_str(r"\$"),
            '#'  => out.push_str(r"\#"),
            '_'  => out.push_str(r"\_"),
            '{'  => out.push_str(r"\{"),
            '}'  => out.push_str(r"\}"),
            '~'  => out.push_str(r"\textasciitilde{}"),
            '^'  => out.push_str(r"\textasciicircum{}"),
            '\\' => out.push_str(r"\textbackslash{}"),
            // Unicode dashes: map to LaTeX ligatures (pdfLaTeX drops raw U+2013/U+2014).
            '\u{2013}' => out.push_str("--"),   // en dash
            '\u{2014}' => out.push_str("---"),  // em dash
            c    => out.push(c),
        }
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crate::{
        domain::{CleaningGroup, Person},
        schedule::build_schedule,
        state::State,
    };

    fn make_state() -> State {
        let mut st = State::default();
        st.created_at    = Some(Utc::now());
        st.last_modified = st.created_at;
        let p  = Person::new_matrix("@bob:example.org");
        let id = p.id.clone();
        st.persons.push(p);
        let mut g = CleaningGroup::new("Hallway");
        g.member_ids.push(id);
        st.cleaning_groups.push(g);
        st
    }

    #[test]
    fn tex_output_is_deterministic() {
        let st  = make_state();
        let sn1 = build_schedule(&st, 1, 4);
        let sn2 = build_schedule(&st, 1, 4);
        assert_eq!(render_tex(&sn1), render_tex(&sn2));
    }

    #[test]
    fn tex_contains_group_name() {
        let st  = make_state();
        let sn  = build_schedule(&st, 1, 2);
        let tex = render_tex(&sn);
        assert!(tex.contains("Hallway"), "group name must appear in .tex output");
    }

    #[test]
    fn tex_column_headers_are_english() {
        let st  = make_state();
        let sn  = build_schedule(&st, 1, 2);
        let tex = render_tex(&sn);
        assert!(tex.contains("Responsible"), "column headers must be in English");
        assert!(tex.contains("Area / Rooms"));
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
        let sn  = build_schedule(&st, 1, 2);
        let tex = render_tex(&sn);
        assert!(tex.contains("Floor A"));
        assert!(tex.contains("Floor B"));
        assert!(tex.contains(r"\clearpage"), "groups must be separated by \\clearpage");
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
    fn uses_longtable_not_tabularx() {
        let st  = make_state();
        let sn  = build_schedule(&st, 1, 2);
        let tex = render_tex(&sn);
        assert!(tex.contains("longtable"), "must use longtable for page-breaking");
        assert!(!tex.contains("tabularx"), "tabularx cannot break across pages");
    }
}
