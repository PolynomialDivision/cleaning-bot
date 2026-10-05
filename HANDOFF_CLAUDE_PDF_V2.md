# Übergabe an Claude: schöner, mit Stift ankreuzbarer Cleaning-Plan

Stand: 2026-10-05. Repository: `/home/nick/matrixbots/cleaning-bot`.

## Auftrag und Grenzen

Der Nutzer hat wegen fast aufgebrauchter Credits ausdrücklich um diese Übergabe gebeten. Die Umsetzung ist **noch nicht fertig**. Bitte den aktuellen Working Tree weiterbearbeiten, nicht neu anfangen.

- **Nicht deployen, keine Live-Unraid-Daten lesen/verändern, nicht committen.** Nur lokalen Code, Tests, Dokumentation und Beispielartefakte bearbeiten.
- Es wurden in dieser Sitzung weder Deployments noch Änderungen am Live-State noch Commits durchgeführt.
- Die ursprüngliche automatische Foto-Importfunktion existiert bereits. Aktuell wird ihr PDF-/Scanner-Layout von Version 1 auf Version 2 umgebaut.
- Keine Subagents ohne ausdrücklichen Auftrag.

## Was der Nutzer jetzt will

Priorität: (1) schöner kompakter Aushang, (2) freundlich/spielerisch mit passenden Icons, (3) leicht mit echtem Stift benutzbar, (4) maschinenlesbar vom Handyfoto, (5) konsistent mit dem bestehenden Schedule und Import.

Das vorherige Layout war zu steril: acht hohe Formularzeilen, viel Weißraum, vier große QR-Codes, getrennte Done/Skip- und Tageskreise. Der Nutzer will **keine ausgefüllten Kreise**, sondern **genau ein deutliches handschriftliches X pro Aufgabe**:

- Erlaubte Wochentage plus Skip als Kästchen.
- Ein X im Tageskästchen bedeutet erledigt an diesem Tag.
- Ein X bei Skip bedeutet ausgelassen.
- Alles andere bleibt leer.
- Ein kleiner QR pro Seite für Dokument/Revision/Seite, vier dezente separate Eckmarker zur Perspektivkorrektur.
- Keine OCR für bekannte Namen, Aufgaben oder Daten.
- Tabellencharakter des älteren schönen PDFs zurückbringen: Wochenbadges, klare Linienhierarchie, dezenter Akzent, monochrom brauchbare Icons. Nicht kindlich und nicht wie ein Steuerformular.
- Wöchentliche, 2×/Woche- und Multi-Slot-Gruppen wie `3+4 Floor` korrekt darstellen.
- Am Ende wirklich fragen: „Would I actually want to print this and hang it in a shared house?“ Bei Nein verbessern.

## Bestehende Architektur, die erhalten bleiben soll

- `src/schedule.rs`: gemeinsamer `ScheduleSnapshot` für Matrix, PDF und iCal.
- `src/paper.rs`: persistierte Dokument-/Layoutmanifeste, Baseline-Konfliktprüfung, Scan-Vorschläge, Matrix-Upload-/Reaction-Handler, normaler Domain-Event-Import.
- `src/commands/exports.rs`: aktuelle/zukünftige PDFs über `paper::document` und `paper::pdf`; History-PDF weiterhin über alten kompakten Renderer.
- `src/pdf.rs`: älterer hübscher LaTeX-Tabellenrenderer, weiterhin für History genutzt. Gut als Stilreferenz! Enthält Broom-/Raumicons, Wochenkreise, Akzent `2F6F73`, Helvetica, stärkere Wochenlinien und feinere Slot-Linien.
- `src/main.rs`: automatische Behandlung von Matrix `m.image`; Scan-Reactions vor allgemeinem Reaction-Handling. Kein `!scan` nötig.
- `src/private.rs`: frische Matrix-State-Prüfung für verschlüsselte Zweier-DMs und Trusted-Room-Mitgliedschaft.
- `State.paper_documents`, `State.paper_scans`: persistiert, serde defaults.
- Domain `CleaningCompleted.completed_on` und `Completion.completed_on`: Datum vom Papier separat vom echten Erfassungszeitpunkt `completed_at`. Gemeinsamer Schedule verwendet das ausgewählte Datum für Matrix/PDF/iCal.
- Scan bietet ein einziges Preview mit ✅ Apply / ❌ Cancel und editiert dieses zum Ergebnis. Nur Uploader bestätigt, 24h gültig. Kein Schreiben vor Bestätigung.
- Eigene Aufgaben für normale Nutzer; fremde Aufgaben/Skip nur für Admins. Ganze Seite wird bei Konflikt/unerlaubter Änderung abgelehnt.
- Bestätigung prüft Mitgliedschaft und Baseline erneut; staged State + normaler `CleaningCompleted`/`CleaningSkipped`-Eventpfad + atomisches Save.
- Replay-/Duplikatschutz, Zuweisungsänderungen einschließlich weg-und-zurück, digitales Undo, Datumskonflikte etc. bereits implementiert/getestet. Nicht abschwächen.
- Aktuelle Manifeste bleiben 400 Tage, Vorschläge 7 Tage; Bestätigungsfrist 24h.

## Was in dieser Sitzung geändert wurde

Zu Beginn war `git status --short` sauber. Die ältere V1-Implementierung war also bereits Teil der Ausgangsbasis (ich selbst habe sie nicht committed).

### Rust-Manifest (`src/paper.rs`)

- `Document.layout_version: u8`, Default für alte State-Daten = 1; neue Dokumente = 2.
- Revision hasht jetzt `(2, &pages)`.
- Bis zu **16 statt 8 Aufgaben pro Seite**, jede Gruppe getrennt.
- `Page.fiducials`: `[[10,10],[200,10],[200,287],[10,287]]`, mm von links oben auf A4.
- `Page.qr_center`: `[187,277]`, mm.
- `Row.y`: `58.25 + row_index * 12.5` mm. `Row.task` enthält Slotname separat.
- `Field.size`: 4.8 mm; `id` bleibt stabil aus Duty-ID + Feld-Kind.
- Kein `done`-Feld mehr: nur erlaubte Tage und `skip`.
- Tages-X-Koordinaten: `111 + weekday_from_monday * 10` mm. Skip: x=189 mm. Alle auf `Row.y`.
- Alte neue Struct-Felder mit serde defaults, sodass V1-Manifeste laden.
- Worker schreibt jetzt zwei eingebettete Python-Dateien ins Temp-Verzeichnis: `paper.py` und `paper_v1.py`.
- Bestehender Rust-Layouttest auf 16 Zeilen/Seite und 3 Felder bei 2-Tage-Fenster (2 Tage + Skip) angepasst.

### Legacy-Kompatibilität (`scripts/paper_v1.py`, neu)

- Kopie des vorherigen `scripts/paper.py` für vorhandene V1-Ausdrucke.
- Einzige funktionelle Ergänzung: `decode(im, protocol="CB1")` kann optional auch CB2-QRs dekodieren, damit der neue Scanner die ZBar-ctypes-Anbindung wiederverwenden kann.
- V1-Rendering/Markenerkennung bleiben vorhanden.
- `scripts/fixtures/legacy-v1.png` ist das ursprüngliche synthetische V1-Beispielbild für den Kompatibilitätstest.

### V2-Scanner (`scripts/paper.py`)

- QR: `CB2:<random document id>:<revision>:<page>`, nur ein QR, 18 mm inklusive Quiet-Zone.
- Vier kleine 5-mm-Eckmarker: schwarzes Quadrat, weißer Kreis Radius 1.5 mm, schwarzer Mittelpunkt Radius .5 mm. Sie enthalten keine Nutzerdaten.
- `components`: Run-length Connected Components auf dunklen Pixeln, kein langsamer Python-Pixel-Floodfill.
- `fiducials`: sucht die verschachtelte Quadrat/weißer-Kreis/Punkt-Geometrie. QR-Findermuster haben andere Größenverhältnisse. Nutzt Schwerpunkt des isolierten Mittelpunktes als Registrierungspunkt.
- `normalize`: vier Marker zyklisch ordnen; QR nahe Bottom-right bestimmt Orientierung. Prüft QR-Position gegen Manifest nach Homographie. Normiert auf 6 Pixel/mm.
- `field_value`: lokale Helligkeit, Rand-/Registrierungsprüfung; Innenfläche ohne gedruckten Rand. Unterscheidet leer / X / mehrdeutig über vier diagonale Arme und begrenzte Tinte außerhalb der Diagonalen. Sucht etwas Offset und verschiedene Steigungen. Tick, einzelner Strich, Punkt, komplett gefülltes Kästchen und Scribble sollen abgelehnt werden.
- `read_marks`: genau ein ausgewähltes Feld pro markierter Aufgabe; Tag -> Done, Skip -> Skipped.
- V1-Dokumente werden an Legacy-Scanner delegiert; Identification erkennt weiterhin alte V1-QRs.
- Bestehende Größen-/CPU-/Memory-Limits kommen durch Import des Legacy-Moduls weiterhin zum Tragen.

### V2-PDF (`scripts/paper.py::render`)

- Helvetica, gedecktes Teal, Broom-Icon vor Gruppentitel, kleiner freundlicher Satz, Datumsbereich, Räume.
- Tabelle: Week | When / task | Who | Mon Tue Wed Thu Fri Sat Sun | Skip.
- Wochenbadges, stärkere Linien zwischen Wochen, gepunktete Unterteilung innerhalb einer Woche, dünne Spaltenlinien.
- Nur erlaubte Tageskästchen werden gezeichnet. Bestehende erledigte Aufgaben zeigen Textstatus statt Eingabefeldern.
- Footer: Pen-Icon + genau-ein-X-Anleitung; Camera-Icon + Foto/Bestätigung; kleines Heart-Icon mit Dank.
- QR unten rechts, separate kleine Eckmarker.
- Rendering funktioniert lokal mit pdflatex. `\faCalendarAlt` existierte nicht in der installierten FontAwesome-Version; bereits durch `\faCalendar` ersetzt.

### Tests (`scripts/test_paper.py`, überarbeitet)

- Synthetisches Dokument mit drei Seiten: Kitchen 2×/Woche, `3+4 Floor` wöchentlich mit zwei Slots, Bathroom wöchentlich.
- Echte PDFs rendern, rasterisieren, mit **zwei separaten Stiftstrichen** X zeichnen, nicht Flächen füllen.
- Variierende Linienbreiten, Graustufen, Offsets, ungleichmäßige X.
- Perspektive, Rotation 0/90/180/270/13 Grad, JPEG-Kompression, Helligkeit.
- Leere Felder, Skip, Wochenenden, unerlaubte Mehrfachmarken, Häkchen/Slash/Punkt/Füllung/Scribble, fehlende Marker, Blur, fremdes Bild, alte Revision, zwei Seiten.
- Optionaler Roundtrip mit dem **tatsächlichen Rust-Manifest**, via `PAPER_LAYOUT_FIXTURE`.
- Alte V1-Identifikation separat getestet.

## Aktueller Teststand — wichtig!

Rust nach Manifeständerungen:

```
269 passed; 0 failed
```

Python V2:

```
Ran 12 tests in 8.513s
FAILED (failures=2)
```

Es bestehen **zwei offene Fehler**, nicht als fertig darstellen:

1. `test_pen_width_offset_and_uneven_cross`
   - Fehler: `Unclear mark: use one clear X, not a tick or filled box`.
   - Bereits isoliert: Nur Testfall `width=1`, `gray=30`, `offset=(0,0)`, `uneven=True` scheitert.
   - Die Fälle `(width=2, gray=90, offset=(1,-1))`, `(3,20,(-1,1))`, `(2,40,(2,0))` bestehen.
   - 1 Pixel bei 6px/mm entspricht ~0.17mm, also dünnem Stift. Aktuelle Innen-Tintendichte-Untergrenze `.065` und/oder Minimalanteil pro Diagonalarm `.22` könnten zu streng sein. **Noch keine genaue Messung durchgeführt.**
   - Nicht einfach pauschal alles toleranter machen! Armabdeckung messen und sicherstellen, dass Slash/Tick/Punkt/Scribble/Füllung weiterhin abgelehnt werden.

2. `test_rotation_perspective_and_jpeg`
   - Fehler: `Box outline unclear; flatten the page and retake`.
   - Vermutung: kleine lokale Registrierungs-/Subpixelverschiebung nach Homographie/Fiducial-Messung plus Resampling/JPEG; aktuelle Randmaske ist starr. Welcher Winkel/welches Feld exakt fehlschlägt, wurde **noch nicht isoliert**.
   - Sinnvoller nächster Ansatz: vor Innenklassifikation pro Kästchen begrenzte lokale Registrierung, z.B. ±2 Pixel Suche nach bester Übereinstimmung der vier gedruckten Randseiten. Gedruckten Rand und eigentliche Tinte weiter getrennt bewerten.
   - `field_value` rundet `field.y * SCALE` auf Pixel; y=58.25mm führt auf Halb-Pixel. Diese fractional offset wird aktuell in xx/yy nicht ausdrücklich berücksichtigt. Das prüfen.
   - Border-Test verlangt aktuell je Seite mindestens `.23` dunklen Anteil in einem .6mm breiten Band. Nicht blind senken, sondern Registrierung verbessern und Negativtests behalten.

Die anderen zehn Python-Tests bestanden, insbesondere echter Rust-Manifest-Roundtrip, alle leeren Seiten, Skip, Wochenendfelder, Mehrfachmarken-Abweisung, falsche Markierungen, fehlende Marker, Revision, Legacy-Identifikation und zwei Seiten.

**Clippy wurde für V2 noch nicht erneut ausgeführt.** Die frühere V1-Sitzung hatte clippy grün; das nicht als aktuellen V2-Befund ausgeben.

Logs:
- `/tmp/paper-tests.log` (Rust)
- `/tmp/paper-python.log` (Python mit Tracebacks)
- `/tmp/cleaning-paper-manifest.json` (aktueller synthetischer Rust-Testexport)

## Bisherige visuelle Inspektion und noch sinnvolle Designverbesserungen

`artifacts/paper-twice-weekly.png` wurde tatsächlich angesehen. Es ist deutlich kompakter und näher am alten Tabellenstil als V1. Ein kleines QR statt vier großer; 16 Aufgaben passen auf eine Seite. Die Wochen-/Fenstertrennung ist gut sichtbar.

Noch nicht abschließend visuell freigegeben. Insbesondere:

- Die gespeicherten Vorschau-PNGs werden derzeit aus `.convert('L')` gespeichert und sind daher absichtlich/versehentlich nur Graustufen. Das PDF selbst enthält Teal. Für eine faire Designbeurteilung zusätzlich RGB-Vorschauen aus den originalen `pdftoppm`-PNGs speichern und ansehen, und B/W weiterhin testen.
- Zeile `Cleaning` wiederholt sich bei jeder Single-Slot-Aufgabe ohne Mehrwert. Besser bei fehlendem Slot weglassen und Fenster/Datum vertikal zentrieren; bei Multi-Slot weiterhin zweite Zeile mit Slot behalten.
- **Datumsformat-Bug:** `window=f"{start:%a}–{end:%a} {start.day}–{end.day} {end:%b}"` unterschlägt Startmonat bei Monatswechsel. Für z.B. 28 Sep–4 Oct korrekt beide Monate anzeigen, nicht `28–4 Oct`. Auch Jahreswechsel/Ein-Tages-Fenster sauber formatieren. Manifestdaten selbst sind korrekt; nur Anzeige betroffen.
- Headerdatum ist derzeit ISO `2026-09-28 — 2026-11-20`; ein freundlicheres `28 Sep – 20 Nov 2026` wäre hübscher, ohne Datenverlust.
- Räume sind aktuell nur Text. Kleine monochrome Shower/Toilet/Utensils-Icons wie im alten Renderer könnten wieder dazu, aber nicht überladen.
- Titel ist Gruppenname mit Broom; prüfen, ob eine kleine „Cleaning plan“-Orientierung sinnvoll ist.
- Namen werden aktuell nach 28 Zeichen abgeschnitten. Who-Spalte ist 26mm breit; lange Namen umbrechen maximal sinnvoll, keine Kollision mit Kästchen. Bei sehr langen Gruppennamen/Tasknamen prüfen.
- Tabellenfooter sitzt fest bei y262ff, maximale 16 Zeilen enden y252; passt. Weniger Zeilen lassen unten Weißraum, aber der Tabellencharakter ist kompakt. Falls Notes gewünscht, nur außerhalb Scanbereiche.
- Gruppierte Wochen können bei >16 Zeilen an Seitengrenzen getrennt werden; jede Seite hat wieder Kopf und jeder erste neue Seitenblock eine Wochenbadge. Prüfen, ob bevorzugt ganze Wochenblöcke paginiert werden sollten (kein Muss, wenn lesbar).
- Aktuell vier Marker jeweils 5mm: relativ unauffällig, technisch gut erkennbar. Nicht verkleinern, ohne Foto-Tests erneut zu machen.

## Konkrete nächsten Schritte für Claude

1. Working Tree und diese Datei lesen. Bestehende V1-Dokumentation ist teilweise überholt; nicht ungeprüft übernehmen.
2. Zwei Scanner-Testfehler isolieren und sauber beheben. In Testfehlern Fallparameter/Winkel/Feld ergänzen, damit Grenzfälle nachvollziehbar sind.
3. Keine Sicherheit lockern: schlechte Aufnahme -> Vorschlag ablehnen, nie falsches Done erzwingen. Leer darf niemals vorhandene Completion löschen. Jede Übernahme weiterhin bestätigungspflichtig.
4. Datumsformat-Bug und die oben genannten wichtigsten visuellen Details beheben.
5. RGB- und B/W-Beispiele für weekly, twice-weekly und multi-slot rendern, **wirklich ansehen**. Die Wall-Aesthetic-Frage ausdrücklich beantworten und ggf. nachbessern.
6. Rust-Tests erweitern: V1-Manifeste ohne neue Felder laden; V2 stabile Revision/Feld-IDs; kein Done-Feld; korrekte Wochentags-x-Position; Field.size und Fiducials/QR stimmen; volle Woche 7+Skip und 2-Tage-Fenster 2+Skip.
7. Python Legacy-Compatibility bisher nur Identifikation: wenn möglich vollständigen alten V1-Manifest-Scan zusätzlich testen, damit alte bereits gedruckte Pläne weiter funktionieren.
8. Zusätzliche negative Marker-/Perspektivfälle sinnvoll: falsche zusätzliche Ziele, dunkler lokaler Schatten, zusammengefaltete/teilweise Seite, Verschiebung, dicke X an Rahmen. Bestätigen, dass unklare Bilder keine Änderungen erzeugen.
9. **Docker-LaTeX-Warmup aktualisieren:** `docker/tex-warmup.tex` lädt FontAwesome schon, verwendet aber neue Icons `\faPen`, `\faCamera`, `\faCalendar` noch nicht. Diese wirklich zeichnen, damit Fontpakete im Tectonic-Cache vorhanden sind. Bestehender Test in `src/pdf.rs` verlangt `warmup.starts_with(PREAMBLE)`; also nicht unnötig das Preamble ändern, neue Icon-Zeile im Body ergänzen. Gegebenenfalls gezielter Test für neue Renderer-Icons.
10. `docs/PAPER.md` und README auf V2 aktualisieren. Derzeit steht dort noch vier QR, Kreise füllen, Done+Tag, acht Aufgaben usw. Alte V1-Sheets weiterhin als Legacy erklären. Neue Runtime-Abhängigkeiten sind **nicht** nötig.
11. `cargo fmt`, volle Rust-Suite, volle Python-Suite mit echtem Rust-Manifest, clippy mit `-D warnings`, `git diff --check`.
12. Abschließend Nutzer Links zu den fertigen PDF/PNG-Beispielen geben, Verhalten/Legacy-Kompatibilität/Testresultate und Einschränkungen ehrlich nennen. Nichts deployen/committen.

## Reproduzierbare Kommandos

Im Repository `/home/nick/matrixbots/cleaning-bot`:

```sh
cargo fmt
PAPER_LAYOUT_FIXTURE=/tmp/cleaning-paper-manifest.json cargo test --offline
PAPER_LAYOUT_FIXTURE=/tmp/cleaning-paper-manifest.json OPENBLAS_NUM_THREADS=1 \
  python3 -m unittest discover -s scripts -p 'test_paper.py' -v
cargo clippy --offline --all-targets -- -D warnings
git diff --check
```

Einzelne Python-Tests z.B.:

```sh
PYTHONPATH=scripts OPENBLAS_NUM_THREADS=1 \
  python3 -m unittest test_paper.PaperTests.test_pen_width_offset_and_uneven_cross -v
```

Tests rendern ihre Fixtures jeweils neu. `setUpClass` benutzt temporäres Working Directory; im Test `__file__` nutzen, wenn Zugriff aufs Repository nötig ist.

## Umgebung / Abhängigkeiten

- Python, Pillow, NumPy, qrencode, libzbar, pdflatex, pdftoppm vorhanden.
- `fontawesome5.sty` unter `/usr/share/texmf-dist/tex/latex/fontawesome5/` vorhanden.
- `tectonic` lokal **nicht** installiert. Production Renderer bleibt Tectonic; Tests nutzen pdflatex zweimal für TikZ-Overlay-Koordinaten.
- Python-Worker importiert Legacy-Modul, das `RLIMIT_AS=1.5GiB`, `RLIMIT_CPU=45s` setzt und OPENBLAS_THREADS=1. Auch Tests erben CPU-Limit. Größere Test-Suiten ggf. Worker-Limits aus Import herausziehen und nur CLI-Worker setzen, statt echte Schutzlimits im Runtime aufzugeben.
- Keine Netzwerk-Installationen nötig. Früher scheiterte Cargo Registry-Netzwerk an DNS; `--offline` funktioniert.
- Matrix-/Unraid-Livezugriff nicht nötig und nicht ausführen.
- Keine echten Handyfotos verfügbar. Synthetische Pen-/Foto-Tests sind nützlich, aber **keine nachgewiesene Realwelt-Kalibrierung**. Nicht behaupten, dass alle realen Stifte/Beleuchtungen bereits validiert wurden.

## Geänderte/neue Dateien (bei Übergabe)

Geändert:
- `src/paper.rs`
- `scripts/paper.py`
- `scripts/test_paper.py`
- `artifacts/paper-example.pdf`, `.png`
- `artifacts/paper-full-layout.pdf`, `.png`

Neu/untracked:
- `scripts/paper_v1.py`
- `scripts/fixtures/legacy-v1.png`
- `artifacts/paper-twice-weekly.png`
- `artifacts/paper-multi-slot.png`
- `artifacts/paper-weekly.png`
- diese Übergabedatei

`paper-phone-simulation.jpg` wird erst geschrieben, wenn der derzeit fehlschlagende Perspektivtest komplett durchläuft; momentan nicht als fertiges Artefakt voraussetzen.

## Kleiner Review-Hinweis

Die aktuelle V2-Implementierung ist ein sinnvoller Zwischenstand, kein fertiges Produkt. Die Architektur/Domain-Sicherheit ist überwiegend unverändert, die neue Pixel-/Markenerkennung braucht noch die oben genannten Korrekturen. Bitte nicht einfach Tests abschwächen oder weglassen, um grün zu werden. Ziel ist ein schöner Aushang, den Menschen mit normalem Stift benutzen können und dessen unklare Fotos der Bot vorsichtig behandelt.
