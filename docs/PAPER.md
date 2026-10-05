# Printed plans and photo synchronization

`!plan pdf` and `!plan pdf next` produce scannable A4 forms. `!plan pdf history`
keeps the compact, read-only history report. Nothing is automatically deployed.

## Using the sheet

Fill the **Done** circle and exactly one weekday circle, or **Skipped** alone.
Use a dark pen and fill circles solidly. Notes are for humans and are not read.
Send a photo as a Matrix **image** to the cleaning room or an authorized encrypted
DM. Include the entire page and all four corner codes. No command is needed.
Unrelated images and unknown document IDs are ignored.

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
other people's assigned duties and mark duties skipped. A page containing any
unauthorized proposed change is rejected as a whole. For a wall sheet marked by
several residents, an administrator should upload and confirm it.

## Layout and identity

`ScheduleSnapshot -> paper::Document -> fixed PDF layout` is the export path.
The snapshot is the same domain projection used by Matrix and iCal. History PDFs
still use the existing compact renderer over that snapshot.

Each page belongs to one group and contains up to eight duties. Each slot and
sub-week window is a separate row, with its week/year, responsible person, dates,
slot, status fields and notes line. Completed/skipped duties have printed status
instead of new input fields. Imported/manual assignments retain their resolved
assignee and a source label. Four 22 mm QR codes sit inside the A4 print margins;
their quiet zones are included. All information survives monochrome printing.

Each QR contains only:

```
CB1:<random 128-bit document id>:<12-hex layout revision>:<page number>:<corner 0..3>
```

No names, database IDs, Matrix IDs, calendar tokens or URLs are encoded. The
random ID is an identifier, not an authorization credential. The manifest stays
in bot state and records group, slot, year/week, shift, assignee, window, assignment
baseline, exact field centres in millimetres, and stable field IDs. Row IDs hash
the duty identity independently of the assignee. Field IDs append the status or
ISO date to that row ID. The revision hashes the complete page manifests.
Identical state produces identical manifests/revisions; new PDFs deliberately
receive new random document IDs. PDF bytes need not be identical.

Coordinates are A4 millimetres from the top-left. QR centres are `(19,19)`,
`(191,19)`, `(191,278)`, `(19,278)`. Input fields are 3.2 mm circles. A weekly duty
has seven day circles; a Mon–Tue / Thu–Fri rhythm has only its two permitted days
per row. Legacy Mon–Wed / Thu–Sun windows are preserved exactly as scheduled.
The form does not change group frequency.

## Recognition pipeline

1. Fetch fresh membership state and authorize the sender/room.
2. Download/decrypt using the Matrix SDK. Reject advertised or received sizes over
   12 MB; use a 20-second download timeout.
3. Run an isolated Python worker; decode only QR symbols through ZBar. The worker
   accepts at most 24 megapixels, normalizes EXIF orientation and limits working
   image size. No photo is sent to an external service.
4. Resolve the opaque identity against persisted manifests. Decode all four
   matching corner codes. Reject incomplete pages, mixed page identities and
   extreme/too-small geometry. An additional half-turn decode recovers occasional
   orientation-dependent QR misses.
5. Calculate a homography from QR centres and normalize perspective to A4 at
   5 pixels/mm. QR centres use diagonal intersections, not vertex averages.
6. Inspect field interiors against local background. Check printed outlines and
   surrounding whitespace as registration/quality checks. Intermediate fill
   density, shadows, missing outlines, multiple days and incompatible statuses
   reject the entire scan. Blank fields never clear existing state.
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
engine. `graphicx` was added to the LaTeX warmup; existing TikZ/fonts are reused.
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
They render real PDFs, rasterize them, fill circles, apply synthetic perspective
and rotations, and scan them back. The environment variable adds a cross-language
round trip using the actual Rust-generated multi-slot/twice-weekly manifest.
`artifacts/paper-example.*` and `artifacts/paper-full-layout.*` are synthetic test
previews, not the house's live plan.

## Deliberate limitations

- Ordinary full-page JPEG/PNG photos are the target. Images sent as `m.file`,
  handwriting, OCR, HEIC-specific decoding and multiple-page extraction are not
  implemented. Four readable corner codes are required.
- Fill bubbles solidly. Light ticks/scribbles may require a clearer mark/photo.
  Confidence thresholds are conservative heuristics, not calibrated probabilities.
- Detection rejects mixed decoded page identities. It cannot prove that no second
  page is present when that page's codes are unreadable. Only the fully recognized
  page can supply proposed changes.
- Names are shortened on paper to keep fixed rows readable; full identity remains
  in the manifest. Existing LaTeX font coverage still limits unusual Unicode names.
- This was tested offline with pdflatex (Tectonic is unavailable on the development
  host). Production Tectonic/container rendering and Matrix homeserver/phone-camera
  integration were not exercised. No claim of real-camera calibration is made.
- Digital history reports and older PDFs have no document markers and are not
  scannable. Generate a new current/upcoming form to use photo synchronization.
