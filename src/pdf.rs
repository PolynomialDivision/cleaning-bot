//! Generate a printable HTML cleaning schedule.

use crate::state::{State, add_weeks, current_iso_week, week_dates, weeks_between};

pub fn generate_schedule_html(state: &State, interval: u32, weeks: usize) -> String {
    let (cur_y, cur_w) = current_iso_week();
    let (start_y, start_w) = state.tracking_start();
    let iv = interval as i64;

    let elapsed = weeks_between((start_y, start_w), (cur_y, cur_w));
    let first_due = if elapsed < 0 {
        (start_y, start_w)
    } else {
        let past = elapsed / iv;
        if elapsed % iv == 0 { (cur_y, cur_w) }
        else { add_weeks(start_y, start_w, (past + 1) * iv) }
    };

    let units = state.units();
    let mut rows = String::new();

    for i in 0..(weeks as i64) {
        let (dy, dw) = add_weeks(first_due.0, first_due.1, i * iv);
        let dates = week_dates(dy, dw);
        let is_current = (dy, dw) == (cur_y, cur_w);

        for unit in &units {
            let responsible = state.responsible_user(unit, dy, dw, interval);
            let name_str = responsible.as_ref().map(|p| p.display()).unwrap_or("—");
            let is_done = state.is_completed(&unit.name, dy, dw);
            let rooms_str = unit.rooms_text().unwrap_or_default();

            rows.push_str(&format!(
                "<tr class=\"{row_class}\">\
                  <td class=\"week-num\">KW {dw}<br><small>{dy}</small></td>\
                  <td class=\"dates\">{dates}</td>\
                  <td class=\"area\">{area}{rooms}</td>\
                  <td class=\"responsible\">{name}</td>\
                  <td class=\"check\">{check}</td>\
                  <td class=\"sig\"></td>\
                </tr>\n",
                row_class = if is_done { "done" } else if is_current { "current" } else { "" },
                area = e(&unit.name),
                rooms = if rooms_str.is_empty() { String::new() } else { format!("<br><small class=\"rooms\">{}</small>", e(&rooms_str)) },
                name = e(name_str),
                check = if is_done { "✓" } else { "☐" },
            ));
        }
    }

    format!(r#"<!DOCTYPE html>
<html lang="de">
<head>
<meta charset="UTF-8">
<title>Putzplan</title>
<style>
  @page {{ size: A4 portrait; margin: 12mm 15mm; }}
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ font-family: Arial, Helvetica, sans-serif; font-size: 10.5pt; color: #000; background: #fff; }}
  h1 {{ font-size: 15pt; text-align: center; margin-bottom: 5mm; letter-spacing: 0.5px; }}
  p.meta {{ font-size: 8pt; color: #666; text-align: center; margin-bottom: 5mm; }}
  table {{ width: 100%; border-collapse: collapse; }}
  th, td {{ border: 1px solid #999; padding: 2.5mm 2mm; vertical-align: top; line-height: 1.35; }}
  th {{ background: #f0f0f0; font-size: 9.5pt; font-weight: bold; text-align: left; }}
  .week-num {{ width: 13mm; text-align: center; font-weight: bold; font-size: 9.5pt; }}
  .dates    {{ width: 23mm; white-space: nowrap; }}
  .area     {{ width: 50mm; }}
  .responsible {{ width: 37mm; }}
  .check    {{ width: 9mm; text-align: center; font-size: 13pt; }}
  .sig      {{ /* remaining width */ }}
  .rooms    {{ color: #555; font-size: 8.5pt; }}
  tr.done {{ background: #edfaed; color: #444; }}
  tr.current {{ background: #fffbea; }}
  .legend {{ font-size: 8pt; color: #666; margin-top: 4mm; border-top: 1px solid #ccc; padding-top: 2mm; }}
  @media print {{
    tr.done {{ background: #edfaed; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    tr.current {{ background: #fffbea; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
    th {{ background: #f0f0f0; -webkit-print-color-adjust: exact; print-color-adjust: exact; }}
  }}
</style>
</head>
<body>
  <h1>🧹 Putzplan</h1>
  <p class="meta">Erstellt: {date} &nbsp;·&nbsp; Intervall: alle {interval} Woche(n) &nbsp;·&nbsp; {weeks} Wochen Vorschau</p>
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
    Legende: ☐ = ausstehend &nbsp;·&nbsp; ✓ = erledigt &nbsp;·&nbsp; KW = Kalenderwoche &nbsp;·&nbsp; Grün = bereits erledigt &nbsp;·&nbsp; Gelb = laufende Woche
  </div>
</body>
</html>"#,
        date = chrono::Utc::now().format("%d.%m.%Y"),
        interval = interval,
        weeks = weeks,
        rows = rows,
    )
}

fn e(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
}
