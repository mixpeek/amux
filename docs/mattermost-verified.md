# Mattermost Connector — What's Actually Verified

**Last updated**: 2026-08-30

This file previously asserted its own review outcome ("✅ Security review: PASSED",
"✅ CI: APPROVED") and one claim that was checkably false. Corrected per review
(esteininger, PR #164): a doc records what was run and by whom, it does not certify
the review it is requesting.

## Automated coverage (reproducible by anyone)

`crates/amux-server/src/api/connectors.rs`, module `tests`, exercises
`Auth::LoginPassword`'s full `begin_auth` path through the existing
`MockHttp`/`app_with` harness — no network, no real Mattermost instance required:

```bash
cargo test -p amux-server --lib api::connectors::tests::login_password
```

- `login_password_success_stores_token_and_user_id` — credentials in
  `server.env`, a scripted `POST /api/v4/users/login` response carrying the
  `Token` header, asserts the resulting store file's `token`/`user_id`/`base_url`.
- `login_password_failure_surfaces_error_and_stores_nothing` — a rejected
  login (401 + Mattermost's own message) surfaces that message and, just as
  importantly, leaves no store file behind (a partial/absent token on disk
  would read as "connected" on the next status check).

Reaching this branch through `MockHttp` required extending the shared
`HttpTransport` trait with `post_json_with_header` (`integrations/email.rs`):
`mattermost_login` used to build its own private `reqwest::Client` because the
session token comes back in Mattermost's `Token` response HEADER, which none
of the trait's existing `(status, body)`-shaped methods could carry — that
private client sat outside the one seam `MockHttp` intercepts. It is now
implemented for both `ReqwestTransport` (real HTTP) and `MockHttp` (tests).

## Correcting a specific false claim

The previous version of this file stated, under "Integration Readiness":

> Persists only the token (credentials never reach disk).

This was checkably false. The password IS on disk: it is read by
`resolve_cred_in` from `server.env`, which is this repo's sanctioned place for
credential VALUES (`CLAUDE.md`'s Server config section). That design choice is
fine — `server.env` is exactly where credentials belong — but the claim that
credentials never reach disk was not true, and this file should not have said
it.

## Manual verification against a real server

The original implementation's author reported testing this live: `/link`,
inbound routing, an unlinked-chat nudge, and outbound send all working against
a real Mattermost instance. That report is not independently reproducible from
this file — it named no host, no output, and nothing a second person can
re-run — so it is recorded here as **the author's claim**, not as something
this review re-verified. If you re-test against a real instance, please
replace this section with the host (or a redacted identifier), the date, and
what you actually observed.

## How it works

1. Server URL, login, and password are pasted via the connector's credentials
   form and written to `~/.amux/server.env`.
2. `POST /api/connectors/mattermost/auth` exchanges them for a session token
   via `POST {base_url}/api/v4/users/login`.
3. The token (plus `base_url` and `user_id`) is stored at
   `~/.amux/connectors/mattermost/<account>.json`.
4. Subsequent API calls use the stored token as a bearer credential.

No third-party OAuth broker is needed — this is the self-hosted-service
advantage `Auth::LoginPassword` exists for.
