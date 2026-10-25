# amux Cloud signup (serverless)

Owner design, 2026-10-17: a static signup page, Stripe Checkout, and one serverless function. No
control-plane server and no database. Each customer gets a standalone amux server.

| Piece | Where |
|---|---|
| Signup, welcome, billing pages | `site/start/` on main (GitHub Pages at amux.io/start) |
| The function | `worker.mjs`, Cloudflare Worker `amux-cloud` at https://cloud.amux.io (live Stripe) and `amux-cloud-test` on workers.dev (Stripe test mode, needs `x-test-key`) |
| Deploy | `deploy.sh live` or `deploy.sh test` |
| Server image | the newest Hetzner snapshot labelled `amux-golden` (`cloud/control/build_image.py --ref <sha>`) |
| End-to-end test | `e2e.mjs` (see below) |
| Offline tests | `node cloud/start/test-worker.mjs`: the real module against fake Hetzner, Cloudflare and Stripe, with injected faults |

Flow: the page hashes the password in the browser (scrypt, the door's format) and posts plan,
name, email and hash to `/api/checkout`. Stripe Checkout takes payment. On
`checkout.session.completed` the function creates the server from the golden image with
`/etc/amux/standalone.env` (domain and the one login) and the DNS record. The server's bootstrap
gets its own Let's Encrypt certificate and runs its first self-update. A cron every minute
sends the ready email as hello@amux.io once the server's `/_door/status` shows a commit, a
dashboard that loads and an update verdict. On `customer.subscription.deleted` it snapshots the
server (kept 30 days), then deletes the server and the record. Deleting the account from the
billing page (an emailed one-hour link) ends the subscription and erases everything at once.

Every step of provisioning is idempotent. If the webhook fails part way (no Hetzner capacity, a
DNS error), Stripe redelivers it and the cron's `reconcile()` finishes any paid subscription
whose signup is unfinished (its `pw_hash` is still in Stripe metadata). An order still
unfinished after 10 minutes emails hello@amux.io once. The cron also checks each ready server
every 10 minutes (reachable, updater healthy, backups on, DNS record right) and emails after two
failed checks in a row.

State lives in Hetzner labels (`amux-sub`, `amux-slug`, `amux-mode`, `amux-ready`, `amux-ending`)
and Stripe metadata. The password hash is removed from Stripe once the server exists.

## Activation and friction (is a paying customer stuck?)

Once an hour the cron reads each ready server's activation report, `GET /_door/report` on the
server's door (`cloud/desktop/door.py`, shipped to existing servers by their hourly self-update).
The request carries a two-minute token signed with the same per-server key as the app's sign-in
links (derived from `DOOR_ENTRY_SECRET`), with `scope: "report"` and no person in it, so it cannot
sign anyone in. Anything else gets a 404.

What the report holds, all counts or booleans:

| Section | Fields |
|---|---|
| signin | ever signed in, sign-in count, first/last sign-in time, last signed-in visit, failed sign-ins (24h), when counting began |
| model | which providers are connected (`claude`, `codex`, `gemini`... true/false) |
| workers | total, ever ran (used any tokens), active in the last 24h, running |
| tokens | tokens used today |
| board, schedules | card count; schedule count and how many are enabled |
| errors | error groups and errors in the last 24h (amux-server's own `/api/logs/analyze`) |
| version, disk | running commit; root disk used % |

What it never holds: message text, prompts, worker or file names, paths, emails, card titles,
schedule commands, keys or tokens. The door keeps only sign-in counts and times in
`/etc/amux-door/activity.json`, with no names or addresses. Each section reports `measured`, and a
section that could not be read says why instead of reading zero. Sign-ins are counted from the
door update onward (`tracking_since`), so a server older than that is never called "never signed
in"; it is flagged only after 24 counted hours with no sign-in and no signed-in visit.

Where it shows up:

- Labels: `amux-act` (the setup stage: `not_signed_in`, `no_model`, `no_worker`, `idle`, `working`),
  `amux-rep` (the hour the report was last read), `amux-errg` (yesterday's error-group count).
- `GET /api/ops` and the 13:00 UTC email: an `activation` row per server and a `friction` list. The
  email now also goes out when a customer is stuck. Rules: never signed in after 24h, no model after
  24h, no worker ever after 48h, error groups at 25 or more (or 10 or more and double yesterday's),
  10 or more failed sign-ins in 24h, disk over 85%. A server whose door has no report yet shows
  "no report" and is not counted as stuck.

Setup nudges to the customer, from hello@amux.io: a day after the server is created with no model
connected, how to connect one; three days in with no worker ever run, how to start one. Each goes
once per server (`amux-nudged`), at least a day apart (`amux-nudge-at`), and never to a server
older than 14 days. **Off unless the Worker runs with `NUDGES=on`** (`NUDGES=on cloud/start/deploy.sh
live`; a deploy without it turns them off again). Held nudges are logged as `nudge.held`. Every
nudge says to reply "stop"; when someone does, set the label `amux-nudge-off=1` on their server.

## Moving a local amux in

The customer is the server's owner (`AMUX_WEB_USERS` entry marked `owner`, no member token), so
the door's Basic auth with their email and password is all an import needs:

```bash
amux export                                    # on the local machine: amux-<host>-<date>.amuxmove.json
amux import amux-*.amuxmove.json --to https://<slug>.amux.io --user <email>   # dry run, then y
```

The password comes from `AMUX_IMPORT_PASSWORD` or a prompt, never argv. Import adds rows and
never overwrites; teammates (org members) are refused with 403. Format and rules:
`docs/design/cloud-self-service.md` section 7.1, `crates/amux-server/src/api/move_bundle.rs`.

## Testing it

Stripe runs an invisible hCaptcha on Checkout that holds back automated browsers (Playwright,
headed or headless, test or live mode). The visitor journey therefore runs in a real Chrome
through `amux cdp` (2026-10-17 run: paid to ready in 69 s, door sign-in, dashboard, portal
cancel, deletion in 97 s). `e2e.mjs` covers everything up to Checkout in Playwright and the rest
when Stripe lets it through.
