//! PDF/HTML schedule rendering.
//!
//! `render_pdf` is a pure function over a `ScheduleSnapshot`.
//!
//! Layout: one `<section>` per cleaning group, each section targets one A4 page.
//! Sections are auto-zoomed so the week rows fit the page; if there are too many
//! weeks to stay legible the content simply overflows to a second page for that group.
//!
//! Determinism guarantee: identical snapshot → identical HTML bytes.

use crate::schedule::ScheduleSnapshot;

/// Approximate layout constants for one A4 page (portrait, 10mm/12mm margins).
/// Used to compute the per-section zoom factor.
const A4_USABLE_MM: f32   = 249.0; // 297mm - 2 × 10mm margin - ~28mm for headings/legend
const ROW_HEIGHT_MM: f32  = 6.5;   // at zoom=1.0
const MIN_ZOOM: f32        = 0.50;  // below this the text becomes unreadable

pub fn render_pdf(snapshot: &ScheduleSnapshot) -> String {
    let cur_week = {
        let d = chrono::Utc::now().date_naive();
        use chrono::Datelike;
        (d.iso_week().year(), d.iso_week().week())
    };
    let generated = snapshot.state_timestamp.format("%d %b %Y").to_string();

    // Collect unique groups in first-appearance order.
    let mut group_order: Vec<(String, String)> = Vec::new();
    for a in &snapshot.assignments {
        if !group_order.iter().any(|(id, _)| id == &a.group_id) {
            group_order.push((a.group_id.clone(), a.group_name.clone()));
        }
    }

    let mut sections = String::new();

    for (group_id, group_name) in &group_order {
        let group_rows: Vec<_> = snapshot.assignments.iter()
            .filter(|a| &a.group_id == group_id)
            .collect();

        // Auto-zoom so rows fit on one A4 page.
        let row_count = group_rows.len();
        let max_at_1x = (A4_USABLE_MM / ROW_HEIGHT_MM) as usize;
        let zoom: f32 = if row_count <= max_at_1x {
            1.0
        } else {
            (max_at_1x as f32 / row_count as f32).max(MIN_ZOOM)
        };

        // Date range subtitle.
        let date_range = match (group_rows.first(), group_rows.last()) {
            (Some(f), Some(l)) if f.week_monday != l.week_sunday => {
                format!("{} – {}",
                    f.week_monday.format("%d %b %Y"),
                    l.week_sunday.format("%d %b %Y"))
            }
            (Some(f), _) => f.week_label.clone(),
            _ => String::new(),
        };

        let mut tbody = String::new();
        for a in &group_rows {
            let is_current = (a.iso_year, a.iso_week) == cur_week;

            // Area column: slot name for multi-slot groups, otherwise empty
            // (rooms shown separately below the name).
            let area = match &a.slot_name {
                Some(slot) => {
                    if a.room_names.is_empty() {
                        e(slot)
                    } else {
                        format!("{}<span class=\"rooms\">{}</span>",
                            e(slot), e(&a.room_names.join(", ")))
                    }
                }
                None => {
                    if a.room_names.is_empty() {
                        String::new()
                    } else {
                        format!("<span class=\"rooms\">{}</span>",
                            e(&a.room_names.join(", ")))
                    }
                }
            };

            let check = if a.is_completed {
                "<span class=\"check-done\">✓</span>"
            } else {
                "<span class=\"check-box\"></span>"
            };

            tbody.push_str(&format!(
                "<tr class=\"{cls}\">\
                  <td class=\"col-week\">{wk}<br><small class=\"col-year\">{yr}</small></td>\
                  <td class=\"col-dates\">{dates}</td>\
                  <td class=\"col-area\">{area}</td>\
                  <td class=\"col-name\">{name}</td>\
                  <td class=\"col-check\">{check}</td>\
                  <td class=\"col-notes\"></td>\
                </tr>\n",
                cls   = if a.is_completed { "done" } else if is_current { "current" } else { "" },
                wk    = a.iso_week,
                yr    = a.iso_year,
                dates = e(&a.week_label),
                area  = area,
                name  = e(a.assignee_name()),
                check = check,
            ));
        }

        sections.push_str(&format!(
            "<section class=\"group-section\" style=\"zoom:{zoom:.2}\">\n\
              <h2>🧹 {name}</h2>\n\
              <p class=\"meta\">{range} &nbsp;·&nbsp; Every {interval} week(s) &nbsp;·&nbsp; Generated {gen}</p>\n\
              <table>\n\
                <thead><tr>\
                  <th class=\"col-week\">Week</th>\
                  <th class=\"col-dates\">Dates</th>\
                  <th class=\"col-area\">Area / Rooms</th>\
                  <th class=\"col-name\">Responsible</th>\
                  <th class=\"col-check\">✓</th>\
                  <th class=\"col-notes\">Notes</th>\
                </tr></thead>\n\
                <tbody>\n{tbody}</tbody>\n\
              </table>\n\
              <p class=\"legend\">☐ = pending &nbsp;·&nbsp; ✓ = done</p>\n\
            </section>\n",
            name     = e(group_name),
            range    = date_range,
            interval = snapshot.interval_weeks,
            gen      = generated,
            zoom     = zoom,
            tbody    = tbody,
        ));
    }

    format!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>Cleaning Schedule</title>
<style>
  @page {{ size: A4 portrait; margin: 10mm 12mm; }}
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ font-family: Arial, Helvetica, sans-serif; color: #000; background: #fff; }}

  .group-section {{ page-break-after: always; }}
  .group-section:last-child {{ page-break-after: auto; }}

  h2 {{ font-size: 13pt; margin-bottom: 2mm; }}
  p.meta {{ font-size: 7.5pt; color: #555; margin-bottom: 3mm; }}

  table {{ width: 100%; border-collapse: collapse; table-layout: fixed; }}
  th, td {{
    border: 1px solid #aaa;
    padding: 1.5mm 2mm;
    vertical-align: middle;
    line-height: 1.25;
    overflow: hidden;
  }}
  th {{ background: #f0f0f0; font-size: 7.5pt; font-weight: bold; text-align: left; }}

  .col-week  {{ width: 10mm; text-align: center; font-weight: bold; font-size: 8pt; }}
  .col-year  {{ font-size: 6.5pt; font-weight: normal; color: #666; }}
  .col-dates {{ width: 24mm; font-size: 7.5pt; white-space: nowrap; }}
  .col-area  {{ width: 38mm; font-size: 7.5pt; }}
  .col-name  {{ font-size: 8.5pt; }}
  .col-check {{ width: 8mm; text-align: center; }}
  .col-notes {{ width: 30mm; }}

  .rooms {{ display: block; color: #666; font-size: 6.5pt; margin-top: 0.5mm; }}

  /* Proper checkbox: empty bordered square when pending, tick when done */
  .check-box {{
    display: inline-block;
    width: 4.5mm;
    height: 4.5mm;
    border: 1.5px solid #333;
    vertical-align: middle;
  }}
  .check-done {{ font-size: 10pt; vertical-align: middle; }}

  tr.done    {{ background: #edfaed; }}
  tr.current {{ background: #fffbea; }}

  p.legend {{ font-size: 6.5pt; color: #999; margin-top: 2.5mm; }}

  @media print {{
    .group-section      {{ page-break-after: always; }}
    .group-section:last-child {{ page-break-after: auto; }}
    tr.done    {{ background: #edfaed; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    tr.current {{ background: #fffbea; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    th         {{ background: #f0f0f0; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
  }}
</style>
</head>
<body>
{sections}</body>
</html>"#,
        sections = sections,
    )
}

fn e(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
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
    fn pdf_output_is_deterministic() {
        let st  = make_state();
        let sn1 = build_schedule(&st, 1, 4);
        let sn2 = build_schedule(&st, 1, 4);
        assert_eq!(render_pdf(&sn1), render_pdf(&sn2));
    }

    #[test]
    fn pdf_contains_group_name() {
        let st  = make_state();
        let sn  = build_schedule(&st, 1, 2);
        let html = render_pdf(&sn);
        assert!(html.contains("Hallway"), "group name should appear in PDF");
    }

    #[test]
    fn pdf_title_is_english() {
        let st   = make_state();
        let sn   = build_schedule(&st, 1, 2);
        let html = render_pdf(&sn);
        assert!(html.contains("<title>Cleaning Schedule</title>"));
        assert!(!html.contains("Putzplan"));
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
        let sn   = build_schedule(&st, 1, 2);
        let html = render_pdf(&sn);
        assert_eq!(html.matches("<section class=\"group-section\"").count(), 2);
        assert!(html.contains("Floor A"));
        assert!(html.contains("Floor B"));
    }
}
