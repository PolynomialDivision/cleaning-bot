# cleaning-bot

A cleaning rota for a shared house, in the Matrix room you already use. It
knows whose turn it is, posts and pins the week's plan, reminds people, and
keeps track of who actually cleaned.

## Using it (for housemates)

```
📅 !next · your next turn
📋 !plan · see the plan
👥 !mygroups · join or leave groups
✅ !done · done! (or ✅ on the plan)
🔄 !swap @user · ask for cover
🗓 !ical · your private calendar
📄 !plan pdf · printable plan
```

`!help more` lists the rest (`!status`, `!undo`, `!takeover`, `!join`,
`!leave`, `!groups`, `!stats`, `!plan pdf history`, `!ical reset`).

**The weekly plan** is posted and pinned on the configured weekday (later
that week if the bot was down then). React ✅ on it when your part is done;
take the ✅ back to undo. It is edited in place as things change; an edit
only notifies people who are new on it (after a takeover, say).

Of the bot's own messages only the newest plan stays pinned: posting a plan
unpins every earlier plan or reminder, of any age, and so does every start
of the bot. Messages pinned by people stay pinned.

**Reminders** are short and only ping who is still open:

```
⏰ Still open, ends today: @bob (Bath · Thu–Fri) · Dan (Kitchen)
✅ alice (2nd Floor)
React ✅ here or on the plan when it's done.
```

A ✅ on the reminder counts the same as one on the plan. The reminder is
edited as people finish (without pinging anyone again), until it reads
"✨ All done — thanks!". A shift starting mid-week gets a "🔔 Your turn
starts today" one; `!plan remind` (admin) sends a "Still open this week".

**Welcome and groups.** Everyone gets one welcome, ever — when they join the
room, or with their first command or reaction there, also if an admin added
them before. The welcome is a group selector:

```
👋 Welcome, @mia!
🧹 I'm the cleaning bot: I keep track of whose turn it is and remind you when it's yours.

Tap a number to join or leave a group:
1️⃣ ✅ **2nd Floor**
2️⃣ Kitchen
✅ Joined **2nd Floor**

📅 Next: 2nd Floor · 5 – 11 Oct
🗓 Your calendar: send !ical to @bot in a private chat
!mygroups reopens this · !join / !leave · !help
```

Tapping a number toggles that group. The bot edits the selector — no extra
message — and, once the tap is saved, removes your reaction so the same
number can be tapped again (that is what "tap again to leave" means). If the
bot lacks the power level to remove other people's reactions, your reaction
stays; removing it yourself then undoes that tap. `!mygroups` posts a fresh
selector; `!join`/`!leave` work without buttons. More than ten groups: the
rest via `!groups` and `!join`.

**A private chat with the bot** (encrypted) also takes `!next`, `!plan`,
`!plan pdf`, `!mygroups`, `!join`, `!leave`, `!done`, `!undo`, `!help` and
`!ical` — only about yourself, never about others, and never admin commands
(admins have their own admin DM). See *Private chats* below for what it
takes.

## Running it (for admins)

Copy `config.example.toml`, fill in the Matrix account and the room, start
the bot (see `Dockerfile`; PDFs need `tectonic`, which the image includes),
then set up groups in the room:

```
!groups add Kitchen
!member add @mia:example.org Kitchen      (or a plain name for someone without Matrix)
!groups rhythm Kitchen 2x
```

`!help admin` lists everything. `!validate` checks the saved state.

### Rhythms

| `!groups rhythm <group> …` | means |
|---|---|
| `weekly` | one turn per week (Mon–Sun) |
| `2x` | two turns per week: Mon–Tue and Thu–Fri; Wednesday and the weekend are free |
| `mon-tue thu-fri` | explicit windows: no overlaps, no wrapping into next week, the first starting Monday |
| `mon thu` | back-to-back shifts starting on those days: Mon–Wed, Thu–Sun |
| `every 2` | every second week, counted from when tracking started |
| `daily` | one turn per day |

Parts combine with the rhythm set last (`every 2` keeps `2x`); start afresh
with `weekly …`.

- **A change applies from next week.** This and earlier weeks keep their
  days; the old versions are stored with the group. A change set for next
  week can still be replaced before it starts. `!groups <group>` shows both:
  `weekly · from week 42: 2× per week (Mon–Tue, Thu–Fri)`.
- Future turns from the rotation are re-planned, and the rotation rewinds so
  nobody loses or gains a turn. Turns that were assigned by hand, imported,
  taken over, swapped or already done are never moved silently: a change that
  would move one is refused — change or remove that turn first.
- Groups set up before windows existed (`2x` used to mean Mon–Wed + Thu–Sun)
  keep exactly that until an admin sets a new rhythm.
- A window says when a turn is *due*; nothing enforces when it is actually
  cleaned. A late Mon–Tue cleaning may land right before the Thu–Fri one.

### Too few people

The rotation gives one person at most one automatic duty per group and week.
When a group has more duties per week (slots × shifts) than members, the rest
stay *nobody assigned* — visible in the plan, fillable with `!plan assign`.
`!validate` warns about such groups.

### Private chats

For a resident, a room other than the cleaning room counts as a private chat
only if, according to the server's current state (fetched for every command,
reaction and redaction there):

1. the bot is in the cleaning room, and the resident and the bot are both
   joined members of it;
2. the private room has exactly two members — the bot and the resident — and
   nobody else invited or knocking;
3. the private room is end-to-end encrypted (Megolm).

Anything else, or any error fetching the state, means no access. (The check
runs right before each answer; someone joining in the moment between check
and answer isn't caught.) Display
names and `m.direct` never decide this (`m.direct` is only used to find an
existing chat for a welcome). The bot only joins a chat it is invited to if
the inviter is allowed by `security.allowed_inviters` — so residents who
should use it privately need to be listed there (or `"all"`). Changes made
from a private chat update the plan in the cleaning room.

### Calendar feeds (`!ical`)

With `[ical_server]` configured, `!ical` — in a private chat only — gives a
personal subscription URL (`https://…/ical/<token>.ics`); without it, a .ics
file. In the cleaning room, `!ical` only says to ask privately; a private
welcome shows the link directly.

- Tokens are 256 bits of randomness. The server checks the exact token
  format, that the token isn't revoked, and that its person is active and
  linked to Matrix. It answers with an ETag (and 304 for `If-None-Match`)
  and `Cache-Control: private`; tokens are never logged.
- **The tokens are stored in `state.json`** (with their hash) so `!ical` can
  show the same link again. Treat `state.json` and its backups as secrets,
  serve the feed over HTTPS only (the bot warns at startup otherwise), and
  keep `/ical/` URLs out of reverse-proxy access logs.
- `!ical reset` replaces your link (the old one stops working); `!ical
  revoke` switches it off; admins can `!ical revoke <person>` without ever
  seeing the link.
- A feed keeps working after someone leaves the room — the server doesn't
  check room membership live. Revoke it, or remove the person.
- A feed belongs to a person. When `!member link` joins a name to a Matrix
  user who already had a feed, the feed moves along.
- Events carry `STATUS:CONFIRMED` (or `CANCELLED` for a skipped turn), the
  task and completion in the description, and `LAST-MODIFIED`. Calendar
  apps poll on their own schedule; changes can take hours to show.

### PDF

`!plan pdf [N] [group]` renders N weeks from this one (default 8); `!plan pdf
next [N] [group]` N weeks from next week on; `!plan pdf history [N] [group]`
the last N weeks up to this one. Weeks alternate white and shaded and never
break across pages; a slot's rooms are listed once under the title, with
icons. It is made to be printed, in black and white too: open turns have a
box to tick by hand, done ones a tick and the date. Both show the time
windows, who was responsible, who actually cleaned if that was someone else,
the date it was done, and whether a turn was assigned, imported, taken over
or swapped. A skipped turn shows `--`, not a tick. Disabled groups are left
out. Rendering runs `tectonic` with a 60-second limit; the Docker image fills
tectonic's cache at build time from `docker/tex-warmup.tex`, which must use
everything the renderer does (a test checks).

The PDF fonts cover Latin-1 (German, French, Scandinavian, …) and common
punctuation. Other characters — Polish ł, Cyrillic, Greek, emoji — are
missing from the PDF.

## Reliability, honestly

- **Commands** are remembered by event ID (the last 2000): a message the
  server delivers again after a restart doesn't run twice. A command counts
  as run with its first saved change, so one that failed before changing
  anything may run again. Replies are sent after saving; a crash in between
  loses the reply, not the change.
- **The weekly plan, reminders and welcomes** are sent with fixed Matrix
  transaction IDs, so a retry after a crash returns the message already sent
  instead of posting it twice. That holds only while the homeserver remembers
  the transaction for the same device; after a new login or restoring an old
  `state.json`, a duplicate is possible.
- **Taps and ✅ reactions** count once per reaction event, also across
  restarts. A redaction that arrives before its reaction still wins. Taking a
  ✅ back never undoes a done mark made later by other means (for ✅ given
  before this version, a later mark by the same person can still go with it).
- Commands, reactions and the scheduler run one at a time, so edits are
  never based on stale state. A slow PDF render delays other commands for up
  to a minute.

There is no transactional outbox: the bot does not promise exactly-once
delivery of every message.

## Upgrading

All new state fields have defaults, so an existing `state.json` loads as is:

- group rhythms gain windows (`shift_ends`), a start date and earlier
  versions — old rhythms keep their meaning;
- old calendar tokens (hash only) keep working; `!ical` issues a new link
  that can be shown again, and the old URL still works until `!ical reset`;
- selectors without a room belong to the cleaning room;
- bookkeeping for commands, redactions and welcomes starts empty.

The calendar is now named "Cleaning – <name>" instead of "Putzplan – <name>";
event UIDs are unchanged, so subscriptions just update.

**Rolling back** to an older version is not safe once rhythms were changed:
it ignores the new fields and would treat `2x` windows as Mon–Wed/Thu–Sun and
forget earlier rhythm versions.

`cargo run --bin state_debug -- state.json` prints a quick report of a state
file, but its schedule is a simple guess that ignores rhythms and slot
assignments — use `!plan` and `!validate` for the real thing.
