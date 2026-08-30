# cloud/ — cloud.amux.io provisioning

**Status (2026-08-09):** migrated to the **Rust server**. The workspace image is
built from `crates/` by `cloud/docker/Dockerfile`; there is no `amux-server.py`
anywhere in this path any more, and `.github/workflows/deploy-cloud.yml` cannot
build a python image (a guard job asserts it, and that guard has been exercised
against the deleted python Dockerfile — all four of its assertions trip there).

`cloud.amux.io` is a LIVE product. Nothing in this directory deploys itself:
the workflow builds and pushes the image and updates the host's gateway, and it
deliberately does **not** recreate running workspaces. See
[Cutting a live workspace over](#cutting-a-live-workspace-over).

---

## The pieces

| Path | What it is |
|---|---|
| `main.tf`, `variables.tf`, `outputs.tf`, `setup.sh`, `deploy.sh` | GCP single-VM box (`amux-dev`). **NOT migrated** — see below. |
| `setup-cloud.sh` | Bootstrap for the multi-tenant host: docker, nginx, certbot, the gateway systemd unit, `/etc/amux/gateway.env`. |
| `gateway/gateway.py` | The auth + orchestration gateway. Host process, stays python. |
| `gateway/nginx.conf` | TLS termination for `cloud.amux.io` → `127.0.0.1:8080` (the gateway). |
| `docker/Dockerfile` | The workspace image: multi-stage rust build + a debian runtime. |
| `docker/docker-compose.template.yml` | Per-tenant stack, instantiated by the gateway. |
| `litestream/litestream.yml` | Per-tenant SQLite WAL replication to R2. |
| `seed.py`, `ui_seed.py`, `tests/` | Provisioning and end-to-end checks for the hosted tier. |
| `plans/*.json` | Declarative workspace definitions consumed by `seed.py`. |

## Request path

```
browser → nginx :443 (letsencrypt)
        → gateway.py :8080          Clerk JWT → amux_session cookie → org → port
        → https://127.0.0.1:<port>  the tenant's container, loopback-only
        → amux-server-rs :8822      inside the container
```

The gateway is the trust boundary. It strips `authorization` and `cookie` before
proxying, adds **`X-Amux-User-Email`** (the server reads it in `api/mod.rs`,
`api/schedules.rs`, `api/session_verbs.rs`), and the container therefore runs
with `AMUX_AUTH_TOKEN=none` bound to `127.0.0.1` only. A `0.0.0.0` publish here
was once a cross-tenant unauthenticated RCE.

### The one thing the rust cutover changed about that path

The rust server **always serves TLS** — there is no no-tls switch, and a plain
HTTP request to it gets a `301` to `https://localhost:8822`, which from the host
points at the host and not at any container. So the loopback hop is `https` with
a self-signed per-container cert, and the gateway reads **`CONTAINER_SCHEME`**
(`http` by default, set to `https` in `/etc/amux/gateway.env` by the deploy).

A gateway left on `http` serves the Starting page forever, which reads as a
container fault. The one line that discriminates it is in
`/var/log/amux-gateway.log` at startup:

```
[gateway] container hop: https:// (cert verification off) — set CONTAINER_SCHEME in /etc/amux/gateway.env
```

The deploy asserts on that line rather than on `systemctl is-active`, because a
process existing does not prove it read the config that was just written.

## The image

`docker build -f cloud/docker/Dockerfile -t ghcr.io/mixpeek/amux:latest .`
(context is the **repo root**; `.dockerignore` keeps it small).

Build stage needs `build-essential cmake perl` — `libsqlite3-sys` (bundled),
`ring`, and `aws-lc-sys` (in the graph via rustls 0.23 even though the server
pins `ring` at runtime) all compile C. `--release` is required, not preferred:
`rust-embed` only embeds the dashboard in release, so a debug binary boots,
answers `/health`, and serves "dashboard not embedded" with nothing in the log
naming the cause.

Runtime carries what the server actually spawns — `tmux`, `git`, `python3`
(tmux `pipe-pane` log writer: without it every session log is silently empty),
`procps`, `ripgrep`, `ffmpeg`, `chromium`, `curl` — plus node 22, the Claude
Code CLI, and the toolchain the customer's agent needs (`python3-pip`,
`openssh-client`, `jq`, `git`). `claude --version` and `chromium --version` run
at build time so a broken CLI fails the build instead of the customer's session.

### The two things the image must say that a laptop does not

Both were found by running the image, not by reading the code, and both are
environment facts the server cannot infer:

* **`IS_SANDBOX=1`.** The server launches `claude --dangerously-skip-permissions`,
  and Claude Code v2.1.69+ refuses that flag as root. Without this env the pane
  exits 1 at launch in **every** workspace — and the worker still reports
  `status: idle, running: true`, so the fleet looks healthy while no agent is
  running (AMUX-2644). The python server dodged it by stripping the flag when
  `getuid()==0`; the rust server has no such branch and should not grow one,
  because "am I root" is not the question — "am I an isolated single-tenant
  container" is, and only the deployment knows.
* **A chromium shim on PATH** carrying `--headless=new --no-sandbox
  --disable-dev-shm-usage`. Without it Chrome spawns and its CDP port never
  answers; the error names the symptom (`CDP … never answered within 12s`) and
  not the cause. With it the browser primitive is verified end to end in the
  container: start → navigate `example.com` → screenshot 200 → `/api/browser/state`
  returns the real page text and elements → stop.

Neither is a code branch. If a third case appears, it belongs in one of these
two shapes (env, or a binary on PATH) and not in `crates/`.

### What degrades in the container, and how

Verified by exercising each one in the built image:

| Capability | State | How it fails |
|---|---|---|
| macOS keychain (Claude OAuth) | absent | `security` spawn fails → `keychain_token()` returns `None` → falls through to `~/.claude/.credentials.json`. No platform branch, no error. |
| herdr session backend | absent | tmux is the default; herdr is only constructed when `AMUX_HERDR_SESSION` is set, and reports `herdr not found` if it is. |
| local whisper dictation | absent | `whisper_available()` is false (no weights, no interpreter with `whisper`+`torch`) → dictation falls back to the Gemini path, which needs `GEMINI_API_KEY`/`GOOGLE_API_KEY`. |
| iMessage alerts (`osascript`) | absent | returns `imessage error: <io error>`. Set `TWILIO_*` in the workspace's `server.env` and the SMS path is taken instead — `osascript` is never reached. |
| `/api/browser/agent` | 501 | deliberate: the browser primitive drives Chrome over CDP natively; there is no browser-use shell-out to install. The rest of `/api/browser/*` works (verified above). |
| media **transcode** (`/api/file/prepare`) | broken on linux | `file_viewer.rs` hardcodes `h264_videotoolbox`, a macOS-only encoder. The job fails and is recorded as failed. The *streaming* path correctly uses `libx264`, so playback works. |
| `xdg-open` reveal | works | `api/fs.rs` already branches per platform. |

### What no longer supervises the container

The `watchdog` sidecar is gone from the compose template. `scripts/watchdog.py`
now supervises the LOCAL macOS service (`launchctl kickstart
gui/<uid>/com.amux.server-rs`); inside a container it can only log a failure
forever, so keeping it would have been theatre.

What that leaves: `restart: unless-stopped` covers a container that **exits**,
and the image `HEALTHCHECK` marks a wedged one **unhealthy** — but nothing
restarts an unhealthy container, and the gateway's response to unhealthy is to
serve the Starting page. **Alive-but-wedged is currently unsupervised.** The fix
belongs in the gateway, which already knows every tenant's health and already
has docker access: restart a container that has been `unhealthy` for N minutes,
once, with a cooldown. Filed, not built — it changes live-tenant behaviour and
wants a real test.

### Capability that did NOT carry over

`AMUX_AUTOSTART_SESSIONS=1` was set in the python image so a replaced container
brought its owner's sessions back. **The rust server does not implement that
variable** (zero hits in `crates/`). It is deliberately not set anywhere — an
env var the server ignores is a lie the deploy tooling then acts on. The
consequence is operational, not cosmetic: recreating a container stops every
tmux session in that workspace and nothing brings them back, which is why the
deploy does not recreate anything on its own.

### Known gateway ↔ server contract gaps

Both are server-side and both were checked against the running rust build:

* **`/api/observability` → 404.** `_refresh_org_spend()` reads it to roll
  transcript costs into an org's spend. With a 404 it returns `None`, so budget
  enforcement rests entirely on the proxy meter — exact for traffic through the
  house key, blind to BYO-key usage. `/api/usage` is *not* the replacement; it
  is the Claude subscription probe and carries no `total_cost`.
* **`/api/share/<token>/info` → 404**, so `_resolve_share_token()` fails for
  every token and public share links are dead. Worse, `/s/<token>` returns
  **200** from the SPA catch-all, so a dead share link renders the dashboard
  shell and looks like it worked.

## Deploying

`.github/workflows/deploy-cloud.yml`. Two triggers (AMUX-2968, Ethan
2026-08-12):

- **Automatic** — after the `rust` CI workflow finishes **successfully on
  `main`** (`workflow_run`). This runs the safe jobs only (guard → build →
  deploy): the CI-passed binary becomes `ghcr :latest` and the host pulls it.
  It builds from the **exact commit CI passed** (`head_sha`, pinned in every
  checkout and the image tag), never a PR, never a red run.
- **Manual** — `gh workflow run deploy-cloud.yml`, and the ONLY way to reach
  the recreate job below.

A commit that does not trigger `rust` (docs, a pure `cloud/gateway.py` edit,
the bash CLI — none match rust.yml's `paths`) does not auto-deploy: there is no
CI pass to gate on and no new binary to ship. Deploy those manually.

1. **guard** — the image is a rust build and nothing resurrects the python
   server. This job carries the auto-deploy `if` (dispatch, or a *successful*
   `rust` run); the whole cascade hangs off it via `needs`, so a red/PR CI run
   leaves everything skipped.
2. **build** — `ghcr.io/mixpeek/amux:{latest,<head_sha>}`, `linux/amd64`, gha cache.
3. **deploy** — gateway secrets, `ADMIN_EMAILS`, `CONTAINER_SCHEME=https`,
   `gateway.py` + compose template + litestream config, `docker pull`,
   regenerate each tenant's compose, `docker compose up -d --no-recreate`, then
   **name every workspace still on the old image**.
4. **redeploy** — only on `workflow_dispatch` with `recreate=yes`. Because it is
   gated on `github.event_name == 'workflow_dispatch'`, an auto-deploy (which
   arrives as `workflow_run`) can **never** reach it — taking a customer's
   sessions down stays a human's explicit choice. Recreates running workspaces
   and then proves each one answers `/health` with `"server":"amux-rust"` on the
   new image digest.

### Cutting a live workspace over

A recreate stops every session in the workspace and nothing restores them (see
above). That is a customer-visible outage of their in-flight work, so it is a
human's decision, not a push's:

```bash
gh workflow run deploy-cloud.yml -f recreate=yes
```

Required repo secrets: `CLOUD_SSH_KEY`, `STRIPE_SECRET_KEY`, `ANTHROPIC_API_KEY`,
`STRIPE_PLATFORM_FEE_PRICE_ID`, `STRIPE_PLATFORM_PRICE_ID`. `GITHUB_TOKEN` is
automatic and is what pushes to ghcr.io.

## The python that is still here, and why

None of these ever imported the python server; all are stdlib HTTP clients.
They are kept because they are the only end-to-end coverage of the hosted tier,
and every endpoint they call is routed on the rust server (checked against
`/api/debug/routes`).

* **`seed.py`** — provisions a workspace from a plan JSON (`plans/*.json`):
  context docs, sessions, schedules, board items, optional run + verify. It
  drives the gateway admin API and the container API. Not server code; a
  server subcommand for it would be a worse home.
* **`ui_seed.py`** — the same seeding through the REAL UI via `/api/browser/*`,
  because the API path sails past gate 409s and read-after-write holes that a
  human clicking hits immediately. Needs the container's chromium, which the
  image now ships.
* **`tests/e2e_smoke.py`** — Clerk user → gateway auth → sessions per provider →
  BYO key → logout. ⚠ it exercises Codex and Gemini; the image only installs the
  **claude** CLI, so those two arms fail on a missing binary until `@openai/codex`
  and `@google/gemini-cli` are added to the Dockerfile.
* **`tests/e2e_trial.py`** — trial provisioning, invite acceptance, budget 402 +
  session stop, pro upgrade. Purely gateway-level.
* **`tests/godmode_walkthrough.py`** — signs in as an admin and collects evidence
  per customer org. It prints and does not conclude, on purpose: the `verified`
  gate is a human reading the evidence.
* **`tests/e2e_personas.py`** — the assertive twin of `godmode_walkthrough`: signs
  in as a user and turns every observation into PASS/FAIL, per environment and per
  persona (a persona = a preconfigured worker role). Data-driven from `plans/*.json`.
  The first assertion (environment reachable) is the outage detector this suite was
  written for after the 2026-08-16 disk-full incident — a scheduled run goes red the
  moment cloud.amux.io 502s. Read-only on real-customer orgs; the active reply probe
  (`--send-probe`) is opt-in and refused for anything that is not a demo. Exits
  non-zero on any failure so a scheduler can gate on it; `--json` for a summary.
* **`cloud_autofix.py`** — daily cloud health check + deterministic AUTOFIX (runs on
  SCHED-356). Probes cloud.amux.io, runs `e2e_personas` when up, and when down applies
  the exact repairs the 2026-08-16 outage needed BY HAND: a truncated `gateway.env` ->
  restore from the newest good backup (merged with current) + restart; disk full of
  LOGS -> truncate container json-logs + journald + restart; gateway crash-loop ->
  restart. It ESCALATES what it must not touch (disk full of DATA = customer volumes,
  ethos rule 8; or a still-502 after repairs) via board + `amux alert`, and traces
  every action to `/var/log/cloud-autofix.jsonl` and stdout. `--no-fix` for a dry check.
  It SSHes to the host via `~/.ssh/amux_cloud`; override the host with `AMUX_CLOUD_HOST`.
* **`CDN.md`** — design + usage for the public `cdn.amux.io` CDN subdomain (Cloudflare R2 + edge cache, the CloudFront-equivalent on amux's stack). Provisioning is blocked on one Cloudflare R2-admin token; the doc has the architecture, the upload commands, and the exact access grant needed.

`gateway.py` stays python for the same reason: it is a host process that never
runs customer code, and rewriting it was not part of the server cutover.

## Not migrated: the single-VM GCP box

`main.tf` + `setup.sh` + `deploy.sh` provision `amux-dev`, a one-machine
Tailscale-only box whose systemd unit ran `python3 /opt/amux/amux-server.py`.
That is a different topology from cloud.amux.io and it has **not** been ported.
`deploy.sh` now refuses at its prerequisite check with the real reason rather
than provisioning a VM whose service can never start; `--destroy` still works.

Porting it is small: ship `target/release/amux-server` instead of the `.py` and
change one `ExecStart`. Nobody has needed the box, so it has not been done.
