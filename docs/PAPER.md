# Printed plans and photo synchronization

`!plan pdf` and `!plan pdf next` produce scannable A4 forms. `!plan pdf history`
keeps the compact, read-only history report. Nothing is automatically deployed.

## Two styles

An administrator chooses which sheet `!plan pdf` prints, for every group or
for one; it is kept in the bot state (`paper_style` for everyone,
`paper_styles` per group), and anyone can see what each group prints with
`!plan pdf style`:

```
!plan pdf style days            · everyone: a box for each day, the day is recorded (default)
!plan pdf style tick            · everyone: one box per duty, slots side by side; done or not
!plan pdf style Kitchen tick    · only Kitchen (multi-word names need no quotes)
!plan pdf style Kitchen default · Kitchen prints like everyone again
```

A group's own style wins over everyone's, and goes when the group is
deleted. Each page of the manifest records its style, so one PDF can hold a
days page for one group and a tick page for another, and sheets printed
before a switch keep scanning as what they are.

**Tick** sheets have one line per week. The group's slots and shifts stand
side by side as columns (for a group with two slots cleaned twice a week:
*Stairs Mon–Tue · Hall Mon–Tue · Stairs Thu–Fri · Hall Thu–Fri*), each cell
the name and one box. A page holds 21 weeks; without a number of weeks
`!plan pdf` prints 21. A tick has no day, but a completion is recorded with
one (statistics, history, "done on time"). The bot dates it in the middle of
the duty's days — `start + (end − start) / 2`, rounded down: Thursday for
Mon–Sun, Monday for Mon–Tue, Thursday for Thu–Fri — or the day the photo is
processed if that is earlier, never before the first day. A tick photographed
before the duty's days start is refused like a day in the future. The
preview says it: `Done (counted as Thu 8 Oct)`. A duty already recorded as
done, on whatever day (by `!done`, ✅ or an earlier photo), is no change for a
tick; on a days sheet a different day is a conflict.

## Plans only to look at

`!plan pdf view [next] [N] [group]` prints the same pages as `!plan pdf`
(each group in its style, as many weeks) with nothing to tick: no boxes, no
QR code, no corner targets. On a days page a solid bar runs across the days
a duty may be done instead (nothing that looks like a box to tick). The header says "CLEANING PLAN · VIEW ONLY, NOT FOR
TICKING", and a framed note at the bottom says it is not the sheet to tick
and to use the plan with boxes or `!done`. Its manifest has `view_only` and
is not stored: a photo of a view has no QR code to identify and is ignored.

## Using the sheet

On a days sheet each duty has a box for each day it may be done; on a tick
sheet one box. The sheet asks for **one clear X** in the day you cleaned (or
in your box), but any clear mark through the box counts: an X, a tick, a
stroke. A box **filled in** or scribbled over counts as taken back: it is
not counted, and the preview names it (so a correction is simply: fill in
the wrong box, mark the right one). A dot is too little to count. There is
no Skip box on paper: a skipped duty is recorded in Matrix. Any pen works;
the example box in the footer shows it. Notes are for humans and are not
read.

Send a photo as a Matrix **image** to the cleaning room or an authorized
encrypted DM. Include the whole page with all four small corner targets. No
command is needed. Unrelated images are ignored. A photo that clearly shows a
sheet (three or four corner targets) but whose code can't be read gets an
answer saying how to retake it, and so does a sheet the bot has no record of.

Sheets printed with the first layout (four corner QR codes, circles to fill)
still scan as before.

The bot sends one preview, with ✅ Apply and ❌ Cancel reaction buttons. Only the
uploader can confirm; confirmation expires after 24 hours. The preview is edited
into the result, with empty `m.mentions`. User reactions are removed when the bot
has permission. Without redaction permission, the terminal confirmation remains
idempotent; removing a reaction never undoes a paper completion.

Example:

```
📷 Cleaning sheet recognized

✅ Week 41 · Alice · Kitchen Mon–Tue 5 – 6 Oct
Done 2026-10-06

Not read:
❓ Week 43 · Bob · Kitchen Mon–Tue — box not clearly visible (fold, blur)
Record these with !done, or send a clearer photo.

Filled in, so taken back (not counted):
✏️ Week 42 · Carol · Kitchen Thu–Fri · Thu
Meant as done? Record it with !done.

1 changes · ✅ Apply · ❌ Cancel
Only you can confirm. Expires in 24 hours.
```

A duty whose box can't be read (a fold or shadow over it, only a dot, or
several days marked) doesn't hold up the others: it is named under "Not
read", and the rest of the page is applied as usual. Only when more than a
quarter of a page's boxes can't be read (a crumpled sheet, a blurred or very
slanted photo) is the photo refused as a whole, asking for it to be smoothed
out and taken again: on such photos creases look like marks.

Residents can complete their own assigned duties. Administrators can record
other people's assigned duties (and, on old version-1 sheets, mark duties
skipped). A page containing any
unauthorized proposed change is rejected as a whole. For a wall sheet marked by
several residents, an administrator should upload and confirm it.

## Layout and identity

`ScheduleSnapshot -> paper::Document -> fixed PDF layout` is the export path.
The snapshot is the same domain projection used by Matrix and iCal. History PDFs
still use the existing compact renderer over that snapshot.

The sheet is a table in the style of the history PDF: week badges (one per
week, centred across its rows), strong rules between weeks and dashed ones
between a week's rows, columns *Week · When (· task) · Who · Mon … Sun*, and
Helvetica. It is printer-friendly: everything is solid black on white
(DeviceGray 0 — no colour, no grey, no shading; a test checks the PDF), so a
cheap or low-toner printer prints it crisply instead of as a pale dot screen.
Lines and text are told apart by weight and size. Each page belongs to one group and holds up
to **21 duties**. Without a number of weeks, `!plan pdf` takes as many whole
weeks per group as fill one page. Longer plans are split evenly across pages
and only between weeks (22 rows make 12 + 10, not 21 + 1). Rows are 11 mm high
on a full page and grow up to 14 mm when a group's pages hold fewer, so the
table fills the page. The header is one line: title, then dates and rooms
beside it — the rooms per slot, with a symbol for toilets, showers and
kitchens instead of the word (`Colbe Toilet 3rd` in slot Colbe reads
"Colbe: [toilet] 3rd"); rooms of no known kind keep their name. Each slot and each shift of a week is its own row: a
twice-weekly group shows two rows per week (`Mon–Tue · 28–29 Sep`,
`Thu–Fri · 1–2 Oct`), a group with slots shows the slot in bold above the
dates. Only the days a duty may be done get a box: seven for a weekly duty,
two for a two-day window. Completed or skipped duties show
their status as text instead of boxes. Long names wrap between words and
after hyphens, get smaller when long, and a word too wide for the column is
scaled down: nothing crosses a column line. Row dates leave out the year; the
header shows it.

The TeX source is pure ASCII (special characters as LaTeX commands, the same
table as the history PDF): production compiles with Tectonic (XeTeX), where a
raw `·` printed as `ů` and a raw `–` vanished, and an embedded QR image came
out as bare outlines. A test checks both.

Layout version 2, in A4 millimetres from the top left (`src/paper.rs` and
`scripts/paper.py` agree; tests check it):

| | |
|---|---|
| corner targets | `(10,10)`, `(200,10)`, `(200,287)`, `(10,287)` — 5 mm black square, white disc, black dot; no data |
| identity QR | one, centred at `(185,276)`, 20 mm including its quiet zone, drawn as vector squares (no image); upper-case payload, so QR's compact alphanumeric mode gives 29×29 modules of 0.54 mm |
| rows | from 31 mm, 11–14 mm each (per group, see above); centres in the manifest |
| boxes | 6 mm squares with a 0.4 mm outline on the row centre (sheets printed before: 4.8 mm, 0.22 mm — each field's size is in the manifest, and the scanner reads either): Monday at x = 111.5, then every 13 mm (the days share the width right of *Who*) |
| tick sheets | a line per week, same rows and heights; *Week* 14–27, *When* 27–52, the columns share 52–196 evenly; one 4.8 mm box per duty, 4.5 mm left of its column's right edge (field kind `done`, ID `<row>:done`) |

The QR contains only:

```
CB2:<random 128-bit document id>:<12-hex layout revision>:<page number>
```

No names, database IDs, Matrix IDs, calendar tokens or URLs are encoded. The
random ID is an identifier, not an authorization credential. The manifest stays
in bot state and records group, slot, year/week, shift, assignee, window,
assignment baseline, the exact box centres and size, and stable field IDs. Row
IDs hash the duty identity independently of the assignee; field IDs append
the ISO date. The revision hashes the layout version and the complete
page manifests. Identical state produces identical manifests/revisions; new
PDFs deliberately receive new random document IDs. Old manifests without the
new fields load as layout version 1. The form does not change group frequency.

## Recognition pipeline

1. Fetch fresh membership state and authorize the sender/room.
2. Download/decrypt using the Matrix SDK. Reject advertised or received sizes over
   12 MB; use a 20-second download timeout.
3. Run an isolated Python worker; decode only QR symbols through ZBar. The worker
   accepts at most 24 megapixels, normalizes EXIF orientation and limits working
   image size. No photo is sent to an external service.
4. Resolve the opaque identity against persisted manifests (version-1 sheets
   go to the unchanged version-1 reader). If the QR can't be read directly —
   small, tilted, compressed — straighten the page on its four corner targets
   (which don't depend on rotation), try the four orientations and read the
   QR from the straightened corner.
   If the code or a corner target can't be found, the same is tried once
   more with the light evened out (each pixel divided by the paper's
   brightness around it): a shadow over a corner makes the paper there darker
   than the fixed thresholds' "dark". Photos that read as they are never get
   this step.
5. Find the four corner targets (nested square / white disc / dot; QR
   finder patterns have different proportions), at three levels of
   strictness, in the photo and with its light evened out: in a small,
   compressed or dim photo a 5 mm target's white disc and dot blur. The QR
   code, at a known place near the bottom-right target, picks the four that
   belong to this page (a neighbouring sheet in the photo has targets too)
   and fixes the orientation. If only three are found (one in a shadow), the
   fourth follows from them and the QR code. Normalize perspective to A4 at
   6 pixels/mm (from the evened-out photo). Reject too small or too steep
   photos.
6. Read each box the manifest names. A page that isn't flat (curled, wavy
   where it hangs) moves boxes against the corners by 2–3 mm, so each box is
   first looked for within 3 mm of its place, where all four sides of a
   box-sized square are dark (a rule or a column line alone never passes;
   boxes are 11 mm and more apart). The printed outline is found with a
   lighter threshold than ink: in a shrunk, compressed photo the 0.22 mm line
   of older sheets is only a pixel or two of grey. Then it is registered
   precisely (up to ±2 px, sub-pixel centre) and must be continuous on all
   four sides, with clean paper around it — otherwise that duty is unclear
   (shadow, fold, missing outline). Inside, away from the outline, the share
   of ink decides: under 0.8 % is blank; ink through the box's middle,
   spanning at least 40 % of it, is a mark (an X, a tick, a stroke); more
   than 55 %, or ink all across the box (three quarters of its 4×4 patches,
   where an X leaves the triangles between its arms free) is filled in —
   taken back; a speck is unclear. One marked day per duty; several make it
   unclear. Blank boxes never clear existing state.
7. Build a persisted proposal. Recheck assignment identities, edit history,
   permissions, date windows and existing completion state.
8. On confirmation, repeat authorization and all domain checks under the normal
   operation lock. Apply `CleaningCompleted` / `CleaningSkipped` to a staged
   state, then save atomically with the proposal result. Refresh the pinned plan.

The worker has a 1.5 GiB address-space limit, 45-second CPU limit and a 60-second
parent timeout. Photos and generated worker files live only in temporary local
directories. Images are not placed in the SDK media cache by this feature.
The SDK still buffers downloads: the received-size check happens after download,
so it is not a hard network-stream allocation bound against a lying server.

## Conflicts, restart and persistence

A current resolved assignment must match its printed baseline. Explicit changes
away and back are also detected through assignment event history. Routine first
materialization of unchanged round-robin assignments does not invalidate a sheet.
Group configuration changes can conservatively invalidate its sheets. A digital
undo invalidates the old row, so paper cannot silently restore it.

An identical completion already recorded is a no-op; a different date or status
is a conflict. No completion is overwritten. Future completion dates are rejected.
The whole proposal is validated before any event application; a conflict changes
nothing. There is no automatic-apply mode.

The image event ID and a stable Matrix transaction ID prevent duplicate previews
when the same event is replayed. Equivalent pending/applied proposals from the
same person/room are suppressed for 24 hours. Domain idempotency protects repeated
photos even after that. Proposals and terminal outcomes survive restart. If a
crash interrupts the final Matrix edit, tapping the preview again repairs its
result without applying twice.

New `paper_documents`, `paper_scans` and optional `completed_on` fields use serde
defaults. No offline migration is required. Old completions keep their previous
date behavior. `completed_at` remains the actual recording timestamp;
`completed_on` stores a paper-selected date, which the shared schedule exposes to
Matrix/PDF/iCal. Event-log backfill preserves that date. Manifests expire after
400 days, proposals after 7 days, during normal state saves; confirmations expire
at 24 hours. Unknown/expired document IDs are ignored. No live state was migrated.

DM processing reuses the existing fresh-state boundary: the sender and bot must
be joined to the trusted room; the private room must be encrypted and contain
exactly those two joined members, without pending third-party invitations. Main
room image processing also checks fresh joined membership. QR knowledge alone
cannot authorize anything. Membership is checked again at confirmation.

## Dependencies and validation

No new Rust crates. Runtime packages added to the Dockerfile:
`python3`, `python3-pil`, `python3-numpy`, `libzbar0`, `qrencode`.
The worker is embedded in the Rust binary. Tectonic remains the production PDF
engine. The LaTeX warmup draws the plan's icons too (a test compares them), so
Tectonic has their fonts cached.
The ZBar location API supplies the QR polygons:
https://zbar.sourceforge.net/api/zbar_8h.html

Run:

```sh
export PAPER_LAYOUT_FIXTURE=/tmp/cleaning-paper-manifest.json
export PAPER_TICK_FIXTURE=/tmp/cleaning-paper-tick-manifest.json
export PAPER_VIEW_FIXTURE=/tmp/cleaning-paper-view-manifest.json
cargo test --offline
OPENBLAS_NUM_THREADS=1 python3 -m unittest discover -s scripts -p 'test_paper.py' -v
cargo clippy --offline --all-targets -- -D warnings
```

Python integration tests need `pdflatex`, `pdftoppm`, plus the runtime packages.
`PAPER_ENGINE=tectonic` renders with Tectonic instead, as production does
(its package cache must be warm: compile `docker/tex-warmup.tex` once).
They render real PDFs, rasterize them, draw marks with separate pen strokes
and scan them back:

- X marks with pens 0.17–0.5 mm, grey to black, off-centre, small to large,
  uneven — all read as done, and on 6 mm boxes up to a 0.67 mm marker,
  photographed tilted and compressed; ticks, slashes, circles and plus signs
  read as done too; a filled-in or densely scribbled box as taken back (and
  named), a correction (one day filled in, another marked) as the marked
  day; a dot and several marked days as unclear;
- rotation 0/90/180/270/13°, perspective, JPEG, darker exposure;
- empty pages, the second shift of a week, weekend days, a shadow over a
  box (that duty unclear, the rest read), a band buckled further than a box
  is looked for (those rows unclear), creases all over a page (refused as a
  whole), a cropped or blurred photo, a lost corner target (read from three
  and the QR code), an unrelated image, an old revision, two pages in one
  photo;
- a wavy, curled page with a shadow over the QR corner, shrunk to 1200×1600
  and compressed the way chat apps send photos (read correctly); a sheet with
  corner targets but no readable code (answered, not ignored);
- a complete version-1 sheet (still scannable);
- views of both styles: the notice on every page, no QR code or corner
  targets, a photo ignored (with `PAPER_VIEW_FIXTURE` also the Rust one);
- a tick sheet: empty, ticks side by side in one week and across the page
  (read without a day), photographed, a tick counted;
- with `PAPER_LAYOUT_FIXTURE` and `PAPER_TICK_FIXTURE`, round trips through
  the actual Rust-generated multi-slot/twice-weekly manifests in both styles.

`artifacts/` (not in git; the tests write it on every run) holds synthetic,
anonymous previews, not the house's live plan:
`paper-{weekly,twice-weekly,multi-slot}.png` in colour, `*-bw.png` in greyscale
(what the scanner and a black-and-white printer see), `paper-tick.*`,
`paper-full-layout.*` and `paper-tick-full-layout.pdf` from the Rust
manifests, `paper-phone-simulation.jpg`.

## Deliberate limitations

- Ordinary full-page JPEG/PNG photos are the target. Images sent as `m.file`,
  handwriting, OCR, HEIC-specific decoding and multiple-page extraction are not
  implemented. Three corner targets and the QR code must be visible.
- The mark reader is a set of simple checks, not a calibrated model. It was
  checked on five real phone photos, sent compressed by a chat app
  (1200×1600): on a flat sheet every real mark (ballpoint and felt pen, small,
  big, over the edge, with a tail) read correctly and every empty box as
  empty; crumpled sheets and a very slanted photo with a corner target in a
  shadow are refused as a whole (on those, creases read as marks); no mark
  was ever applied to the wrong duty or day.
- On sheets printed with 4.8 mm boxes, a big X with a thick pen (0.5 mm and
  more), blurred by a tilted photo, can fill the small box and read as taken
  back. It is named in the preview, never dropped silently. New sheets have
  6 mm boxes, where it reads as done. A loose scribble reads as done or as
  taken back. Every scan is a preview the uploader must confirm, which is
  where such a misread shows.
- Detection rejects mixed decoded page identities. It cannot prove that no second
  page is present when that page's codes are unreadable. Only the fully recognized
  page can supply proposed changes.
- Names longer than 60 characters are cut after a whole word on paper; the
  full identity remains in the manifest. Characters without a glyph in the
  PDF fonts (emoji, non-Latin scripts) are left out on paper. Existing LaTeX font coverage still limits unusual Unicode names.
- Rendering was tested with both pdflatex and Tectonic (the full suite passes
  with either). The container build itself, Matrix homeserver and phone-camera
  integration were not exercised. No claim of real-camera calibration is made.
- Digital history reports and older PDFs have no document markers and are not
  scannable. Generate a new current/upcoming form to use photo synchronization.
