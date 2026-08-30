# Connectors E2E: gaps + the setup checklist only Ethan can do

Companion to `connectors.md` (the design). This is the actionable plan to get a
working, scoped (global / group / worker) connectors platform with the first
five connectors live and verified: **Granola, Gmail, Google Calendar, Google
Drive, Google Admin**. It separates cleanly into two columns: **what amux
builds** (me) and **what only Ethan can do** (OAuth apps, GCP, keys, admin
consent). Work the Ethan column one item at a time; each connector is then
verifiable end to end.

Status legend: [have] exists today · [build] amux work · [ethan] only Ethan can do.

---

## 1. What already exists (so we do not redo it)

- [have] **GCP project** `mixpeek-inference-463103` with **ADC** present
  (`~/.config/gcloud/application_default_credentials.json`) and a `GOOGLE_API_KEY`.
- [have] **A Google OAuth client** (`~/.amux/gmail-oauth-client.json`,
  `client_id` present, project `mixpeek-inference-463103`). One OAuth client can
  serve ALL the Google connectors by adding scopes, so we reuse this one.
- [have] **Working Gmail OAuth** for 4 accounts
  (`~/.amux/gmail-tokens/*.json`: esteininger21@gmail.com, ethan@mixpeek.com,
  hello@amux.io, info@mixpeek.com) via the existing `/api/gmail/auth` +
  `gmail_auth.rs`. This is the precedent the broker generalizes.
- [have] The **`connectors` scope capability** data model (`scope.rs`):
  per-level pref keys global / `group:<g>` / worker, and the
  `/api/connectors?worker=` explain endpoint. So "scope Gmail to this worker"
  already has somewhere to be WRITTEN.
- [have] **Calendar id** `AMUX_GCAL_ID` (the iCal feed) and Drive access via the
  Drive REST API + ADC (see `memory/gdrive.md`).

The one broken thing in the existing setup, which Ethan must fix (see below):
the OAuth client's redirect is pinned to the **retired 8822 port**
(`http://localhost:8822/api/gmail/callback`).

---

## 2. What amux builds (the technical gaps) — no Ethan input needed to START

Ordered; each is a card. None of these need Ethan, so I can build them while he
works the setup column in parallel.

1. **Scope-resolution-at-launch** (the foundation). Today only the global env
   layer reaches a launched worker; group/worker scope layers are written but
   never composed in, and `env-explain`/`memory-explain` answer 501. Until this
   lands, "scope a connector to a worker" has nowhere to take effect. This is
   the prerequisite for the whole feature and repays `env` + MCP scoping too.
2. **OAuth broker** — generalize `gmail_auth.rs` into
   `/api/connectors/<provider>/{auth,callback,refresh,revoke,status}`. PKCE
   where supported; `redirect_uri` built from `canonical_port()` (8824), which
   also fixes the dead-8822 pin (AMUX-3026/2943).
3. **Provider registry** — a small in-tree const table: per provider, its OAuth
   endpoints, default scopes, token-store shape, and MCP-server template. **This
   is the "trivial to add another connector" mechanism** — adding one is adding
   a registry row plus Ethan supplying that provider's client/key.
4. **Token store** — `~/.amux/connectors/<provider>/<account>.json`, chmod 600,
   generalizing `~/.amux/gmail-tokens/`. Refresh owned by the broker.
5. **Connectors tab** (frontend) — global + worker level, per connector:
   name, status (connected / needs-reauth / not-connected), account mapping,
   the scope it applies at, and Connect / Disconnect actions that drive the
   in-browser OAuth flow. Replaces the MCP tab.
6. **Per-connector clients** — the read/write verbs each connector exposes as
   MCP tools (Gmail, Calendar, Drive, Admin, Granola).
7. **Write-safety guard** (AMUX-3273) — read broadly, never write to a customer
   channel / customer recipient. Fail-closed, logged. Lands with the first
   writing connector.
8. **Scope-aware MCP delivery** — a launching worker gets only the connectors it
   is entitled to, with that worker's account token.

---

## 3. What ONLY Ethan can do — the setup checklist

This is the column to work one item at a time. Credential VALUES go into
`~/.amux/server.env` (never the repo); I inventory them by name in
`docs/credentials.md`.

### 3a. Google (covers Gmail, Calendar, Drive, Admin — shared setup)

All four Google connectors share ONE GCP project and ONE OAuth client; the
difference is which APIs are enabled and which scopes are consented.

- [ethan] **Pick the project.** Reuse `mixpeek-inference-463103`, or create a
  dedicated `amux-connectors` project (cleaner blast radius). Tell me which.
- [ethan] **Enable the APIs** in that project (APIs & Services → Enable):
  - Gmail API
  - Google Calendar API
  - Google Drive API
  - **Admin SDK API** (for Google Admin)
  - (optional, commonly needed) People API
- [ethan] **OAuth consent screen** (APIs & Services → OAuth consent screen):
  - App name, support email.
  - **User type:** Internal if every account is under a Google Workspace you own
    (simplest — no verification, no test-user cap); External otherwise (then add
    yourself + the accounts as Test users, or publish).
  - Add the scopes for the connectors you want (I'll give the exact scope
    strings per connector; e.g. `gmail.modify`, `calendar`, `drive`,
    `admin.directory.user.readonly`).
- [ethan] **Fix the OAuth client's redirect URIs** (Credentials → the OAuth 2.0
  Client): REMOVE `http://localhost:8822/...` and ADD the broker callback
  `https://localhost:8824/api/connectors/google/callback` (and the cloud one,
  `https://cloud.amux.io/api/connectors/google/callback`, if you want it in the
  cloud too). This is the single most likely thing to silently break the flow.
- [ethan] **Provide the client credentials** to `~/.amux/server.env` as
  `GOOGLE_OAUTH_CLIENT_ID` / `GOOGLE_OAUTH_CLIENT_SECRET` (or confirm I should
  read the existing `gmail-oauth-client.json`). I never see the value if you
  paste it via the dashboard Settings → API Keys field.
- [ethan] **Run the OAuth grant** (in-browser, once per account you want
  connected) when the broker is up — the Connect button drives it.

### 3b. Google **Admin** — the extra step (admin-only)

Google Admin (directory/users/groups) needs more than a normal OAuth grant:

- [ethan] You must be a **Workspace super admin** on the domain.
- [ethan] Choose ONE:
  - **Admin-consented OAuth**: grant the `admin.directory.*` scopes on the
    consent screen as the super admin (read-only to start:
    `admin.directory.user.readonly`, `admin.directory.group.readonly`), OR
  - **Service account + domain-wide delegation** (the robust path for
    background reads): create a service account in the project, enable
    domain-wide delegation, and in the **Workspace Admin console → Security →
    API controls → Domain-wide delegation**, authorize that service account's
    client-id for the exact admin scopes. Then I use JWT auth, no per-user
    OAuth. Tell me which path; I'll wire whichever you pick.

### 3c. Granola

Cleanest of the five: **Granola shipped an official REST API in April 2026**,
API-key auth, no OAuth. ("Lightfield" is a separate AI-CRM company that ships a
Granola MCP connector, which is why the names pair up — the source system is
Granola.)

- [ethan] **Be on a Granola Business ($14/user/mo) or Enterprise plan** — the
  free plan cannot mint an API key. This is the only gate.
- [ethan] In the **Granola desktop app → Settings → Connectors → API keys**,
  create a key (scope: Personal notes, and Public notes if you want workspace
  notes). It looks like `grn_...`.
- [ethan] **Provide the key** as `GRANOLA_API_KEY` in `~/.amux/server.env`.
- [build] amux calls `https://public-api.granola.ai/v1/notes` and
  `/notes/{id}?include=transcript` with `Authorization: Bearer grn_...` (rate
  limit ~5 req/s). No OAuth broker needed — Granola is a **key-only connector**,
  which the provider registry also supports (not everything is OAuth).

Fallback if NOT on Business: Granola's local cache is now encrypted (July 2026),
so the old `supabase.json`-token / cache-read hacks are broken or fragile. There
is no clean no-upgrade path; the API key is the recommendation.

### 3d. Anything you may be missing (candidates to include)

Given the fleet, these are the connectors that keep coming up and are worth
adding to the same tab now while the mechanism is fresh:

- **Slack** (already an Ethan card, AMUX-3104) — OAuth via browser; you create a
  Slack app + provide `SLACK_CLIENT_ID`/`SLACK_CLIENT_SECRET`.
- **GitHub** — the fleet already uses `gh`; a GitHub App or PAT would make
  issues/PRs/CI a first-class connector.
- **Notion / Linear** — you named them as NOT the board, but if you want them as
  read connectors, each is one OAuth app.
- **Apollo / Instantly** (GTM) — API keys, used by cold-outbound already.

---

## 4. The connector list, and "trivial to add another"

The whole point of the provider registry (§2.3) is that a new connector = a
registry row (endpoints, scopes, MCP template) + Ethan providing its client/key.
No new subsystem per connector. Adding "Notion" later is: one registry entry,
one `NOTION_CLIENT_ID/SECRET` in server.env, one OAuth grant. The tab renders it
automatically from the registry.

| Connector | Auth | Ethan provides | amux builds |
|---|---|---|---|
| Gmail | OAuth (have) | scopes + redirect fix | read/send (guarded) |
| Google Calendar | OAuth (shared client) | enable API + scopes | read/write events |
| Google Drive | OAuth (shared client) + ADC | enable API + scopes | read/write files |
| Google Admin | OAuth admin OR SA+DWD | admin consent / DWD | directory reads |
| Granola | API key (no OAuth) | Business plan + `GRANOLA_API_KEY` | notes/transcript pull |
| Slack | OAuth | Slack app id/secret | read/post (guarded) |

---

## 5. Verification (per connector, after each is set up)

For each: (1) the Connect flow completes and the token file is written; (2) a
read verb returns real data; (3) a write verb is refused for a customer target
and the refusal is logged (write-safety); (4) scoping works — a worker WITHOUT
the connector cannot use it and gets a structured, logged error. The e2e
(AMUX-3093/3094) exercises each against the real UI + API. `random` verifies the
tab renders and each connector shows the right status.

---

## The single decision that unblocks the most

Answer §3a's first two bullets — which GCP project, and enable the four APIs —
and I can wire Gmail/Calendar/Drive end to end against your real accounts. Admin
(§3b) and Granola (§3c) are the two that need their own decision; everything else
is mechanism I build regardless.
