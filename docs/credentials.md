# Credentials — what exists, what it is for, where the value lives

**Values live in `~/.amux/server.env` (chmod 600), which is outside every git repo on
this machine. This file documents NAMES ONLY and is safe to commit. Never paste a value
here.**

This inventory exists because of a failure that is cheap to repeat: on 2026-08-06 a
session spent a round trip asking Ethan to create a Clerk account and to choose whether
to hand over a `sk_live_` key — while `E2E_CLERK_SECRET_KEY` (an `sk_live_` key) and a
full `AMUX_QA_*` god-mode login were already sitting in `server.env`, in the same file
that session had been reading `E2E_COOKIE_SECRET` from all day. A credential you cannot
enumerate is functionally a credential you do not have. Grepping one key out of a file
never reveals the other 34.

## How this is meant to work

- **Value** → `~/.amux/server.env` only. Not the repo (`~/Dev/amux` is PUBLIC), not a
  board card, not a prompt.
- **Name + purpose** → this file, committed, so the next session can answer "do we
  already have X?" without reading a secrets file or asking a human.
- **Adding one:** put the value in `server.env`, add the row here, then restart the
  server to reload: `launchctl kickstart -k gui/$(id -u)/com.amux.server-rs`.

Read the names you have with:

```bash
grep -oE '^[A-Z0-9_]+=' ~/.amux/server.env | tr -d '=' | sort
```

## God mode / test identities

| Key | Purpose |
|---|---|
| `AMUX_GODMODE_EMAIL` | `hello@amux.io` — god-mode identity for cloud.amux.io. In the gateway's `ADMIN_EMAILS`, so it sees every workspace. |
| `AMUX_GODMODE_PASSWORD` | Its Clerk password. NOTE: password alone does NOT complete a sign-in — the instance requires an `email_code` second factor even though the user has `two_factor_enabled: false`, so this is instance config rather than per-user MFA. Automation should NOT use this path: reading the code needs the hello@amux.io mailbox, which couples login to a revocable Gmail OAuth grant (it was revoked on 2026-08-07 and the failure surfaced as "Clerk did not send the code"). Use `AMUX_GODMODE_USER_ID` + `E2E_CLERK_SECRET_KEY` instead — see AC-282. |
| `AMUX_QA_EMAIL` | `qa-godmode@amux.io` — the PRE-EXISTING god-mode account. Also in `ADMIN_EMAILS`. Check this before provisioning another one. |
| `AMUX_QA_PASSWORD` | Its password. |
| `AMUX_QA_USER_ID` | Its Clerk user id, for admin API calls that take a user id. |
| `AMUX_GODMODE_USER_ID` | hello@amux.io's Clerk user id. Used with `E2E_CLERK_SECRET_KEY` to mint a backend sign-in token, which needs NO second factor — that is how the walkthrough rig logs in without reading a mailbox (AC-282). |
| `E2E_COOKIE_SECRET` | HMAC secret for minting an `amux_session` cookie directly, bypassing Clerk. The e2e/admin back door — treat as god mode. |
| `E2E_CLERK_SECRET_KEY` | Clerk **Backend API** key (`sk_live_`). Creates/updates users server-side, which skips the sign-up captcha that blocks the public flow. |

## Cloud infrastructure

| Key | Purpose |
|---|---|
| `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ZONE_ID` | DNS + WAF for amux.io. Relevant to the bot-challenge rules on `accounts.amux.io`. |
| `AMUX_S3_BUCKET`, `AMUX_S3_KEY`, `AMUX_S3_REGION` | Public iCal feed. `AMUX_S3_KEY` is a random token and must NEVER be committed — a guessable key leaked once. |
| `AMUX_TUNNEL_TOKEN` | Reverse proxy for exposing a localhost service. |
| `AMUX_MIXPEEK_OPS_TOKEN`, `AMUX_MIXPEEK_OPS_URL` | mixpeek ops API. |

## Third-party APIs

| Key | Purpose |
|---|---|
| `OPENAI_API_KEY` | OpenAI. |
| `GOOGLE_API_KEY`, `GOOGLE_MAPS_API_KEY` | Google APIs / Maps. |
| `ELEVENLABS_API_KEY` | Voice synthesis. |
| `LIGHTFIELD_API_KEY` | Lightfield. |
| `POSTHOG_KEY`, `POSTHOG_HOST`, `POSTHOG_PERSONAL_API_KEY`, `AMUX_POSTHOG_KEY` | Analytics. The personal API key can read/write project config; the others are ingest. |

## Notifications

| Key | Purpose |
|---|---|
| `AMUX_OWNER_PHONE` | Destination for `amux alert` SMS/iMessage. |
| `AMUX_URGENT_PUSH`, `AMUX_URGENT_SMS` | Enable/disable each urgent channel. |
| `AMUX_VAPID_SUBJECT` | Web-push VAPID contact. |

## Not credentials — behaviour flags

`AMUX_COMMIT_GUARD`, `AMUX_TASK_GUARD`, `AMUX_RATE_LIMIT_MODE`, `AMUX_NOTES_DIR`,
`AMUX_GCAL_ID`, `AMUX_BU_DRIVER_PYTHON`, `AMUX_ALLOW_AGENT_SESSION_DELETE`. Listed so
their presence is not mistaken for a secret.

## Other persistence surfaces — keeping it out of git is necessary, not sufficient

A secret can be committed, but it can also be written to places git never sees. Both have
happened here:

- **Board cards.** Prompt auto-capture wrote a god-mode password into a card's title and
  desc verbatim (AC-251, redacted). An Anthropic API key sat in a board card (AC-156).
- **`session_events`.** The prompt-capture log is append-only and still holds at least one
  raw credential. Do NOT rewrite an audit log to hide a secret — **rotate**, which makes
  every persisted copy inert, wherever it landed.

Tracked as AC-160 (unredacted prompt capture).

ROTATION: recommended for AC-239, AC-214, AC-156 and `AMUX_GODMODE_PASSWORD`, and DECLINED by the owner — Ethan, 2026-08-06 14:52: "no need to rotate just continue." Recorded so this reads as a weighed decision by the party entitled to make it rather than an outstanding action nobody picked up. The exposures themselves are unchanged and still described on those cards.

## Verifying nothing is committed

The repo's own pre-commit hook (`scripts/git-hooks/pre-commit`) scans staged content. To
audit the tracked tree:

```bash
git grep -nIE '(sk_live_|sk_test_|AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----|xoxb-|ghp_[A-Za-z0-9]{20,})'
```

Expect hits that are **pattern mentions, not secrets** — the pre-commit hook's own rule
list, and the `site/guides/*` pages that document detection regexes. Confirm each hit is a
mention before reporting a leak, and confirm the probe can still find a real one before
trusting a clean result.

## Connectors (OAuth broker)

| Key | Purpose |
|---|---|
| `GOOGLE_OAUTH_CLIENT_ID` | Optional override for the shared Google OAuth client the connectors broker uses. When unset, the broker reuses `~/.amux/gmail-oauth-client.json` (the client Gmail already authorizes with), so normally NOTHING goes in server.env for Google. |
| `GOOGLE_OAUTH_CLIENT_SECRET` | Its secret (same override/fallback rule). |
| `SLACK_CLIENT_ID` | Slack app client id for the Slack connector (AMUX-3104). No fallback file — paste via the Connectors tab. |
| `SLACK_CLIENT_SECRET` | Its secret. |
| `GRANOLA_API_KEY` | Granola key-only connector (`grn_...`). |

Grant TOKENS (not credentials) live per account under `~/.amux/connectors/<family>/<account>.json`
and `~/.amux/gmail-tokens/<account>.json` — written by the broker callbacks, chmod 600, never
committed. `GET /api/connectors/accounts` enumerates them with live health.
