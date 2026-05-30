//! LaTeX schedule renderer.
//!
//! `render_tex` is a pure function over a `ScheduleSnapshot`.
//!
//! Layout: one section per cleaning group, separated by `\clearpage`.
//! Font size scales down automatically when a group has many rows so that
//! all weeks fit on a single A4 page for that floor.
//!
//! Determinism guarantee: identical snapshot → identical .tex bytes.

use crate::schedule::ScheduleSnapshot;

// ── LaTeX preamble ────────────────────────────────────────────────────────────

const PREAMBLE: &str = r#"\documentclass[a4paper]{article}
\usepackage[a4paper, top=12mm, bottom=12mm, left=14mm, right=14mm]{geometry}
\usepackage{booktabs}
\usepackage{tabularx}
\usepackage{array}
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
\setlength{\tabcolsep}{3.5pt}

\definecolor{headerblue}{RGB}{44,95,138}
\definecolor{rowgray}{RGB}{247,247,247}
\definecolor{donebg}{RGB}{234,246,234}
\definecolor{currentbg}{RGB}{255,248,225}
\definecolor{metagray}{RGB}{102,102,102}
\definecolor{roomfg}{RGB}{153,153,153}
\definecolor{yearfg}{RGB}{170,170,170}
\definecolor{legendfg}{RGB}{187,187,187}
\definecolor{donegreen}{RGB}{46,125,50}

% Printable checkbox: a bordered square sized to match the line height.
\newcommand{\checkbox}{\raisebox{0.5pt}{\framebox[4.5mm][c]{\rule{0pt}{3.5mm}}}}"#;

// ── Public entry point ────────────────────────────────────────────────────────

pub fn render_tex(snapshot: &ScheduleSnapshot) -> String {
    let cur_week = {
        let d = chrono::Utc::now().date_naive();
        use chrono::Datelike;
        (d.iso_week().year(), d.iso_week().week())
    };
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
            group_name, &date_range, snapshot.interval_weeks, &generated, &rows, cur_week,
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
    cur_week:   (i32, u32),
) -> String {
    let (fsize, fskip) = font_size_for_rows(rows.len());
    let mut s = String::new();

    // Open font-size group so scaling doesn't bleed past this section.
    s.push_str(&format!("{{\\fontsize{{{fsize}}}{{{fskip}}}\\selectfont\n"));

    // Section heading.
    s.push_str(&format!(
        "{{\\fontsize{{14}}{{17}}\\selectfont\\bfseries\\color{{headerblue}} {}}}\\\\\n",
        tex_esc(group_name),
    ));

    // Subtitle: date range · interval · generated.
    s.push_str(&format!(
        "{{\\fontsize{{7.5}}{{9.5}}\\selectfont\\color{{metagray}} \
         {} $\\cdot$ Every {} week(s) $\\cdot$ Generated {}}}\n",
        tex_esc(date_range), interval, tex_esc(generated),
    ));
    s.push_str("\\vspace{2.5mm}\n\n");

    // Column spec: Week | Dates | Area/Rooms | Responsible (flex) | ✓ | Notes
    s.push_str("\\begin{tabularx}{\\linewidth}{\n");
    s.push_str("  >{\\centering\\arraybackslash}p{13mm}\n");
    s.push_str("  >{\\raggedright\\arraybackslash}p{28mm}\n");
    s.push_str("  >{\\raggedright\\arraybackslash}p{36mm}\n");
    s.push_str("  >{\\raggedright\\arraybackslash}X\n");
    s.push_str("  >{\\centering\\arraybackslash}p{9mm}\n");
    s.push_str("  p{26mm}\n");
    s.push_str("}\n");

    // Header row (blue background, white text).
    s.push_str("\\rowcolor{headerblue}\n");
    s.push_str("{\\color{white}\\textbf{Week}} & ");
    s.push_str("{\\color{white}\\textbf{Dates}} & ");
    s.push_str("{\\color{white}\\textbf{Area / Rooms}} & ");
    s.push_str("{\\color{white}\\textbf{Responsible}} & ");
    s.push_str("{\\color{white}$\\checkmark$} & ");
    s.push_str("{\\color{white}\\textbf{Notes}} \\\\\n");

    // Data rows.
    for (i, a) in rows.iter().enumerate() {
        let is_current = (a.iso_year, a.iso_week) == cur_week;

        let bg = if a.is_completed   { "donebg"    }
                 else if is_current  { "currentbg" }
                 else if i % 2 == 1 { "rowgray"   }
                 else                { "white"     };

        s.push_str(&format!("\\rowcolor{{{bg}}}\n"));

        // Week cell: bold week number, tiny year below.
        s.push_str(&format!(
            "\\textbf{{{}}}{{\\newline{{\\tiny\\color{{yearfg}}{}}}}}",
            a.iso_week, a.iso_year,
        ));
        s.push_str(" & ");

        // Date range.
        s.push_str(&tex_esc(&a.week_label));
        s.push_str(" & ");

        // Area / rooms cell.
        let area = match &a.slot_name {
            Some(slot) if !a.room_names.is_empty() => format!(
                "{}\\newline{{\\tiny\\color{{roomfg}}{}}}",
                tex_esc(slot),
                tex_esc(&a.room_names.join(", ")),
            ),
            Some(slot) => tex_esc(slot),
            None if !a.room_names.is_empty() => format!(
                "{{\\tiny\\color{{roomfg}}{}}}",
                tex_esc(&a.room_names.join(", ")),
            ),
            None => String::new(),
        };
        s.push_str(&area);
        s.push_str(" & ");

        // Responsible.
        s.push_str(&tex_esc(a.assignee_name()));
        s.push_str(" & ");

        // Checkbox / tick.
        if a.is_completed {
            s.push_str("{\\textcolor{donegreen}{$\\checkmark$}}");
        } else {
            s.push_str("\\checkbox");
        }
        s.push_str(" & \\\\\n");
    }

    s.push_str("\\end{tabularx}\n\n");

    // Legend.
    s.push_str(
        "{\\fontsize{6.5}{8}\\selectfont\\color{legendfg}\\hfill \
         $\\square$ = pending \\quad $\\checkmark$ = done}\n"
    );

    s.push_str("}\n"); // close font-size group
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
    }
}
