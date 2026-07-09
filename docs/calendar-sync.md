# Calendar & Google/Apple sync

amux has a built-in calendar with three layers, and it can push your **events** to
Google or Apple Calendar via a standard iCal feed. This guide covers creating
events, subscribing your real calendar to them, the exposure options, and the
gotchas worth knowing before you rely on it.

## The three layers

The **Calendar** tab overlays three independently toggleable layers:

| Layer | What it is | Leaves amux? |
|-------|------------|--------------|
| **Events** | Real calendar events you create | **Yes** — this is the only layer that syncs out |
| **Tasks** | Scheduled/recurring jobs (the scheduler) | No — in-app only |
| **Issues** | Board items that have a due date | No — in-app only (off by default) |

Only events sync. Tasks and issues would be noise on your real calendar, so they
stay inside amux behind toggles.

## Creating an event

- Click **+ Event** in the calendar header (or click any empty slot).
- Give it a title; choose **all-day** or a start/end time; optionally a location
  and description. **Save.**
- Click an existing event to edit or delete it.

Events are stored in the `cal_events` table and served as an RFC 5545 iCal feed at
`GET /api/calendar.ics`.

## Subscribing your real calendar

Click **Subscribe** in the calendar. amux hands you a public feed URL and buttons
for Google and Apple. The URL comes from whichever exposure you have configured,
in priority order:

1. **Tunnel** — `https://<id>.t.amux.io/api/calendar.ics`. The intended path: no
   extra infra, no port forwarding. (Requires the [tunnel](../README.md#tunnel).)
2. **S3** — set `AMUX_S3_BUCKET`; the feed auto-uploads on every event change and
   is always reachable, even when your machine is off.
3. **Download .ics** — a static, one-time import. Zero infra, but no live updates.

### Adding it to Google Calendar (step by step)

1. Open Google Calendar → **Settings** (gear) → **Add calendar** → **From URL**.
2. Paste the feed URL from the Subscribe dialog.
3. Click **Add calendar**. Google fetches the feed and shows it under
   *Other calendars*.

The amux dashboard's Subscribe dialog also has one-click **Add to Google** /
**Add to Apple** buttons that pre-fill this for you.

## Gotchas (read this before relying on it)

**Google caches iCal feeds by URL — aggressively.** After the first fetch, Google
re-polls on its own cadence (often hours) and will keep serving its cached copy.
There is no reliable "refresh now." If you change the *shape* of the feed and need
Google to pick it up immediately, publish to a **new URL** and re-subscribe (bump
the S3 key, e.g. `calendar.ics` → `calendar-v2.ics`; the old URL keeps serving the
stale cache). See `CLAUDE.md` § iCal / Google Calendar sync for the S3 mechanics.

**A tunnel feed is only reachable while your machine + tunnel are up.** Google polls
periodically; if your laptop is asleep at poll time, Google just keeps the last
snapshot. For an always-fresh feed regardless of your machine, use the S3 exposure.

**One tunnel serves one target.** If you point the tunnel at some other local port
(e.g. a dev server), `/api/calendar.ics` is no longer exposed through it. Point the
tunnel back at the dashboard (`amux tunnel start`, no port) for the calendar.

**Timezones are handled for you.** Timed events are emitted as UTC (`DTSTART:…Z`)
so Google/Apple display the correct local time; all-day events use `VALUE=DATE`. If
an event shows hours off, you're looking at a stale cached fetch (see above), not a
bug.

## Verifying it worked

- Watch `amux tunnel status` — the **request count** ticks up when Google fetches
  the feed through the tunnel.
- Check the event shows at the **right local time** in Google/Apple.
- If you have `curl`, `curl -s <feed-url> | grep SUMMARY` should list your events.

## Cleaning up / switching feeds

Subscriptions live in *your* Google account, not amux — remove them in Google
Calendar (*Settings → the calendar → Unsubscribe*). Because feed URLs are
long-lived and Google names calendars from the feed's `X-WR-CALNAME`, you can end
up with duplicates named "amux" if you subscribe to more than one exposure (tunnel
+ S3). Keep the one you want and unsubscribe the rest; each calendar's **Settings**
page shows its source URL so you can tell them apart.
