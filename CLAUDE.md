# amux

- Whenever you fix a bug: 1) fix it at the root cause, 2) make it surface in amux logs so a sweep would catch it.

Rust workspace (`crates/amux-core`, `amux-server`, `amux-cli`, `amux-dashboard`)
serving a static SPA on **port 8824**. Use `$AMUX_URL` or `$(amux url)`.

8822 is retired. `tests/legacy_port_guard.rs` fails the build if it reappears.
If your `$AMUX_URL` still says 8822, use `$(amux url)` (reads `~/.amux/endpoint.json`).

The Python server was deleted at `792ce1f`. Do not resurrect it.

## Ethos

Gut-check every feature against `.claude/rules/ethos.md` (8 rules). The core question:
when the next model is better, does this feature get better with it, or become the ceiling?

## Primitives (do not reinvent)

board, workers, schedulers, filesystem, groups, memories, environment, messages.
If a request decomposes into these, the work is configuration and UX. Do not add a
ninth thing that re-expresses them.

## Dogfooding + two-fix rule

You run inside amux. Fix rough edges at the root, not with workarounds. Every bug fix
owes two things: the fix, and a log signal so the next instance self-announces
(counter, WARN line, verdict field). Log friction to `frustrations.md` (format in
`.claude/rules/frustrations.md`).

## Structure

- `crates/amux-server` -- axum server: `src/api/`, `src/db/`, `migrations/`, `src/runtime_jobs/`
- `crates/amux-dashboard` -- SPA: `static/` (`index.html`, `app.js`, `app.css`, `sw.js`)
- `crates/amux-core` / `crates/amux-cli` -- shared types; Rust CLI
- `amux` -- bash CLI. This file IS the fleet's CLI: `~/.local/bin/amux` is a
  symlink pointing HERE, not the other way round. Live on save, not on commit —
  and so also live on `git checkout`, `stash`, or a branch switch, which swap it
  for all lanes with no save involved.
- `e2e/` -- Playwright; `crates/amux-server/tests/` -- integration tests
- `cloud/` -- cloud.amux.io (read `cloud/README.md` first)

## Hooks

Tool-event hooks need a `matcher` (regex). `"*"` is invalid; use `".*"`.
Verify a hook by what it WROTE, not by the settings file.

## Workflow

- **No auto-pull.** Shared checkout; the freshness hook reports staleness, the human decides.
- **Commit after every completed task.** Committing deploys locally (builder adopts within ~60s).
- **Bash CLI ships on SAVE** (symlink). `check-and-commit.sh` runs `bash -n` on every save.
  Server ships on COMMIT via the auto-builder.
- **`CARGO_TARGET_DIR=~/.amux/rust-build-target`** -- one shared build dir, never per-session.
- **Bracket measurements with `/health`'s `build`** -- the builder swaps the binary on any commit.
  Use `commit` for "did source change?", `build` for "same process image?".
- **Pre-fix specimen: `<your-sha>^`**, never `HEAD~1` (shared checkout).
- **Client JS: bump `APP_VER` (app.js) + `CACHE` (sw.js) together.**
- Syntax gates: `cargo check --workspace`. Before push: `cargo clippy --workspace --all-targets -- -D warnings`.
  Tests: `cargo test -p amux-server`.

## Observability

Use the server's diagnostic endpoints before writing a grep:

| Endpoint | Use |
|---|---|
| `GET /api/logs/analyze?since_h=24` | Error groups with verdicts |
| `GET /api/logs/stats?since_h=24` | Traffic/latency rollup |
| `GET /api/debug/routes` | Route table as JSON |
| `GET /api/health/invariants` | Failing invariants (passing ones only visible in `/api/debug/invariants`) |
| `GET /api/debug/sse?since_h=24` | Is the realtime backbone carrying the fleet, or has it dropped clients onto polling? `live_connections` + `opened_total` (per-PROCESS: the builder restarts this binary on every commit and all SSE connections die with it) joined with `stale_reconnects`, the client-side beacon fired at the 18s zombie trigger. Neither half answers alone — from the server a reconnect looks like a laptop lid; only the client knows it declared the stream stale. A 0 shortly after a deploy is a ramp-up, not a verdict; `live_connections` is the discriminator. |
| `GET /api/debug/tmux` | Fleet discovery from inside the server |

Raw logs: `~/.amux/logs/server-rs.log`

## Deploy

**Before `git push origin main`:**
```bash
git fetch origin
git rev-list --count origin/main..main
git log --format="%h [%(trailers:key=Amux-Session,valueonly,separator=)] %s" origin/main..main
```
If foreign commits exist, ask their author before pushing.

When user says "deploy": `git add` + `git commit` + verify above + `git push origin main`.

A fix in `git log` is not live until `/health`'s `commit` matches.

## Single-codebase rule

Server code is identical for local and cloud. No `if IS_CLOUD` branches.
Differences driven by env vars/headers from the gateway, not build flags.

## Server config

`~/.amux/server.env` -- persistent env vars, loaded at startup as setdefault.
Credential VALUES live here only (repo is public). Inventory: `docs/credentials.md`.
After editing: `launchctl kickstart -k gui/$(id -u)/com.amux.server-rs`.

## iCal sync

Events only (not schedules/board). `GET /api/calendar.ics` locally. S3 key is random
and lives only in `server.env`. Never commit the actual URL (repo is public).

## Browser Automation

Use `/chrome-cdp`: `node skills/chrome-cdp/scripts/cdp.mjs <list|snap|shot|click|type|eval|nav> <target>`.
