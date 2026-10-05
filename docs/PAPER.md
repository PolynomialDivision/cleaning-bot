# Printed plans and photo synchronization

`!plan pdf` and `!plan pdf next` produce scannable A4 forms. `!plan pdf history`
keeps the compact, read-only history report. Nothing is automatically deployed.

## Using the sheet

Each duty has a box for each day it may be done. Put **one clear X** (two
strokes) in the day you cleaned and leave everything else blank. There is no
Skip box on paper: a skipped duty is recorded in Matrix. Any pen works; the example box in the footer shows
it. Notes are for humans and are not read.

Send a photo as a Matrix **image** to the cleaning room or an authorized
encrypted DM. Include the whole page with all four small corner targets. No
command is needed. Unrelated images and unknown document IDs are ignored.

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

1 changes · ✅ Apply · ❌ Cancel
Only you can confirm. Expires in 24 hours.
```

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
week, centred across its rows), strong rules between weeks and dotted ones
between a week's rows, columns *Week · When (· task) · Who · Mon … Sun*,
weekend columns lightly shaded, a calm teal accent and Helvetica. Everything
stays readable in black and white. Each page belongs to one group and holds up
to **21 duties**. Without a number of weeks, `!plan pdf` takes as many whole
weeks per group as fill one page. Longer plans are split evenly across pages
and only between weeks (22 rows make 12 + 10, not 21 + 1). Rows are 11 mm high
on a full page and grow up to 14 mm when a group's pages hold fewer, so the
table fills the page. The header is one line: title, then dates and rooms
beside it. Each slot and each shift of a week is its own row: a
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
| boxes | 4.8 mm squares on the row centre: Monday at x = 111.5, then every 13 mm (the days share the width right of *Who*) |

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
5. Find exactly four corner targets (nested square / white disc / dot; QR
   finder patterns have different proportions). The QR near the bottom-right
   target fixes the orientation; its position must match the manifest after
   the homography. Normalize perspective to A4 at 6 pixels/mm. Reject too
   small or too steep photos.
6. Read each box the manifest names. Its printed outline is registered locally
   (up to ±2 px, sub-pixel centre) and must be continuous on all four sides,
   with a clean background around it — otherwise the whole scan is rejected
   (shadow, fold, missing outline). Inside, ignoring the outline: almost no
   ink is blank; an **X** needs ink running along all four diagonal arms
   (measured as continuity, so thin and thick pens count alike), and no
   circle around its crossing may pass more than four pieces of ink (extra
   strokes). Ticks, slashes, dots, filled boxes, grids, crossed-out marks and
   scribbles are ambiguous and reject the scan. More than one X per duty
   rejects it too. Blank boxes never clear existing state.
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
PAPER_LAYOUT_FIXTURE=/tmp/cleaning-paper-manifest.json cargo test --offline
PAPER_LAYOUT_FIXTURE=/tmp/cleaning-paper-manifest.json OPENBLAS_NUM_THREADS=1 \
  python3 -m unittest discover -s scripts -p 'test_paper.py' -v
cargo clippy --offline --all-targets -- -D warnings
```

Python integration tests need `pdflatex`, `pdftoppm`, plus the runtime packages.
`PAPER_ENGINE=tectonic` renders with Tectonic instead, as production does
(its package cache must be warm: compile `docker/tex-warmup.tex` once).
They render real PDFs, rasterize them, draw X marks with separate pen strokes
(no fills) and scan them back:

- X marks with pens 0.17–0.67 mm, grey to black, off-centre, small to large,
  uneven — all read; ticks, slashes, dots, fills, circles, plus signs, grids,
  three-armed marks, crossed-out X and scribbles (thin and medium pens) never;
- rotation 0/90/180/270/13°, perspective, JPEG, darker exposure;
- empty pages, the second shift of a week, weekend days, two marks for one duty, shadows over a box,
  a buckled band, a cropped or blurred photo, missing targets, an unrelated
  image, an old revision, two pages in one photo;
- a complete version-1 sheet (still scannable);
- with `PAPER_LAYOUT_FIXTURE`, a round trip through the actual Rust-generated
  multi-slot/twice-weekly manifest, including its geometry.

`artifacts/` (not in git; the tests write it on every run) holds synthetic,
anonymous previews, not the house's live plan:
`paper-{weekly,twice-weekly,multi-slot}.png` in colour, `*-bw.png` in greyscale
(what the scanner and a black-and-white printer see), `paper-full-layout.*` from
the Rust manifest, `paper-phone-simulation.jpg`.

## Deliberate limitations

- Ordinary full-page JPEG/PNG photos are the target. Images sent as `m.file`,
  handwriting, OCR, HEIC-specific decoding and multiple-page extraction are not
  implemented. All four corner targets must be visible.
- The X reader is a set of conservative geometric checks, not a calibrated
  model, and it was tuned on synthetic pen strokes only — **no real phone
  photos or real pens**. Expect some real marks to be rejected as unclear (then
  retake the photo or record it in Matrix).
- Known gap: a *loose* scribble with a thick marker (0.5 mm and more) can look
  like an X in a 4.8 mm box. Dense scribbles count as filled boxes. Every scan
  is a preview the uploader must confirm, which is where such a misread shows.
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
