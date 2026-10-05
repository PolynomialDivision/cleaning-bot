# cleaning-bot

A cleaning rota for a shared house, in the Matrix room you already use. It
knows whose turn it is, posts and pins the week's plan, reminds people, and
keeps track of who actually cleaned.

## Using it (for housemates)

```
📅 !next · your turns — swap one or 🆘 from there
📋 !plan · see the plan
👥 !mygroups · join or leave groups
✅ !done · done! (or ✅ on the plan)
🆘 !sos · can't make it? find cover (or 🆘 on the plan)
🗓 !ical · your private calendar
📄 !plan pdf · printable plan
```

The bot answers a command as a reply to it — in the thread, if you wrote
it in one.

**The help board.** An admin can post a friendly overview with `!help post`.
Its reactions are buttons — 📅 your next turns (as the menu below), 📋 the
plan, 👥 your groups, 📄 a PDF, 💬 a private chat: the bot answers privately if you
have a private chat with it, else as a reply to the board, addressed to
you, and then takes the reaction away so the button stays clean. 💬 makes
the bot invite you to a new encrypted chat (no invite allowlist needed for
that); once you join, it greets you there with your groups and calendar
link.

`!help more` lists the rest (`!status`, `!undo`, `!takeover`, `!swap`,
`!join`, `!leave`, `!groups`, `!stats`, `!plan pdf history`, `!ical reset`).

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
React ✅ here or on the plan when it's done · 🆘 if you can't make it.
```

A ✅ on the reminder counts the same as one on the plan. The reminder is
edited as people finish (without pinging anyone again), until it reads
"✨ All done — thanks!". A shift starting mid-week gets a "🔔 Your turn
starts today" one; `!plan remind` (admin) sends a "Still open this week".

**Your turns, as a menu.** `!next` (or `!next N`, up to 10) and 📅 on the
help board show your next turns as a menu to tap — in a private chat with
the bot if you have one, else in the cleaning room:

```
📅 Your next turns
1️⃣ 2nd Floor · Thu–Sun (8 – 11 Oct) · later this week
2️⃣ 👉 Kitchen · 13 – 19 Oct · next week
3️⃣ 2nd Floor · Mon–Wed (20 – 22 Oct) · in 2 weeks · 🆘 asked

👉 Kitchen · 13 – 19 Oct — 🔄 swap it · 🆘 I can't make it
```

Tap a number to pick a turn, then 🔄 to ask the room who swaps with you
(only 🔄 counts on that request: whoever takes it gives you their next turn
of the group), or 🆘 if you can't make it (the request below: 🙋 or 🔄).
Only your own taps count; the menu is edited in place and your tap taken
back, so the same buttons work again. It keeps showing what became of each
turn — done, asked, or someone else's now. Only your latest menu is live.
`!next <person>` and `!next 11`+ give the plain list.

**Swapping without commands.** Two ways, both by reaction:

- *Cleaning early.* ✅ (or `!done`) while your own shift of the week is
  still to come — Thu–Sun, say — and the running shift of the same slot is
  still open: you did that one, so you swap. You get the running shift,
  marked done; its holder gets your later one and is told:

  ```
  🔄 @alice — bob already cleaned **2nd Floor · Mon–Wed** for you, so you two swapped: you're on **Thu–Sun (8 – 11 Oct)** now.
  Doesn't suit you? React ↩️ and I'll swap you back.
  ```

  ↩️ there (by either of you) swaps back until the later shift starts; the
  cleaning stays recorded. Taking the ✅ back undoes both.
- *🆘 Who steps in?* 🆘 on the plan or a reminder asks the room to cover
  your open turns of that week; `!sos [group] [slot] [week N] [on <day>]`
  does it for your next or any later turn (also from a private chat). The
  request pings the group's other members:

  ```
  🆘 Who can step in? bob can't make it:
  2nd Floor · Thu–Sun (8 – 11 Oct) · week 41
  🙋 I'll do it · 🔄 swap — you take it, bob takes your next turn
  (bob: ↩️ if you can make it after all)
  @alice @carol
  ```

  🙋 — anyone in the room — takes the turn over. 🔄 takes it and hands the
  helper's next turn of that group (not yet started, not one where the asker
  already cleans) to the asker; without such a turn, the bot says so and 🙋
  still works. The request is edited to "✅ Sorted!" / "✅ Swapped!", which
  pings the asker. ↩️ by either of them undoes it until its turns start, and
  the request is open again; ↩️ by the asker on an open request (or taking
  the 🆘 back) withdraws it. A request closes by itself once the turn is
  done, over, or reassigned otherwise. If nobody has stepped in when the
  turn starts, the bot replies to it pinging the admins.

`!swap @user` (ask one person, who accepts with `!swap accept <id>`) and
`!takeover` still work.

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
🗓 Your calendar: subscribe — add it in your calendar app
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
`!plan pdf`, `!mygroups`, `!join`, `!leave`, `!done`, `!undo`, `!sos`,
`!help` and `!ical` — only about yourself, never about others, and never admin commands
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
- **The start of the week takes turns.** In a week split into shifts, the
  rotation picks who cleans that week as before, then gives each of them the
  shift after the one they had last time: start of the week this time, end of
  the week next time. Without that, a group with an even number of members
  would leave the same people on Mon–Wed every time.

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

### Names

Plans, PDFs and messages call people with a Matrix account by their display
name in the room at hand; else by their global Matrix profile's name, which
the bot can look up without sharing a room with them; else by the name it
stored last time; else by their Matrix username. What the room or the profile
says is stored on the person (startup, before a PDF, on each of their
commands, on `!member link`), so plans render without asking again. Profile
lookups are cached and bounded in time; when a server refuses (403/404) or
can't be reached, the stored name stays. Nothing depends on them, and a
lookup never creates a person. All of this lives in `src/names.rs`.

### Calendar feeds (`!ical`)

With `[ical_server]` configured, `!ical` gives a personal subscription URL
(`https://…/ical/<token>.ics`); without it, a .ics file. The welcome and
every group selector show the link too. It isn't treated as a secret: a
feed holds only that person's own turns, which the plan in the room shows
anyway — but anyone with the link can follow those turns.

- Tokens are 256 bits of randomness. The server checks the exact token
  format, that the token isn't revoked, and that its person is active and
  linked to Matrix. It answers with an ETag (and 304 for `If-None-Match`)
  and `Cache-Control: private`; tokens are never logged.
- The tokens are stored in `state.json` (with their hash) so `!ical` can
  show the same link again. Serve the feed over HTTPS (the bot warns at
  startup otherwise).
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

`!plan pdf [N] [group]` renders N weeks from this one — without N, as many
whole weeks as fill one page per group (ten for a group with two slots, 21
for a weekly one); `!plan pdf next [N] [group]` the same from next week on;
`!plan pdf history [N] [group]` the last N weeks up to this one (default 8). It is made to be printed in black and
white: a heavy frame and heavy rules between weeks, dashed rules between the
shifts of a week, dotted ones between slots; a week never breaks across
pages. Open turns have a box to tick by hand, done ones a tick and the date;
a slot's rooms are listed once under the title, with icons. Colour is only
a bonus on screens. Both show the time
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
- **Taps (selectors, menus), ✅, 🆘, 🙋, 🔄 and ↩️ reactions** count once per reaction event, also across
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

On the first start, plans made before the start of a split week took turns are
re-seated once, from next week on: within each week, the two people may trade
Mon–Wed and Thu–Sun. Nobody gains or loses a week. Weeks that were assigned,
swapped, taken over, done or asked about (🆘, a pending swap) stay as they are.

The calendar is now named "Cleaning – <name>" instead of "Putzplan – <name>";
event UIDs are unchanged, so subscriptions just update.

**Rolling back** to an older version is not safe once rhythms were changed:
it ignores the new fields and would treat `2x` windows as Mon–Wed/Thu–Sun and
forget earlier rhythm versions.

`cargo run --bin state_debug -- state.json` prints a quick report of a state
file, but its schedule is a simple guess that ignores rhythms and slot
assignments — use `!plan` and `!validate` for the real thing.

## Paper plans and photos

Current/upcoming `!plan pdf` exports are printable, scannable wall plans in
one of two styles, chosen by an administrator — for every group with
`!plan pdf style days` or `!plan pdf style tick`, for one group with
`!plan pdf style <group> tick` (`<group> default`: like everyone again).
`!plan pdf style` shows what each group prints. One PDF can hold pages of
both styles:

- **days** (default): one row per duty with a box for each allowed day. Put
  one clear X (or any clear mark) in the day you cleaned; that day is
  recorded.
- **tick**: one line per week, the slots and shifts side by side, one box per
  duty. Put one clear X (or any clear mark) in your box. The sheet only says *that* it was done,
  so the bot records the middle of the duty's days (Thursday for a whole
  week, Monday for Mon–Tue, Thursday for Thu–Fri), or the day of the photo if
  that is earlier.

`!plan pdf view [next] [N] [group]` prints the same plan only to look at —
for hanging up where nobody ticks: each group's table in its style, without
boxes (on a days plan a solid bar across the days a duty may be done), without QR
code or corner targets, and with "View only, not for ticking" at the top and
a framed note at the bottom pointing to the plan with boxes or `!done`. A
photo of it is ignored.

Then send a full-page photo of a sheet with boxes to the cleaning
room or an authorized encrypted DM. The bot previews changes and asks for ✅ Apply / ❌ Cancel.
Nothing is applied before confirmation. Boxes the photo leaves unclear (a
fold or shadow over them) are named in the preview, to record with `!done`;
the rest still applies. A filled-in box counts as taken back (and is named
too). A crumpled sheet is refused as a whole: smooth it out and retake. Residents can record their own duties;
administrators can confirm a shared sheet. `!plan pdf history` remains a compact
read-only report. See [paper workflow, dependencies and tests](docs/PAPER.md).
