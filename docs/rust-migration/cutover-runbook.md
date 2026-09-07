# Cutover Runbook — Python → Rust (Phase 11)

> **HISTORICAL (the cutover happened at 792ce1f, 2026-08-09).** Ports below are
> as they were during the cutover. Today the address is **8824**; 8822 survives
> only as a compatibility bind for pre-cutover processes and is being retired —
> see [server-boundary.md § The legacy port 8822](server-boundary.md#the-legacy-port-8822--retirement-and-the-number-that-decides-it)
> for the exit condition. Do not take a port from this file as current.

The strangler-fig sequence for moving `amux-server.py` (port 8822) to
`amux-server` (Rust). Every step is reversible until step 6; the DB file is
bilingual by construction (additive-only migrations, proven by the
rehearsal). Nothing here is a ceremony — each gate names the evidence it
requires and where to get it.

## Standing evidence (regenerate before cutover; both are one command)

| Evidence | Command | Last result |
|---|---|---|
| Migration rehearsal | `./docs/rust-migration/migration-rehearsal.sh` (moved from scripts/ 2026-08-11 — HISTORICAL, do not run against the live system) | PASSED 2026-08-09 vs a copy of the live 640k-row DB: 7 migrations, no table lost, row counts unchanged, integrity ok, Python reads+writes post-migration |
| Perf baseline | `cargo build --release -p amux-server && ./scripts/perf-baseline.sh` | PASSED 2026-08-09: dashboard 3ms, health 2ms, board 28ms @ 622KB, RSS 66MB (targets 500/50/200/200) |
| Test suite | `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings` | 470+ tests, 0 warnings |
| Browser suite | `npx playwright test --config e2e/playwright.config.ts` | 52+ scenarios, real dashboard, desktop+mobile |

## Phase A — shadow (current state)

Rust runs on **8824** against the SAME DB file (WAL readers coexist). The
Python server owns all writes that matter operationally; the Rust server's
write surface is exercised by its own tests and by anyone pointed at 8824.

- Scheduler: Rust is in SHADOW mode (fires nothing; journals
  `schedule_shadow` events). Compare against Python's `schedule_runs`:
  every Python `cron` fire should have a matching shadow event within the
  same minute. Divergence = a next-run computation bug; fix before Phase B.
- Orchestrator: `pickup_unowned=false` and unresolvable owners are skipped,
  so the Rust orchestrator cannot touch the Python fleet's cards.
- Launchd: add `com.amux.server-rs` alongside `com.amux.server` (do NOT
  touch the Python plist):
  ```bash
  # plist ProgramArguments: /path/to/target/release/amux-server
  # EnvironmentVariables: AMUX_RS_PORT=8824
  launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.amux.server-rs.plist
  ```

Exit criteria for A: 7 consecutive days of (1) shadow-scheduler agreement,
(2) zero Rust panics in its log, (3) `/api/metrics` dead_letters stable.

## Phase B — traffic swap (reversible in seconds)

1. Freeze: announce on the board; no deploys during the swap window.
2. Regenerate both standing evidence rows above. Red = stop.
3. Swap ports: Python plist gets `AMUX_PORT=8824` (its fallback home), Rust
   plist gets `AMUX_RS_PORT=8822`. `launchctl kickstart -k` both.
4. Smoke, in order (each can fail; stop on first red):
   ```bash
   curl -sk https://localhost:8822/health | grep '"server":"amux-rust"'
   curl -sk https://localhost:8822/ | grep -c app.js            # dashboard serves
   amux-rs --url https://localhost:8822 board list | head -3    # CLI reads
   amux-rs --url https://localhost:8822 board add "cutover smoke" # write path
   ./scripts/perf-baseline.sh   # with AMUX_RS_BIN + live port if desired
   ```
5. The fleet's CLAUDE.md `$AMUX_URL` needs no change (same port). The
   dashboard PWA picks up the new server on next load; the service worker
   CACHE key differs, so clients refresh their shell automatically.
6. Enable the Rust scheduler ONLY after confirming the Python one is no
   longer firing (both firing = double-send): stop Python or set its
   scheduler off, then `AMUX_RS_SCHEDULER=1` in `~/.amux/server.env` +
   restart. `schedule_runs.source` discriminates forever (`cron` = Python,
   `cron-rs` = Rust) — if both sources appear for one schedule in one
   minute, you have double-fire: kill AMUX_RS_SCHEDULER and investigate.

Monitor for 48h: `/api/metrics` (dead_letters, queue depth), `/health` rev
moving, board writes from real sessions landing (ask one session to run
`amux board add` and read it back).

## Rollback (any time in B; < 1 minute)

```bash
# Swap the ports back and kick both. The DB needs NOTHING: additive-only
# migrations mean the Python server reads the same file it always did
# (proven each rehearsal run, step 5).
launchctl kickstart -k gui/$(id -u)/com.amux.server      # Python back on 8822
launchctl kickstart -k gui/$(id -u)/com.amux.server-rs   # Rust back on 8824
```
Post-rollback: file the failure as a board card with the exact smoke line
that went red. The rehearsal + baseline must be re-run after the fix.

## Phase C — decommission (day 15+, only after 14 quiet days)

- Stop the Python launchd job; keep the plist + amux-server.py for 30 days.
- Move the Python scheduler entries' audit trail note: source=`cron` rows
  end here; `cron-rs` is authoritative after.
- The single-file Python server stays in the repo (it is the reference
  implementation for parity questions) but the deploy workflows stop
  shipping it.

## Known-incomplete at time of writing (blockers for Phase B, not A)

- Golden scenarios RR-0078/0080/0082/0085/0086 (live-model happy paths,
  scoped gates, multi-provider fleet, backend interchangeability) — in
  progress; Phase B requires at least the happy path green.
- Terminal scan loop (RR-0067) for hookless interactive workers — the
  structured protocol covers headless execution; INTERACTIVE tmux/herdr
  sessions need the capture loop before the Python watcher retires.
- SPA endpoints not yet implemented in Rust (extraction census):
  /api/journal, /api/terminal, /api/dictation, /api/torrents, /api/history,
  /api/gmail auth flow, /api/org — the dashboard tabs backed by these are
  degraded until ported. Phase B is still viable if those tabs are accepted
  as degraded during the swap window; list them in the announcement.
