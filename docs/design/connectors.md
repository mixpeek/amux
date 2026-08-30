# Connectors platform design

Status: proposed (AMUX-3101). Owner: `amux`. Captured from Ethan's 2026-08-14 vision, block 3.

> **2026-08-20 update (AMUX-3418/AMUX-3192):** build-order step 4 — the OAuth broker —
> is LIVE in `api/connectors.rs`: `auth` (state + PKCE + pending), the public
> `callback` (code→token exchange, per-account store at
> `~/.amux/connectors/<family>/<account>.json`), `token` (SA or stored user grant,
> broker-owned refresh), and `GET /api/connectors/accounts` (per-account health +
> the one reconnect action). One refinement over the text below: Google tokens are
> stored per FAMILY, not per provider — a single union-scope grant (gmail + calendar
> + drive/docs) per account covers every Google connector and is mirrored into
> `~/.amux/gmail-tokens/` so the email subsystem is repaired by the same approval.
> Token rot auto-files one board card per account (autofix `connector-auth` detector).

A "connector" is amux's word for a configured third-party integration: Gmail, Slack,
Google Drive, a Mixpeek API, an MCP tool server. Today these are scattered across
bespoke code paths. This doc defines one coherent surface for them, built entirely from
primitives amux already has, and names the one gap that has to be closed to make it real.

## What Ethan asked for (the cards this serves)

- **AMUX-3101** (this doc): pick the most robust connector platform and scope each
  connector at global / group / worker.
- **AMUX-3105**: a Connectors tab at both global and worker level, OAuth in-browser.
- **AMUX-3102**: remove the MCP tab, superseded by Connectors.
- **AMUX-3103**: Gmail connector, multiple accounts scoped to different workers
  (`info@`/`ethan@mixpeek.com` for mixpeek workers, personal for refresh.house), with
  unhappy-path validation.
- **AMUX-3104**: Slack connector, OAuth via the browser.

## The thesis: a connector is a composition of four primitives, not a new subsystem

Per the amux ethos ("build on the primitives, never reinvent or abstract them"), a
connector decomposes cleanly into things that already exist:

1. **scope**: where a connector is turned on and mapped to an account, per
   global/group/worker. This becomes a **new 7th scope capability, `connectors`**,
   sitting beside `memory`, `rules`, `env`, `gates`, `skin`, `status_mode`
   (`crates/amux-server/src/api/scope.rs:91-157`). The scope module's own docs already
   argue that a new vertical should become a scopable capability rather than its own
   `/api/x` (`scope.rs:130-138`), so this is the sanctioned shape.

2. **env**: where a connector's secrets and tokens live. Static provider secrets
   (`client_id`/`client_secret`, API keys) stay in `~/.amux/server.env` and are
   inventoried by name in `docs/credentials.md`. Refreshable OAuth tokens live in a
   per-account file store, mirroring today's `~/.amux/gmail-tokens/<email>.json`.

3. **MCP**: the tool surface a session actually calls. A connector's capabilities reach
   the model as MCP tools. `--mcp-config` is already passed to every session by default
   (`session_verbs.rs:4460-4464`, gated only by `CC_MCP` ∈ {off,none,0}), so the
   ethos-rule-1 "configured but reaches nobody" problem is already half-solved. What is
   missing is per-scope MCP content (see the gap below).

4. **browser**: the interactive OAuth consent handoff. `/api/browser/*` already opens a
   persistent profile for a human to sign in once, then reuses it
   (`~/.amux/playwright-auth/profiles/<name>`, `integrations/browser.rs:9-27`;
   `profile/create` + `stop`-to-flush at `browser.rs:250-288`). OAuth consent is exactly
   this flow pointed at a provider's auth URL.

There is no fifth thing to build. The Connectors tab is a view over these four; the
connector runtime is a resolver that composes them for a launched session.

## The one real gap: scope layers are written but not resolved into sessions

This is the load-bearing finding, and connectors cannot ship without it.

The `env` scope capability already writes three layers: global `~/.amux/amux.env`,
group `~/.amux/env/<group>.env`, worker `~/.amux/sessions/<worker>.env`
(`scope.rs:190-196`). But at launch, **only the global `amux.env` is sourced**
(`session_verbs.rs:4759-4764`); the group and worker layers are never composed into the
running session. The per-worker `env-explain` / `memory-explain` endpoints that would
resolve them answer **501** and are documented in-tree as a named residual gap
(`session_verbs.rs:8196-8201`).

So "scope a connector to a worker" has nowhere to land today, because scoping anything
below global to a worker does not currently reach the worker. Connectors must drive the
general fix, **resolving the global→group→worker scope layers at session launch**, and
that same fix repays the existing `env` capability, MCP content scoping, and any future
scoped capability. This is a primitive getting better, not a connector special-case.

## Decision: native OAuth broker first, Nango deferred (not adopted now)

The captured card leaned toward Nango (self-hosted) as the OAuth lifecycle engine. After
reading the code, the recommendation is to **generalize amux's own working Gmail OAuth
broker first, and hold Nango as a documented later option**, for three concrete reasons:

- **Gmail is already a fully-worked native OAuth precedent** to mirror, not a greenfield:
  PKCE S256, offline+consent, a pending-state store, a per-account token file, live
  refresh, and a health probe that maps `invalid_grant` → `needs_reauth`
  (`gmail_auth.rs`, `integrations/email.rs`). Adopting Nango would replace a working,
  understood component with a heavier one.
- **Nango is a standing service** (its own server plus Postgres and Redis). The
  single-codebase rule (`CLAUDE.md`) says the cloud image is the same binary as local
  with no cloud-only branches; a Docker sidecar with two datastores is a real operational
  weight to carry for what starts as two providers.
- **Get out of the model's way** (ethos): a native broker keeps the primitives visible
  and composable. Nango is an abstraction over "how connectors compose," which is exactly
  the kind of layer the ethos warns becomes the ceiling.

The decision is staged, and the trigger to revisit is explicit: **if the maintained
provider count crosses ~5, re-evaluate Nango as the lifecycle engine sitting behind the
`connectors` scope capability.** Recording it here so it is a weighed choice, not a gap
someone re-litigates.

## Data model

- **`connectors` scope capability** (`scope.rs` `SCOPE_CAPS`): `kind: json`,
  `levels: global/group/worker`, `merge: merge-by-key`, mirroring `skin`. Value shape per
  scope: `{ "<connector>": { "enabled": true, "account": "<id>", "mcp": "<server-name>",
  "write": "read_only" | "allow", "deny_channels": ["#cust-*"], "deny_recipients": ["*@customer.com"] } }`.
  This says "this worker/group/global uses Gmail as `info@mixpeek.com`, and it may read but
  never send to a customer address." The `write`/`deny_*` fields are the write-safety policy
  (see below); absent, a connector defaults to `read_only` so a mis-scoped connector fails
  closed rather than sending.
- **Provider registry** (static, in-tree): per provider, the OAuth2 endpoints, scopes,
  and the MCP server template. Small enough to be a Rust const table, like `GMAIL_SCOPES`.
- **Token store**: `~/.amux/connectors/<provider>/<account>.json`, chmod 600, outside the
  repo, generalizing `~/.amux/gmail-tokens/`. Refresh owned by the broker.
- **Provider client secrets**: `~/.amux/server.env` (e.g. `GOOGLE_OAUTH_CLIENT_ID`,
  `SLACK_CLIENT_ID`/`SLACK_CLIENT_SECRET`), added to `docs/credentials.md` by name only.

## OAuth broker

Generalize `gmail_auth.rs` into `/api/connectors/<provider>/{auth,callback,refresh,revoke}`:

- `auth` builds the provider auth URL (PKCE where supported) and stores pending state.
- `callback` (public route, like the Gmail callback) exchanges the code and writes the
  token file.
- `refresh` / `revoke` manage lifecycle; `/status` reports live health per account, as
  Gmail's `/accounts` already does via a real round-trip cached 300s
  (`gmail_auth.rs:465-553`).
- **redirect_uri must be built from the canonical base, never a hardcoded port.** Gmail's
  is pinned to the retired `:8822` at `gmail_auth.rs:62`, and a runtime guard already
  detects the mismatch against `config::canonical_port()` and warns while still handing
  out a dead URL (`gmail_auth.rs:277-299`). The broker derives redirect_uri from
  `canonical_port()` so the guard has nothing to catch. (Board note: this defect is
  AMUX-3026 on the board but AMUX-2943 / AC-337 in the code comments, likely duplicate
  cards to reconcile.)

Interactive consent hands off to `/api/browser/*`: open a per-connector persistent
profile, navigate to the auth URL, let the human complete consent, flush the profile on
`stop`. Reuse the saved profile via CDP for re-auth. Note AMUX-3063 first: `/api/browser/start`
currently defaults to a shared slot where peers kill each other, so connector OAuth needs
its own named profile, not the shared one.

## MCP delivery, made scope-aware

Today MCP content is global-only (`~/.amux/mcp.json`, every worker gets the whole set);
the only per-worker control is the `CC_MCP` on/off toggle
(`session_verbs.rs:4460-4471`, `:11057-11069`). Connectors turns this into a resolved,
per-scope set: for a launching worker, resolve its enabled connectors from the
`connectors` scope layers, generate the effective MCP config (only the servers that
worker is entitled to, with that worker's account token), and pass that as
`--mcp-config`. This reuses the launch path that already exists and depends on the
scope-resolution fix above.

## UI: the Connectors tab (replaces the MCP tab)

- Two levels: global and worker (the tab reads its scope like the Scope tab does).
- Per connector: name, status (connected / needs-reauth / not-connected), account
  mapping, and the scope it applies at. A "Connect" action launches the in-browser OAuth
  flow; a "Disconnect" revokes.
- The existing MCP tab (`index.html:1506-1514`, `app.js:25245-25327`) is removed
  (AMUX-3102). MCP becomes plumbing beneath a connector, not a raw registry a human edits.
- Client changes bump `APP_VER` (`app.js`) and `CACHE` (`sw.js`) together.

## Unhappy paths (the AMUX-3103 eval, and the two-fixes rule)

Wrong account, unconnected, or revoked token must fail loudly and **surface in the logs**,
not silently no-op:

- A tool call for a connector a worker is not entitled to returns a structured error
  naming the connector and the worker, and increments a counter visible to
  `GET /api/logs/analyze`.
- A revoked token surfaces as `needs_reauth` (Gmail already does this) and raises a
  "needs human sign-in" signal (ties to AMUX-3073, which notes agents currently have no
  such signal and no deep link to the connect UI).
- The e2e scenario (block 3, AMUX-3093/3094) exercises each of these against the real UI
  and API.

## Write safety: read broadly, write narrowly (AMUX-3271, Ethan's verify state)

Ethan's governing constraint for connectors, stated as the acceptance criteria: a
connector may **read** widely, but its **writes** must never reach a customer. Concretely:

- Slack: read and write our own channels, but **never post in a customer channel**.
- Email: read an inbox, but **never send an outbound message to a customer or user**.

This is not covered by the entitlement checks in "Unhappy paths" above (those answer "is
this worker allowed to use Gmail at all"). It is a second, finer gate on the write verbs
of an entitled connector, and it is the highest-consequence property of the whole feature:
an over-broad connector that auto-emails a customer is the failure that matters.

Design, in primitives (this is scope + a guarded verb, not a new subsystem):

- **Policy lives in the `connectors` scope value**, the `write` / `deny_channels` /
  `deny_recipients` fields above. It resolves global -> group -> worker like every other
  scope layer, so "read-only for this whole group, one worker may send from `info@`" is
  expressible without special-casing.
- **Fail closed.** A connector with no explicit `write` is `read_only`. A write verb whose
  target matches a `deny_*` rule, or that cannot be evaluated against the policy, is
  refused before the provider call, never after.
- **The refusal is a first-class, logged event, not a silent no-op** (two-fixes rule): a
  blocked write returns a structured error naming the connector, the worker, and the
  matched rule, and increments a counter surfaced by `GET /api/logs/analyze`, so the next
  time a connector is one deny-rule away from mailing a customer, a log sweep sees it
  without a human noticing first. The e2e (AMUX-3093/3094) asserts a customer-targeted
  write is refused AND that the refusal appears in the request log.
- **This composes with the harness boundary already in force**: sending a message on the
  user's behalf is a permissioned action. The write guard is amux enforcing that same
  boundary at the connector layer, so an agent cannot route around it by calling a tool.

Customer identity (which addresses and channels are "customer") is itself data amux
already holds: the CRM contacts and the customer-tagged worker groups. The deny lists seed
from those rather than a hand-maintained blocklist, so a new customer added to the CRM
tightens the guard automatically.

## Build order

1. **AMUX-3101**: this doc.
2. **Scope resolution at launch**: compose global→group→worker layers into a session;
   closes the 501 gap (`session_verbs.rs:8196-8201`) and repays `env` + MCP scoping. This
   is the prerequisite for everything below and is worth a card of its own.
3. **`connectors` scope capability** + provider registry + token store.
4. **OAuth broker** generalizing `gmail_auth.rs`; redirect_uri from `canonical_port()`
   (folds in AMUX-3026 / AMUX-2943).
5. **Write-safety guard** (AMUX-3271): the resolved `write`/`deny_*` gate on every
   connector write verb, fail-closed, with the logged-refusal signal. Lands with the first
   writing connector and is asserted by the e2e; it is Ethan's verify state, so no writing
   connector ships without it.
6. **Gmail on the new broker**: multi-account, per-worker scope, unhappy-path eval
   (AMUX-3103), read + guarded send.
7. **Connectors tab**; remove the MCP tab (AMUX-3105, AMUX-3102).
8. **Slack connector** (AMUX-3104), read + guarded post (never a customer channel).
9. **Nango re-evaluation gate**: only if provider count crosses ~5.

## Open questions for Ethan

- **Write-safety (AMUX-3271) is now folded in as a first-class gate above.** Confirm the
  fail-closed default (a connector with no explicit policy is read-only) and that seeding
  the customer deny-lists from the CRM + customer-tagged groups is the right source of
  truth, rather than a hand-kept blocklist.
- **Slack vs Gmail first.** The verify state names both. Build order does Gmail first
  because it is a worked native OAuth precedent to generalize; Slack reuses that broker.
  Say if you want Slack led instead.
- The staged native-first / Nango-deferred call: confirm, or say "stand up Nango now."
- Scope-resolution-at-launch (step 2) is a meaningful new card the vision did not name
  explicitly. It is the true foundation; flag if you want it split out or folded into
  AMUX-3103's runtime.
- Provider secrets go in `server.env` (`GOOGLE_OAUTH_CLIENT_ID`, `SLACK_*`). Gmail's
  client config is currently a separate `~/.amux/gmail-oauth-client.json`; the broker can
  keep that per-provider-file shape or move to `server.env`. Default: keep the file shape
  Gmail already uses.
