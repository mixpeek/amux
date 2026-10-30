<!-- rules: parent=../../CLAUDE.md owner=amux-vision enforces=A4,A5,A9,A11 -->
# amux-server

The axum server: `src/api/` (one module per API family), `src/db/` (SQLite), `migrations/`
(SQL, append-only), `src/runtime_jobs/`, `src/backend/` (tmux and herdr). Integration tests in
`tests/`.

## Building and testing

- `CARGO_TARGET_DIR=~/.amux/rust-build-target`: one shared build dir, never per session.
- Local cargo goes through `scripts/safe-cargo.sh` (memory and disk budgets).
  `cargo check --workspace` for syntax; before push,
  `cargo clippy --workspace --all-targets -- -D warnings`.
- Tests: `scripts/test-contended.sh -p amux-server` (same arguments and exit status as
  `cargo test`). The builder may swap the shared binary while tests spawn it, so a red suite in
  a module you did not touch can be that; the wrapper says which kind of run it was.
- A new repo-walking test is listed in `scripts/ci-affected.mjs`.

## Diagnostics (A5)

Every diagnostic route answers with `measured` (did the probe run) and `n_considered` (how big
the population was), and `why_unmeasured` when it could not run. `total_errors: 0,
measured: true, n_considered: 4210` is a quiet window; `measured: false` is a probe that never
ran. Those two used to look the same, which was 41 of 83 frustration entries (AF-320). A new
diagnostic route without both fields fails `tests/diagnostic_contract.rs`. A window parameter
is `since_h`.

## tmux

Every `-t` target comes from the exact-match helpers in `backend/tmux.rs`
(`session_target`, `pane_target`). tmux resolves a bare `-t foo` by prefix, so `amux-amux`
matches `amux-amux-frustrations`; `tests/tmux_target_audit.rs` refuses a hand-spelled target.

The control-mode client (`backend/tmux_ctl.rs`) is off unless `AMUX_TMUX_CONTROL=1`: with it
attached, tmux 3.6a segfaulted in `control_notify_session_closed` on 2026-10-29 and took every
worker down (A11).

## Crash and stateful recovery (A11)

Every durable harness change includes recovery at its failure boundary. Use real
SIGKILL/restart tests, preserve failed evidence, adopt completed receipts, and assert no
duplicate delivery or false success. Supervise consumer futures and keep isolation, owner stops
and authorization gates intact. See `docs/harness-recovery.md` and
`scripts/test-harness-recovery.sh`; retaining rows alone is not proof that unfinished work
recovers.

## Isolated workers (A9)

Isolated means direct owner-to-CLI transport: no board capture or attribution, command
decomposition, injected prompts, automated schedule/steering, or amux routing, behavioral hooks
or MCP. Passive provider lifecycle observation is allowed for status only: it must not add
context, change permissions, send prompts, or trigger boards. Preserve owner input, delivery
receipts and message history. Explicit owner queuing is transport only. Isolation overrides
stale worker metadata; a restarted raw CLI must not inherit harness routing.

Three owner-configured exceptions reach isolated workers: their schedules, the goal keeper
continuing an owner-set Claude Code `/goal` that is not met (`AMUX_GOAL_CONTINUE=0` opts out),
and the worker boundaries in `boundaries.toml` (payments, email, credentials, resources, the
subagent cap), whose guard refuses the same tool calls for isolated workers
(`apply_to_isolated = false` opts out). The guard knows an isolated worker by
`AMUX_ISOLATED_WORKER`. A refusal injects no prompt and routes nothing.

A chat speaks for the owner: a worker's Chat tab or a `chat`-type worker can read, send to,
queue for and leave card notes that reach any worker, isolated included.

## Owner messages

An owner message always reaches the worker. Holding one for the board dispatcher needs
`AMUX_STAGE_OWNER_COMMANDS` on top of forced adherence (owner, 2026-10-29).

## Browser automation

`amux browser route start '{"profile":"<saved-profile>","url":"https://..."}'`, then
`amux browser route state`, `amux browser route shot` and
`amux browser route action '{"action":"click","selector":"..."}'`. Omit `profile` and the route
uses the profile `amux browser for <url>` picks. Backends are tried in the `ladder` of
`browser-methods.toml` (Amux, then a CDP copy of the Chrome profile, then CUA when enabled).
When another worker holds the profile, the Amux backend joins its browser in a tab of your own;
it falls to the CDP copy only if joining fails, or with `"join":false`. The result's `route`
names the backend, the profile, how it was chosen and every attempt. After a handoff, observe
again before acting; uncertain actions are never replayed. Scope refusals remain refusals.
Login-cookie evidence is not live auth proof. `amux browser route goal '{"task":"..."}'` hands a
whole task to an agent method from `browser-methods.toml`. Raw CDP is a fallback only when a
route cannot do the job (Ethan, 2026-10-27): `cdp_port` is withheld unless the caller passes
`cdp_reason`, and what it gets is a logging proxy (`api/cdp_proxy.rs`), so the grant and every
page-acting command land in `amux audit <worker>`.

## iCal

Events only (not schedules or board). `GET /api/calendar.ics` locally. The S3 key is random and
lives only in `server.env`; never commit the URL (the repo is public).
