# Event subscriptions: workers and people subscribe to amux activity

Ethan, 2026-11-11: "workers can 'subscribe' to events and there's a list of
events across every piece of activity it can listen to and define its own
natural language -> filters criteria." First use case: get told when a browser
session hits a CAPTCHA and cannot continue.

## What already exists (measured 2026-11-11)

This is mostly a matter of finishing and exposing an engine that is already
built. It does not need a ninth primitive.

| Piece | Where | State |
|---|---|---|
| One event funnel | `session_verbs::emit_event` writes `session_events` and calls `signals::on_event` | 128 event types, 126,288 events in the last 7 days |
| Board CDC | `board_change_log` (SQLite triggers, migration 0061), tailed by `runtime_jobs/cdc_poller.rs` every 200 ms | 80,387 rows. Only goes to SSE today, not to subscriptions |
| Subscription engine | `api/signals.rs`, `/api/subscriptions`, migration 0130 (2026-10-20) | Glob on event type, source and session filters, actions `send`, `board`, `schedule` and `shell`, a dedupe window, and a `signal_fires` log |
| Use | `signal_subscriptions` | **0 subscriptions and 0 fires ever.** No CLI, no UI, no catalog, no prompt mentions it |

Ethos rule 1 applies directly: the capability exists and reaches nobody. Most of
the work below is reach: a catalog to choose from, a way to say what you want in
words, and somewhere to see what fired.

## The model

**Event.** `{type, subject, source, ts, data}`. `type` is a dotted noun.verb,
such as `browser.blocked`, `board.card.moved` or `session.native_status`.
`subject` is what it is about (a worker, card, browser profile or schedule).
Every event enters through `emit_event`, and that stays the only door. Stores
that do not go through it today feed the same dispatcher:

- board CDC rows become `board.card.<op>` (the CDC poller calls
  `dispatch_subscriptions` beside its SSE publish);
- schedule runs, email sends and Messages posts already call `emit_event`, or
  get the one-line call where they do not.

**Catalog.** `GET /api/events/catalog` (and `amux events ls`) is COMPUTED from
what is actually emitted: each type with its count over the last 7 days, when it
was last seen, its data field names sampled from recent rows, and one example.
A small hand-written registry adds a one-line meaning per type. A type that is
emitted but has no registry entry is listed as `undocumented`, so drift shows up
in the catalog itself. `amux events tail <pattern>` streams live matches, so a
worker can see what an event looks like before it subscribes.

**Subscription.** The existing row, plus three columns:

- `criteria`: the natural-language ask, as written. This is the source of truth.
- `filter`: a JSON predicate over `data` that is compiled from `criteria`, with
  `all`/`any` and field operators `eq`, `ne`, `in`, `contains`, `regex`, `gt` and
  `lt`.
- `owner`: the worker or person it belongs to.

**From natural language to a filter.** The model is used once, at subscribe
time, to compile `criteria` against the catalog's types and field names into a
pattern plus a `filter`. The compiled result is shown back before it is saved
("matches browser.blocked where reason = captcha"). Each event is then checked
by plain code with no model call (ethos rule 2). When a type's fields change,
the subscriptions that read it are recompiled and flagged.

Some criteria are fuzzy, such as "when a worker seems stuck on a login". For
those, `judge: true` sends only the events that already passed the cheap filter
to the helper model for a yes or no. It is capped per subscription per hour, and
every verdict is logged in `signal_fires`.

**Actions.** The existing `send`, `board`, `schedule` and `shell`, plus
`notify`:

- `notify` posts to the owner's own Messages thread and sends a push. It uses
  the urgent alert path only when the subscription says `urgent`.
- A worker's subscription defaults to `send` to itself. That reaches the worker
  at its next turn boundary with a dedupe key, which is how board state already
  reaches lanes (ethos decision: turn-boundary delivery, not pub-sub).

**Who may subscribe to what.** A worker can subscribe to events whose subject
it could already message or see: itself, its groups, and open groups. This is
the same reach predicate Messages uses, so no new permission model is needed.
The owner can subscribe to anything. Isolated workers get no subscriptions,
because isolation means no amux routing.

## The CAPTCHA case, end to end

1. **Emitter (computed, no model).** The browser route emits `browser.blocked`
   `{reason, profile, url, worker, backend}` when the page shows a known
   challenge. Detection is cheap: a reCAPTCHA, hCaptcha or Turnstile frame, or
   a challenge page (Cloudflare's "Just a moment", a `/sorry/` interstitial).
   The same event covers `login`, `2fa` and `rate_limit`, which
   `amux browser route advance` already reasons about.
2. **Subscription.** Ethan types "tell me when any browser hits a captcha". It
   compiles to `browser.blocked` with `reason = captcha`, action `notify`.
3. **Fire.** A post in Ethan's Messages thread names the worker, profile and URL
   and links the browser view. It is deduped per profile and URL for 10
   minutes. The worker that was driving gets the same event as `send`, so it
   stops retrying.

## Where it lives

- Engine: `crates/amux-server/src/api/signals.rs`, which already holds
  subscriptions. The catalog and filter go beside it.
- CLI: `amux subscribe "<criteria>" [--notify | --to <worker>]`,
  `amux subscriptions`, `amux events ls|tail`.
- UI: a Subscriptions row in each worker's Configuration tab, and Settings →
  Events for the catalog plus every subscription and its recent fires. Both
  follow the phone rule of one row of controls.
- Reach: one line in the worker context that names `amux events ls` and
  `amux subscribe`. Without it, no worker will ever use this (rule 1, as the
  0 subscriptions show).

## How we know it works (rule 4)

- Every fire is a `signal_fires` row with its outcome, plus a
  `subscription_fired` verdict log line.
- A filter that has matched nothing for 7 days while its type is still being
  emitted is listed as `silent`, so a broken compile shows up.
- The catalog says `measured: true` with its row count, so an empty catalog
  cannot pass for a quiet one.

## Phases

1. Catalog (`/api/events/catalog`, `amux events ls|tail`), board CDC into the
   dispatcher, the `notify` action, and the `amux subscribe` CLI on the
   existing glob engine.
2. The `filter` and `criteria` columns, compiling natural language to a filter,
   showing the compile result back, and recompiling on drift.
3. The `browser.blocked` emitter and the CAPTCHA subscription as the first real
   consumer, with an e2e test that drives a test page with a fake challenge
   frame.
4. `judge` mode, then the UI rows and the worker-context line.
