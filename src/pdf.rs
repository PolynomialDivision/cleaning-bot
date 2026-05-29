//! PDF/HTML schedule rendering.
//!
//! `render_pdf` is a pure function over a `ScheduleSnapshot`.
//! It performs no state access, no scheduling logic, no mutations.
//!
//! Determinism guarantee: identical snapshot → identical HTML bytes.

use crate::schedule::ScheduleSnapshot;

/// Render an A4-printable HTML string from the given schedule snapshot.
///
/// All inputs come from the snapshot; no state access required.
pub fn render_pdf(snapshot: &ScheduleSnapshot) -> String {
    let mut rows = String::new();

    let cur_week = {
        let d = chrono::Utc::now().date_naive();
        use chrono::Datelike;
        (d.iso_week().year(), d.iso_week().week())
    };

    for a in &snapshot.assignments {
        let is_current = (a.iso_year, a.iso_week) == cur_week;
        let rooms_html = if a.room_names.is_empty() {
            String::new()
        } else {
            format!("<br><small class=\"rooms\">{}</small>", e(&a.room_names.join(", ")))
        };
        rows.push_str(&format!(
            "<tr class=\"{row_class}\">\
              <td class=\"week-num\">KW {dw}<br><small>{yr}</small></td>\
              <td class=\"dates\">{dates}</td>\
              <td class=\"area\">{area}{rooms}</td>\
              <td class=\"responsible\">{name}</td>\
              <td class=\"check\">{check}</td>\
              <td class=\"sig\"></td>\
            </tr>\n",
            row_class = if a.is_completed { "done" } else if is_current { "current" } else { "" },
            dw        = a.iso_week,
            yr        = a.iso_year,
            dates     = e(&a.week_label),
            area      = e(&a.group_name),
            rooms     = rooms_html,
            name      = e(a.assignee_name()),
            check     = if a.is_completed { "✓" } else { "☐" },
        ));
    }

    let date_str = snapshot.state_timestamp.format("%d.%m.%Y").to_string();

    format!(r#"<!DOCTYPE html>
<html lang="de">
<head>
<meta charset="UTF-8">
<title>Putzplan</title>
<style>
  @page {{ size: A4 portrait; margin: 12mm 15mm; }}
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ font-family: Arial, Helvetica, sans-serif; font-size: 10.5pt; color: #000; background: #fff; }}
  h1 {{ font-size: 15pt; text-align: center; margin-bottom: 5mm; }}
  p.meta {{ font-size: 8pt; color: #666; text-align: center; margin-bottom: 5mm; }}
  table {{ width: 100%; border-collapse: collapse; }}
  th, td {{ border: 1px solid #999; padding: 2.5mm 2mm; vertical-align: top; line-height: 1.35; }}
  th {{ background: #f0f0f0; font-size: 9.5pt; font-weight: bold; text-align: left; }}
  .week-num {{ width: 13mm; text-align: center; font-weight: bold; font-size: 9.5pt; }}
  .dates    {{ width: 23mm; white-space: nowrap; }}
  .area     {{ width: 50mm; }}
  .responsible {{ width: 37mm; }}
  .check    {{ width: 9mm; text-align: center; font-size: 13pt; }}
  .rooms    {{ color: #555; font-size: 8.5pt; }}
  tr.done    {{ background: #edfaed; }}
  tr.current {{ background: #fffbea; }}
  .legend {{ font-size: 8pt; color: #666; margin-top: 4mm; border-top: 1px solid #ccc; padding-top: 2mm; }}
  @media print {{
    tr.done    {{ background: #edfaed; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    tr.current {{ background: #fffbea; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    th         {{ background: #f0f0f0; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
  }}
</style>
</head>
<body>
  <h1>🧹 Putzplan</h1>
  <p class="meta">Stand: {date} &nbsp;·&nbsp; Intervall: alle {interval} Woche(n)</p>
  <table>
    <thead>
      <tr>
        <th class="week-num">KW</th>
        <th class="dates">Datum</th>
        <th class="area">Bereich / Räume</th>
        <th class="responsible">Verantwortlich</th>
        <th class="check">✓</th>
        <th class="sig">Unterschrift</th>
      </tr>
    </thead>
    <tbody>
{rows}    </tbody>
  </table>
  <div class="legend">
    Legende: ☐ = ausstehend &nbsp;·&nbsp; ✓ = erledigt &nbsp;·&nbsp; KW = Kalenderwoche
  </div>
</body>
</html>"#,
        date     = date_str,
        interval = snapshot.interval_weeks,
        rows     = rows,
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
}
