# amux attack surfaces

The map the red-team loop (`RED-TEAM-LOOP.md`) hunts over. Written from origin/main
at `5d552f021` on 2026-11-02, by reading code. Line numbers drift, so treat them as
where to start reading.

This repo is public. This file names trust boundaries and what enforces them, which
anyone can read from the code. Open weaknesses never go here. They go in the private
log named in the loop until they are fixed.

## The trust model in one paragraph

amux runs as one OS user on one machine. Every worker, hook and shell schedule shares
that uid, so the filesystem and the loopback interface are one trust zone: a process
on the box is close to the owner by construction, and the README and `SECURITY.md`
say so. Inside that zone the boundaries between workers (origin stamps, groups,
approval holds, the guard) are policy and audit. The hard boundaries are the ones at
the edge of the zone: the network listener, public links, the cloud gateway. An agent
that reads untrusted text sits inside the zone, which is why prompt injection is the
central threat to reason about.

## Who can reach what

| Position | Starts with |
|---|---|
| Random web page in the owner's browser | Cross-site requests to `localhost` or a tailnet name, DNS rebinding attempts |
| Holder of a `/pub` link | The published app or file only, relayed with cookies stripped |
| Tailnet or LAN peer | The TLS listener on `0.0.0.0:8824`: public routes without a token, the rest with one |
| Untrusted content (web page, email, pasted text, repo file, MCP tool result) | Whatever the agent reading it will do |
| Peer worker | Loopback API, same uid, shared filesystem, messages within its groups |
| Local process | Everything loopback can do |
| Cloud user | Their own org's container through the gateway |
| Malicious dependency | Code at build or install time on this Mac or a CI runner |

## Surfaces

Paths under `crates/amux-server/src/` are written from `api/`, `runtime_jobs/` and
`integrations/`.

### HTTP API

- Listeners: main TLS listener with plain HTTP redirected (`lib.rs:1265`), apps origin on port+1 serving only `/app/*` (`lib.rs:1303`, `api/static_files/apps_origin.rs:159`).
- Layer order, outermost first (`api/mod.rs:785-811`): member identity, request log, gzip, interactions, chat delegate read-only, cross-site guard. Inside the protected router: `require_bearer`, `worker_identity::enforce`, `policy::enforce` (`api/mod.rs:637-647`).
- Admission (`api/auth.rs:68-140`): owner bearer in constant time, verified member cookie with a scope check, loopback socket peer, then public paths.
- Cross-site writes and WebSocket upgrades are refused by `api/cross_site.rs:26-63`. `/pub/` and `/app/` are exempt.
- Routes outside `require_bearer` are merged around `protected` at `api/mod.rs:650-758`. Any route added there is public to the LAN and tailnet, so that block is the first place to read after any change.

### Worker identity and the origin stamp

- `X-Amux-Session` names the caller. Rule 10 lanes must also present a minted token on mutating calls (`api/worker_identity.rs:191-225`, `docs/orchestration-contract.md`).
- `send_post` stamps the origin server-side (`api/session_verbs.rs:30210`, stamp at `:15028`), gates isolated workers (`:30031`) and checks group reach (`peer_input_refusal`, `:29430`).
- Delivery into a pane uses `tmux send-keys -l` and `paste-buffer -p` (`:1485`, `:1503`).
- Chat delegates are read-only by header-suffix match plus the provider tool denylist (`api/chat_delegate.rs:778`).

### Scheduler and other process spawners

- A shell schedule runs `/bin/bash -o pipefail -c` as the server user with the owner lane's vault secrets and a fresh worker token (`runtime_jobs/scheduler.rs:2115-2180`, `:1680` for the 600 s limit).
- Owner locks protect owner-disabled rows (`api/schedules.rs:582-595`).
- Other spawners: `/api/terminal/create` (`api/terminal.rs:124`), `/api/sql` writes (`api/sql.rs:559`), MCP config passed to `--mcp-config` on launch (`api/mcp.rs`, `api/session_verbs.rs:15704`).

### Public links, pages and file serving

- Link create and approve: `api/public_links.rs:419,520`. The id is the credential. Responses are forced into a CSP sandbox and `Set-Cookie`, `Cookie` and `Authorization` are stripped (`:641`, `:778`).
- Publish guards: `refused_port` (`:152`), `resolve_shared` containment (`:596`), `file_refused` (`:177`).
- `/api/file/raw` and `/api/fs` take absolute paths subject to `is_path_allowed` (`api/fs.rs:315-330,968`). Scoped members are limited to worker directories (`api/org.rs:429`).
- Tunnels: the ChatGPT relay with a path allow-list (`runtime_jobs/tunnel.rs:153`), and `tailscale serve` from the Join tailnet action (`api/tailnet_join.rs:148`).

### Dashboard and apps

- `crates/amux-dashboard/static/app.js`: worker output, cards and messages render through `esc`, `escHtml`, `_sanitizeHtml` (DOMPurify) and `ansiToHtml` (`:9624`, `:19493`, `:45964`). The service worker has root scope.
- The shell carries the owner bearer only for a browser on this machine (`api/static_files.rs:407`, `api/fs.rs:1165-1195`).
- Mobile and Mac apps wrap the same origin: `ios/`, `android/`, `desktop/`. The Android network security config is at `android/AmuxApp/app/src/main/res/xml/network_security_config.xml`.

### amux cloud

- Tenancy: one compose stack and volume set per org, port bound to loopback (`cloud/docker/docker-compose.template.yml:54-130`), shared `gateway.db` for orgs and memberships.
- Gateway to container: inbound `X-Amux-*` stripped, then `X-Amux-User-Email` written (`cloud/gateway/gateway.py:2084-2091`). Org membership check at `:2741`. Cookie HMAC at `:1570`. Clerk JWT verified with RS256 and JWKS (`:1631`).
- MCP front door (`cloud/gateway/chatgpt_front.py`): PKCE S256 (`:95,507,666`), redirect allow-list (`:110-125`), scope, resource and client binding (`:509-513,663`), grants bound to user, org and workspace (`:556-575`).
- Admin routes check `is_admin` (`gateway.py:3314-3660`). Dedicated servers sit behind the door on loopback (`cloud/desktop/door.py:1227`), and Caddy drops client member headers (`cloud/desktop/bootstrap.sh:668`). Releases are signature-checked (`cloud/desktop/fleet_agent.py:276`).
- Stripe webhook: `gateway.py:2383-2398`. Tunnels: `gateway.py:1843-1910`.

### Server-side MCP

- `/mcp` and `/oauth/*` authenticate per call (`api/chatgpt_app.rs:19-33,431`). Tools run over loopback (`:918`). Relayed approvals are refused (`:1098`).
- Third-party MCP servers come from the amux home's `mcp.json` and repo `mcp.json`. Their tool descriptions and results are untrusted input to every worker that loads them.

### Boundary guard

- `scripts/hooks/boundary-guard.py` with `boundaries.toml`, listed by `api/boundaries.rs`. Design at `docs/design/worker-boundaries.md`, which states what it is: a text-matching tripwire on tool calls. It stops honest mistakes and leaves an audit trail, so a compromised agent must be assumed to get past it. Do not count it as a wall when rating severity.

### Browser and CDP relay

- Proxy on an ephemeral loopback port (`api/cdp_proxy.rs:75`). `cdp_reason` is audit (`:39,163`).
- Saved auth profiles, profile import at `api/browser_import.rs:283`, sandbox ports on loopback (`integrations/computer.rs:498`).
- Screenshots of signed-in pages can go to a method endpoint set by `browser-methods.toml`, covered by the guard's self-protection rule.

### Email

- `/api/email/*`: external sends from a worker are held for approval (`api/email.rs:552`, `api/email_approval.rs`). Every send is in `/api/email/log`. Token storage: `integrations/email.rs:1014`.

### Payment vault and grants

- `amux order` fills through `api/vault.rs` (`:877-885`). Allowances and spend caps at `:193-213,826`. Card fields reach allow-listed provider iframes (`:646`) and the top document (`:703`). Grants: `api/grants.rs:334-400`. Secrets: `api/vault_secrets.rs:917`. Store: `integrations/secure_store.rs`.

### Stored credentials

- `server.env` in the amux home (masked in `api/settings.rs:564`, `api/connectors.rs:1039`), provider keys imported into pane env (`api/session_verbs.rs:17182-17374`), mail tokens, the GitHub App key, the owner bearer, the vault key in the macOS keychain. `/api/connectors/{id}/token` mints a live bearer (`api/connectors.rs:2712`). Names only: `docs/credentials.md`.

### Untrusted content reaching agents

- Web pages through the browser routes and WebFetch, email through `/api/email/inbox`, pasted text through the composer (tagged by the harness), MCP results, repo files and CLAUDE.md in any checkout, peer messages, card text, and advisory text in this loop itself.
- A compromised agent can use anything a worker can: every surface above from the "peer worker" position.

### Supply chain and CI

- Locks: `Cargo.lock`, `package-lock.json`, `business-ui/package-lock.json`, `ios/Gemfile.lock`, watched by `scripts/advisory_watch.py`. `integrations/procwarden/requirements.txt` is unpinned, and the watch names it on every run.
- `.github/workflows`: 32 files, no `pull_request_target`, two `workflow_run` limited to main (`rust-flake-rerun`, `deploy-cloud`), five `pull_request` with no secrets referenced.
- CI installs with `npm ci --ignore-scripts`. A developer machine and a `workflow_run` deploy holding the cloud SSH key are the two places a worm would want to land. The 2026 worms write hooks into `.claude/settings.json` and `.vscode/tasks.json`, which every checkout on this Mac has.
