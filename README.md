# cleaning-bot

A cleaning rota for a shared house, in the Matrix room you already use. It
knows whose turn it is, posts and pins the week's plan, reminds people, and
keeps track of who actually cleaned.

## Using it (for housemates)

```
📅 !next · your turns: swap one or 🆘 from there
📋 !plan · see the plan
👥 !mygroups · join or leave groups
✅ !done · done! (or ✅ on the plan)
🆘 !sos · can't make it? find cover (or 🆘 on the plan)
🗓 !ical · where your calendar is
📄 !plan pdf · printable plan
```

The bot answers a command as a reply to it, in the thread, if you wrote
it in one.

**The help board.** An admin can post a friendly overview with `!help post`.
Its reactions are buttons: 📅 your next turns (as the menu below), 📋 the
plan, 👥 your groups, 📄 a PDF, 💬 a private chat: the bot answers privately if you
have a private chat with it, else as a reply to the board, addressed to
you, and then takes the reaction away so the button stays clean. 💬 makes
the bot invite you to a new encrypted chat (no invite allowlist needed for
that); once you join, it greets you there with your groups and calendar
link.

`!help more` lists the rest (`!status`, `!undo`, `!takeover`, `!swap`,
`!join`, `!leave`, `!groups`, `!stats`, `!plan pdf history`).

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
"✨ All done. Thanks!". A shift starting mid-week gets a "🔔 Your turn
starts today" one; `!plan remind` (admin) sends a "Still open this week".

**Your turns, as a menu.** `!next` (or `!next N`, up to 10) and 📅 on the
help board show your next turns as a menu to tap, in a private chat with
the bot if you have one, else in the cleaning room:

```
📅 Your next turns
1️⃣ 2nd Floor · Thu–Sun (8 – 11 Oct) · later this week
2️⃣ 👉 Kitchen · 13 – 19 Oct · next week
3️⃣ 2nd Floor · Mon–Wed (20 – 22 Oct) · in 2 weeks · 🆘 asked

👉 Kitchen · 13 – 19 Oct
🔄 swap it · 🆘 I can't make it
```

Tap a number to pick a turn, then 🔄 to ask the room who swaps with you
(only 🔄 counts on that request: whoever takes it gives you their next turn
of the group), or 🆘 if you can't make it (the request below: 🙋 or 🔄).
Only your own taps count; the menu is edited in place and your tap taken
back, so the same buttons work again. It keeps showing what became of each
turn: done, asked, or someone else's now. Only your latest menu is live.
`!next <person>` and `!next 11`+ give the plain list.

**Swapping without commands.** Two ways, both by reaction:

- *Cleaning early.* ✅ (or `!done`) while your own shift of the week is
  still to come (Thu–Sun, say) and the running shift of the same slot is
  still open: you did that one, so you swap. You get the running shift,
  marked done; its holder gets your later one and is told:

  ```
  🔄 @alice, bob already cleaned **2nd Floor · Mon–Wed** for you, so you two swapped: you're on **Thu–Sun (8 – 11 Oct)** now.
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
  🙋 I'll do it
  🔄 swap: you take it, bob takes your next turn
  (bob: ↩️ if you can make it after all)
  @alice @carol
  ```

  🙋 (anyone in the room) takes the turn over. 🔄 takes it and hands the
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

**Welcome and groups.** Everyone gets one welcome, ever, when they join the
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
🗓 Your turns: in your calendar in the cloud, **Cleaning: mia** (its Calendar app, or DAVx⁵ on your phone)
!mygroups reopens this · !join / !leave · !help
```

Without a calendar in the cloud yet, the selector says `🗓 Your calendar:
!ical says where your turns are`.

Tapping a number toggles that group. The bot edits the selector (no extra
message) and, once the tap is saved, removes your reaction so the same
number can be tapped again (that is what "tap again to leave" means). If the
bot lacks the power level to remove other people's reactions, your reaction
stays; removing it yourself then undoes that tap. `!mygroups` posts a fresh
selector; `!join`/`!leave` work without buttons. More than ten groups: the
rest via `!groups` and `!join`.

**A private chat with the bot** (encrypted) also takes `!next`, `!plan`,
`!plan pdf`, `!mygroups`, `!join`, `!leave`, `!done`, `!undo`, `!sos`,
`!help` and `!ical`, only about yourself, never about others, and never admin commands
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
  would move one is refused: change or remove that turn first.
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
stay *nobody assigned*: visible in the plan, fillable with `!plan assign`.
`!validate` warns about such groups.

### The staircase: a group for the whole house

Some places are everyone's: the staircases. `!groups house <group> on` makes
a group **house-wide**: every resident is in it, whatever floor or corridor
they live on, and the bot keeps it so. A resident is anyone in at least one
other group (a paused one too; a subtenant in their kitchen's group as well). Joining a first group puts them on the
staircase as well, right away. Leaving their last one takes them off it
within half an hour (their turns handed on), once a staircase turn of the
running week is done or over: someone moving floors (out of one group, into
another) stays on it with their turns as they were. Nobody joins or leaves
it with `!join`, `!leave` or a selector.
An admin can excuse someone (`!member remove <person> <group>`) or add
someone who is in no other group (`!member add`); the bot remembers both.
`!groups` shows such a group as `🏠 everyone (14) · excused: Mia`.

Two people per staircase are two slots at one **place**: `!groups slot add
Staircase Scharni x2` makes the slots "Scharni 1" and "Scharni 2" at the
place "Scharni". Each of the two has their own turn (reminder, ✅, 🆘, swap);
the plan, reminders and calendars name the place, and a room, weight or
`!groups slot remove` given for the place applies to all of its slots.
Commands that take a slot (`!sos`, `!takeover`, `!plan assign`) take the
place's name too. Calendar events say who you clean it with.

Our house: the staircases Scharni and Colbe, every two weeks, two people
each:

```
!groups add Staircase
!groups rhythm Staircase every 2
!groups slot add Staircase Scharni x2
!groups slot add Staircase Colbe x2
!groups room add Staircase Scharni Staircase Scharni
!groups room add Staircase Colbe Staircase Colbe
!groups weight Staircase 1.5
!groups house Staircase on
```

### Fair across all groups

Each group's rotation queue decides **how often** someone cleans there:
everyone in a group gets the same number of turns, as before. Which of the
people nearly due gets **which week** is decided with all their other duties
in view, in every group:

- Every turn is some work: its rooms (with their weights) × its slot's
  weight × its group's weight (`!groups weight`). Make unpopular or heavy
  tasks (bathrooms, the staircase) heavier: they are spread further apart.
- A clash is another duty of the same person on the same days, in the same
  week, or a week or two apart, counted by both duties' work (the same days
  much more than two weeks apart), plus a little for everything they have on
  around then.
- Of the first few in line (the week's open turns and as many again, but
  never as far back as the people who cleaned last time, so a small group
  just runs in order), the bot takes whoever clashes least with this week
  **compared with the group's next due week**: putting someone off only
  happens when they have less on then. A place further back in line counts
  against a candidate, so a vague difference never reorders the rotation.
- Someone put off keeps their place at the front, is drawn first after two
  due weeks of it whatever it costs, and makes up the places they lost on
  their next turn. A clash moves a turn by a week or two; nobody cleans less
  often because of it, and nobody more.
- Groups cleaned every two weeks are planned twice as far ahead as weekly
  ones; there, the weekly groups' coming turns are forecast from their
  rotation, so the staircase is planned around them too.

With nothing else on (one group, or nobody in two), the rotation runs in
line order exactly as it always did. Frozen turns are never moved: only
weeks not planned yet are planned this way.

### Private chats

For a resident, a room other than the cleaning room counts as a private chat
only if, according to the server's current state (fetched for every command,
reaction and redaction there):

1. the bot is in the cleaning room, and the resident and the bot are both
   joined members of it;
2. the private room has exactly two members (the bot and the resident) and
   nobody else invited or knocking;
3. the private room is end-to-end encrypted (Megolm).

Anything else, or any error fetching the state, means no access. (The check
runs right before each answer; someone joining in the moment between check
and answer isn't caught.) Display
names and `m.direct` never decide this (`m.direct` is only used to find an
existing chat for a welcome). The bot only joins a chat it is invited to if
the inviter is allowed by `security.allowed_inviters`, so residents who
should use it privately need to be listed there (or `"all"`). Changes made
from a private chat update the plan in the cleaning room.

### The house directory: who lives here (`[directory]`)

With `[directory]` the bot reads the house's directory (Authentik, a
read-only token) at startup and every 15 minutes. It is the one source of
who lives here, their Matrix accounts and their groups; the plan keeps only
its own history.

**One person per resident.** A resident's recognised accounts are their house
account (`@<login>:<server_name>`) and their linked accounts (`matrix_ids`,
full Matrix IDs an admin entered or they proved themselves with a link code).
Commands, reactions and undos from any of them count as the same person in
the plan: no second person, no second place in a rotation. The plan finds its
person for a resident by the directory id (Authentik's `uuid`) once it has
stored it, else by one of their accounts, and then stores the id (the event
log says `person_directory_linked`). Before the bot makes a new person for an
account (a welcome, `!join`, `!member add @…`), the account's resident is
looked up: their person is used if there is one.

**The address.** Mentions, reminders and turn notices go to the person's
address in the plan. Once the resident chooses an account for the house
(`matrix_preferred`: their house account or one of their links), the address
moves there (`person_matrix_linked` in the log); private chats the bot opened
for an older account keep working, a chat with any recognised account counts
(each account in one must itself be in the cleaning room, see Private chats).
Not chosen, or a choice that can't be used: the address stays where it is.

**Who lives here.** `residents_group` names the directory groups of everyone
who lives here, one name or a list: in the house `["residents", "subtenants"]`
(full members, and people who live here for a while with a kitchen of their
own; house-infra docs/HOUSE-DIRECTORY.md). Living here is being in ANY of them.
An older config with one name (`residents_group = "residents"`) still works;
`""` or `[]` is off.

**Who leaves.** Someone in the plan the bot has linked who is in none of the
`residents_group` groups (off unless named; moved out to `former-residents`,
deactivated, or deleted) leaves every cleaning group once two good reads in a
row say so, the way `!member remove` does it: this week's turn stays, later
ones are handed on, completions, statistics and the log stay. The admins get
one message per person. Someone in the plan without a directory id (a name
without Matrix, an account the directory doesn't know) is never touched.
Switch `residents_group` on once the first reads have linked everyone (and
the first departures are held anyway, see Guards).

**Kitchens.** `!groups directory <group> kitchen-floor-1` makes a group
follow a group of the directory: its members (who live here, when
`residents_group` is named: a subtenant of kitchen 3 joins kitchen 3 like a
resident) join it the way `!member add` does, someone not in the plan yet is added with their
chosen account, or with their house account when they haven't chosen;
someone no longer in it leaves after two reads, as above. Nobody joins or
leaves such a group with `!join`, `!leave` or a selector (an admin's `!member
remove` is undone by the next read: change the directory instead).
`!groups directory <group> off` ends it. A move from kitchen 1 to kitchen 3:
joins group 3 within 15 minutes, leaves group 1 15 minutes later.

**Guards.** A read that failed, or a last good read older than the
directory's `stale_after` (60 min), changes nothing (the health check shows
the failing reads). A group the directory doesn't have (a typo, a rename;
also any one of the `residents_group` groups) and residents groups with nobody
in any of them remove nobody (one of them may be empty: nobody subletting
now). More departures from one group in
one round than `max(2, 20 %)` of its people are held: nobody leaves that
group (other groups' departures within their limits go; someone moving out
counts in each group they leave), the admins are told once, with the held
departures' digest (8 hex digits), and `!directory confirm <digest>` lets
exactly those go if the directory still says so, answering with who left
what; another digest, or none, does nothing and shows what is held. The
first departures after following began (nothing remembered yet: the
upgrade, or `residents_group` switched on or a group taken out of it; adding a
group, such as `subtenants`, lets nobody more leave and needs no new review)
are all held that way,
however few, until an admin confirms them or two reads in a row ask for no
departure at all (reads while a residents group is missing or all of them are
empty count for neither: they ask nobody to move out, so they say nothing
about who has). An account two residents claim counts for neither, and
the admins hear of it once. `!directory` shows what the bot follows and what
is held. The memory between two reads is in `state.json`, so a restart in
between neither forgets nor repeats a departure.

**Rights never come from a link.** An admin is an account in `[security]
admin_users`, the account that wrote: another account of an admin, linked in
the directory, is them in the plan but no admin. (Until 2026-10 a linked
account of an admin was an admin too.)

- Nothing is merged. When two accounts of one resident already are two
  people in the plan, the one in a group, with a turn or a completion is the
  person. When both are, both stay so and the admins are told once: an admin
  takes the extra one out of its groups (`!member remove`) and its turns.
- An account two residents claim counts for neither.
- While the directory can't be read, the last answer stays for recognising
  accounts and nothing follows it; before the first one (or without
  `[directory]`), every account is a person of its own.

All of this lives in `src/directory.rs` and `src/directory/reconcile.rs`.

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

### `!ical`

`!ical [weeks]` says where your turns are: in your own calendar in the
house cloud (below), kept up to date. Without one (no house account, or
before your first login in the cloud), it sends your next turns (26 weeks,
or `weeks`) as a `.ics` file, which does not follow changes. Events carry
`STATUS:CONFIRMED` (or `CANCELLED` for a skipped turn), the task and who
you clean it with, the completion, and `LAST-MODIFIED`.

There are no subscription links (feeds) anymore: `!ical reset` and `!ical
revoke` only say so, and links handed out before stopped working.

### Calendars in the cloud (`[nextcloud]`)

With `[nextcloud]` and `[directory]`, every resident with a house account
(in the directory; their login is their Nextcloud user) gets a calendar of
their own in the house's Nextcloud, `cleaning-<login>` named "Cleaning:
<name>". The bot's Nextcloud account owns it and shares it read-only with
that resident alone. It holds their turns from today on (one event per turn,
the same UIDs as an `!ical` file) and follows every change of the plan within a
minute (takeovers, swaps, rhythms, done marks): only what changed is
written. At the start and every `refresh_minutes` everything is compared
with the server (the calendar, its share, every event's ETag); past and
obsolete turns go. The bookkeeping is `store/nextcloud.json`; losing it
costs one full comparison, nothing else.

- Once the calendar is shared with them, `!ical` and their group selectors
  say where it is (the cloud's Calendar app, DAVx⁵ on Android, a CalDAV
  account on iPhone), and the bot tells them once in their private chat, if
  they have one (a note that did not arrive is tried again in the next
  round).
- Nextcloud only has an account for someone after their first login there:
  until then, the calendar waits unshared and `!ical` sends them a file.
- Residents without a house account (not in the directory) get files.
- Someone who is no longer in the directory, or in no group and without
  turns, loses the calendar: unshared, then deleted (Nextcloud's trash bin
  keeps it for a while). Only after a complete comparison, never while the
  directory has not been read, never when nobody at all would get one, and
  never more than a third of the calendars (at least 2) in one round: more
  at once is a directory gone wrong, so none is removed and the log says so.
- Two logins that would get the same calendar (they differ only in
  characters a calendar name cannot hold, or in case) get none, and the log
  names them; `!ical` sends them files.
- The app password comes from the environment (`password_env`, default
  `CALDAV_PASSWORD`). Without it, or without `[directory]`, the bot runs
  without calendars and says so in the log. Without calendars (also once
  `[nextcloud]` is removed), `!ical` sends everyone files.

All of this lives in `src/calendars.rs`; the CalDAV side (calendars, shares,
events by UID) is mxbot-common's `calendar::nextcloud`.

### PDF

`!plan pdf [N] [group]` renders N weeks from this one. Without N, as many
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
punctuation. Other characters (Polish ł, Cyrillic, Greek, emoji) are
missing from the PDF.

## The website

With `[web] enabled`, residents on the house network see the plan in a
browser, on phones too: **This week** (`/`: the week, your next three turns
with how far off they are, every group's turns with their state, then the
next six weeks in short; `?weeks=` 1–26), **Groups** (`/groups`, one group
at `/g/<id>`: each group's coming weeks; the staircase with its places side
by side, "Scharni: Alice & Bob · Colbe: Carol & Dan"), **People**
(`/people`: everyone in the plan with their turns of the next 8 weeks, how
many and their load, the rooms weighted as the rotation balances them) and
**Mine** (`/me`: your turns of the next 12 weeks, with who shares the
place). Your own turns are highlighted and say "you". German, English or
Ukrainian: `?lang=`, the house-wide `house_lang` cookie, Accept-Language,
else German (`src/lang.rs`, with mxbot-common's `web::lang`).

- **Read only, and always the plan of the room.** Every page is built from
  the same schedule snapshot as the plan, the PDF and the calendars, under the
  bot's own state lock: nothing on the website can differ from Matrix, and
  nothing can be changed there. ✅, `!sos` and swaps stay in Matrix.
- **Only behind the house login.** The reverse proxy (Caddy with
  Authentik's forward auth) sends `X-House-User`, `X-House-Groups`
  (`|`-separated), `X-House-Name`, `X-House-Matrix-Ids` and the secret header
  `X-Cleaning-Proxy-Secret`, whose value is the environment variable
  `CLEANING_PROXY_SECRET` (at least 32 characters; the bot refuses to start
  without it while `[web]` is on). No or a wrong secret: 403 (compared in
  constant time); no login: 401; not in `allowed_groups`: 403. An empty
  `allowed_groups` lets nobody with a login in (until 2026-10-30: everyone).
  The check is mxbot-common's `web::ForwardAuth`, the same as the deals
  website's.
- **In the house without a login** (`[web] guests = true`): for a proxy that
  asks for the login only from outside the house, the plan is shown to a
  request without a login as to a guest: everything, but nobody is "you",
  and `/me` says to ask the bot (`!next`). The proxy must drop a client's own
  `X-House-*` headers (Caddy does) and still send the secret. A guest's way
  to log in is the link to `/login`: the proxy asks for the login there from
  everywhere, and from then on for every request with the identity
  provider's session cookie (house-infra: `forward_auth_login_path`), so
  whoever logged in once sees their turns in the house too; `/login` itself
  only sends them on to `/me`.
- **Who you are:** your house account `@<login>:<the bot's server>`
  (`mxbot_common::web::house_account`: a login that is no plain localpart has
  none; Authentik's `matrix_localpart` is not in the headers, so a renamed
  resident is their new login's account here) or one
  of the accounts in `X-House-Matrix-Ids`, the person the plan knows them
  by (see *The house directory*). Someone the plan doesn't know sees
  everything, just without "you". Display names only, never Matrix IDs, and
  never anything to subscribe to (`/me` points to `!ical`).
- **Its own listener** (`bind_addr`, default `0.0.0.0:8080`).
- Pages are never cached (`no-store`) or indexed (`X-Robots-Tag`), load
  nothing from elsewhere and need no script (CSP: this origin's style sheets
  only): the house UI kit `/_house/ui.css`, then `/style.css`.

```toml
[web]
enabled        = true
bind_addr      = "0.0.0.0:8081"
public_url     = "https://putzplan.example.org"
allowed_groups = ["residents"]   # in the house: who lives here, ["residents", "subtenants"] (and the admins' groups)
# home_url = "https://example.org/"  # the house bar's link home (default: public_url's parent domain)
# home_name = "Example"
```

All of this lives in `src/web.rs` and `src/web/`.

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
  versions: old rhythms keep their meaning;
- the calendar feeds are gone: their tokens in an old `state.json` and an
  `[ical_server]` section in an old config are ignored (and dropped from the
  state with its next save); selectors lose their link with the first
  refresh after the start;
- selectors without a room belong to the cleaning room;
- bookkeeping for commands, redactions and welcomes starts empty;
- people gain a directory id and groups a directory group, both unset: with
  `[directory]`, the first good read links everyone it can find by account
  (one `person_directory_linked` per person), and from the second read on,
  linked people who no longer live here leave their groups (once
  `residents_group` is named) and kitchens following a directory group lose
  who is no longer in it. The first departures are all held for
  `!directory confirm <digest>`, however few.

On the first start, plans made before the start of a split week took turns are
re-seated once, from next week on: within each week, the two people may trade
Mon–Wed and Thu–Sun. Nobody gains or loses a week. Weeks that were assigned,
swapped, taken over, done or asked about (🆘, a pending swap) stay as they are.

The calendar is now named "Cleaning – <name>" instead of "Putzplan – <name>";
event UIDs are unchanged, so subscriptions just update.

**Rolling back** to an older version is not safe once rhythms were changed:
it ignores the new fields and would treat `2x` windows as Mon–Wed/Thu–Sun and
forget earlier rhythm versions. A version from before the house directory fails
on a `state.json` whose event log holds `person_directory_linked` or
`group_directory_set` events: **take a copy of `state.json` before upgrading**,
and roll back with that copy (what happened since is lost). From this version on
the event log is tolerant: an event kind it does not know (written by a newer
version) is kept as written, saved back unchanged and ignored by the plan and the
statistics, so later additions no longer break a rollback.

`cargo run --bin state_debug -- state.json` prints a quick report of a state
file, but its schedule is a simple guess that ignores rhythms and slot
assignments: use `!plan` and `!validate` for the real thing.

## Paper plans and photos

Current/upcoming `!plan pdf` exports are printable, scannable wall plans in
one of two styles, chosen by an administrator, for every group with
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

`!plan pdf view [next] [N] [group]` prints the same plan only to look at,
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
