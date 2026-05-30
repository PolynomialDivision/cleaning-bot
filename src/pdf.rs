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

/// Approximate layout constants for one A4 page (portrait, 14mm margins).
/// The usable height excludes section header (~20mm), table header (~8mm),
/// and legend (~8mm) — leaving ~213mm for data rows.
const A4_USABLE_MM: f32 = 213.0;
const ROW_HEIGHT_MM: f32 = 7.0;  // at zoom=1.0 with 1.8mm cell padding
const MIN_ZOOM: f32      = 0.62; // ~8.5pt effective body text — still legible

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
                format!("{} \u{2013} {}",
                    f.week_monday.format("%d %b %Y"),
                    l.week_sunday.format("%d %b %Y"))
            }
            (Some(f), _) => f.week_label.clone(),
            _ => String::new(),
        };

        let mut tbody = String::new();
        for a in &group_rows {
            let is_current = (a.iso_year, a.iso_week) == cur_week;

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
                "<span class=\"check-done\">&#x2713;</span>"
            } else {
                "<span class=\"check-box\"></span>"
            };

            tbody.push_str(&format!(
                "<tr class=\"{cls}\">\
                  <td class=\"col-week\"><span class=\"kw\">{wk}</span><span class=\"col-year\">{yr}</span></td>\
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
              <div class=\"section-header\">\
                <h2>{name}</h2>\
                <p class=\"meta\">{range} &nbsp;&middot;&nbsp; Every {interval} week(s) &nbsp;&middot;&nbsp; Generated {gen}</p>\
              </div>\n\
              <table>\n\
                <thead><tr>\
                  <th class=\"col-week\">Week</th>\
                  <th class=\"col-dates\">Dates</th>\
                  <th class=\"col-area\">Area / Rooms</th>\
                  <th class=\"col-name\">Responsible</th>\
                  <th class=\"col-check\">&#x2713;</th>\
                  <th class=\"col-notes\">Notes</th>\
                </tr></thead>\n\
                <tbody>\n{tbody}</tbody>\n\
              </table>\n\
              <p class=\"legend\">&#x25A1; = pending &nbsp;&middot;&nbsp; &#x2713; = done</p>\n\
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
  @page {{ size: A4 portrait; margin: 14mm; }}
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ font-family: 'Helvetica Neue', Arial, Helvetica, sans-serif; color: #1a1a1a; background: #fff; }}

  .group-section {{ page-break-after: always; }}
  .group-section:last-child {{ page-break-after: auto; }}

  /* Section header: coloured left border + light background */
  .section-header {{
    border-left: 4px solid #2c5f8a;
    padding: 2.5mm 4mm;
    margin-bottom: 3mm;
    background: #f4f8fb;
  }}
  h2 {{ font-size: 13pt; font-weight: 700; color: #1a3a52; }}
  p.meta {{ font-size: 7.5pt; color: #666; margin-top: 1mm; }}

  table {{ width: 100%; border-collapse: collapse; table-layout: fixed; margin-top: 1mm; }}

  /* Dark blue header band */
  thead tr {{ background: #2c5f8a; }}
  th {{
    padding: 2mm 2.5mm;
    font-size: 7pt;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.4px;
    color: #fff;
    text-align: left;
    border: none;
  }}

  td {{
    padding: 1.8mm 2.5mm;
    font-size: 9pt;
    border: none;
    border-bottom: 1px solid #e8e8e8;
    vertical-align: middle;
    line-height: 1.3;
    overflow: hidden;
  }}

  /* Alternating row shading */
  tbody tr:nth-child(odd)  {{ background: #fff; }}
  tbody tr:nth-child(even) {{ background: #f7f7f7; }}
  tr.done    {{ background: #eaf6ea !important; }}
  tr.current {{ background: #fff8e1 !important; }}

  /* Column widths */
  .col-week  {{ width: 11mm; text-align: center; }}
  .kw        {{ display: block; font-weight: 700; font-size: 9.5pt; }}
  .col-year  {{ display: block; font-size: 6pt; color: #999; font-weight: 400; }}
  .col-dates {{ width: 27mm; font-size: 8pt; color: #555; }}
  .col-area  {{ width: 40mm; font-size: 8.5pt; }}
  .col-name  {{ font-size: 9.5pt; font-weight: 500; }}
  .col-check {{ width: 10mm; text-align: center; }}
  .col-notes {{ width: 32mm; }}

  .rooms {{ display: block; color: #999; font-size: 7pt; margin-top: 0.5mm; }}

  /* Checkbox: empty bordered square (pending) or green tick (done) */
  .check-box {{
    display: inline-block;
    width: 5mm;
    height: 5mm;
    border: 1.5px solid #555;
    border-radius: 1px;
    vertical-align: middle;
  }}
  .check-done {{ font-size: 11pt; color: #2e7d32; vertical-align: middle; line-height: 1; }}

  p.legend {{ font-size: 6.5pt; color: #bbb; margin-top: 2.5mm; text-align: right; }}

  @media print {{
    .group-section           {{ page-break-after: always; }}
    .group-section:last-child {{ page-break-after: auto; }}
    .section-header          {{ background: #f4f8fb !important; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    thead tr                 {{ background: #2c5f8a !important; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    th                       {{ color: #fff !important; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    tbody tr:nth-child(even) {{ background: #f7f7f7 !important; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    tr.done                  {{ background: #eaf6ea !important; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    tr.current               {{ background: #fff8e1 !important; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    .check-done              {{ color: #2e7d32 !important; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
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
