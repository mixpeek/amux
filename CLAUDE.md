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
  Tests: `scripts/test-contended.sh -p amux-server` (same args as `cargo test`, same exit status).
- **Prefer offloading any of the above to remote hardware (see the remote-build convention below)
  over running them in an interactive pane at all.** If a local run is genuinely unavoidable, run
  it through `scripts/safe-cargo.sh <cargo args>` instead of bare `cargo` — every process in an
  interactive amux pane shares one systemd scope, and an OOM-killed `cargo`/`clippy` process
  inside that scope takes the WHOLE PANE down with it, not just the build (confirmed via
  journalctl, AMUX-70/frustrations.md 2026-09-01). The wrapper runs cargo in its own sibling
  scope so an OOM kill there can't cascade into the session.
  **A red suite here is not automatically a regression.** This box builds and tests amux
  continuously, so the auto-builder can rewrite the shared binary while your tests spawn it,
  and the ETXTBSY family surfaces as failures in modules you never touched (AMUX-3853: 8
  failures in `opencode::structured`, 15/15 green on an immediate rerun). Plain `cargo test`
  cannot tell you which kind of run you got, so every green also silently means "and nothing
  was building" and every red looks like your fault. The wrapper prints that missing clause
  beside the result, in both directions. Use plain `cargo test` when you want the raw thing.

## Verification

`VERIFY.md` names the proof for each surface: the literal command, and what a
pass looks like. The board refuses `done` without evidence (AF-321) and its
refusal points here.

Paste the command AND its result line into `--evidence`. A command with no
result is a claim that you ran it.

```bash
amux board done <ID> --evidence-stdin <<'EOF'
scripts/test-contended.sh -p amux-server -> test result: ok. 1571 passed
EOF
```

`none: <reason>` is the honest answer when a card genuinely produced no artifact.
It is stored and counted, not a bypass.

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

**Read `measured` before you read the number.** Every diagnostic endpoint
answers with `measured` (did the probe run) and `n_considered` (how big the
population was). `total_errors: 0, measured: true, n_considered: 4210` is a
quiet window; `total_errors: 0, measured: false` is a probe that never ran, and
`why_unmeasured` says what stopped it. Those two used to be the same payload,
which is 41 of 83 frustration entries (AF-320). A new diagnostic route without
both fields fails `tests/diagnostic_contract.rs`.

Raw logs: `~/.amux/logs/server-rs.log`

**Grep them with `-a`.** A single NUL byte anywhere in the file makes grep call
the whole thing binary and SUPPRESS match output, while `-c` keeps counting
lines. Same file, same pattern, measured 2026-09-04: `grep -c` 17, `grep -o` 8,
`grep -ao` 17. Nineteen NUL bytes in 67 MB did that, and grep says nothing when
its output goes to a pipe. AF-481 removed the source (a sentinel that reached a
warn), but any logged payload can reintroduce one, so pass `-a` rather than
trusting the file.

Also: `grep -c $'\0'` does not count NUL bytes. bash cannot put a NUL in a
string, so that argument is the EMPTY string and the command is `grep -c ''`,
which counts every line. It reads like a NUL count and is a line count. Read the
bytes if you need the real figure.

## Reading CI: use the App token, and CHECK that you got it

`gh` here defaults to the user identity (`esteininger`, id 15973166) on a **5000/hr**
budget shared by every amux lane. A GitHub App is already provisioned on this box
and sits on its own larger budget:

```bash
eval "$(~/.amux/github-app/get-token.sh)"
gh api rate_limit --jq '.resources.core | "limit=\(.limit) remaining=\(.remaining)"'
```

**The limit number is the check, and it costs nothing** because `rate_limit` is not
itself counted. `limit=5000` means you are on the shared user budget; anything
higher means you got the App. Measured 2026-09-01: bare `gh` reported
`limit=5000 remaining=4999`, the App `limit=8700 remaining=8625`.

Run the check, do not assume the eval worked. `get-token.sh`'s own docstring records
why: if the script exits non-zero the `eval` sets nothing, `gh` falls back to user
auth, and you are back on the contended budget with no sign that anything happened.

**Do not poll `gh` in a loop.** Secondary limits are per-account and trigger on
request RATE, so one lane's 30s `until` loop 403s every other lane, on every
endpoint, including a plain repo read. On 2026-09-01 two lanes lost CI visibility
for hours that way, and `gh api rate_limit` reported `used: 0, remaining: 5000`
throughout, because the counter it reads is not the counter being enforced. Use
ScheduleWakeup, or one delay sized to the job. (AF-396)

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

## Local integration testing while PRs await upstream merge

A session's `gh` account can be a **fork-and-PR contributor** on the upstream
repo with zero write access there (`push`/`admin`/`maintain` all `false` —
check with `gh api /repos/<owner>/<repo> | jq .permissions`, not by trying an
action and reading the error), even though it has full admin on its own fork.
That session cannot merge PRs, re-run failed CI jobs, or trigger the upstream
auto-builder — those need someone who IS a collaborator on the upstream repo.
Confirmed 2026-08-28: `gh run rerun` and the Actions "re-run" API both 403
identically ("Must have admin rights to Repository"), so there is no CLI path
around a missing merge permission — only a NEW PUSH triggers fresh CI, since
pushing a branch only needs write access to the FORK.

**While several PRs sit open and green awaiting that merge, build a LOCAL
integration branch instead of waiting idle:**

```bash
git fetch origin main --quiet
git checkout -b local/all-features-testing origin/main --quiet
git merge <branch-1> --no-edit   # smallest/lowest-risk PRs first
git merge <branch-2> --no-edit   # ... in ascending size/conflict-risk order
# largest, most conflict-prone PRs last — you now know exactly what collides
```

This branch is **LOCAL ONLY — never pushed, never opened as a PR.** It exists
so the checkout's live server (whichever branch this shared checkout has
HEAD on becomes the fleet's live build — see Workflow above) can serve
everyone's in-flight work at once for real testing, without that testing
being mistaken for review or merged history anywhere.

**Migration numbering WILL collide.** Two PRs rebased independently onto the
same `main` baseline often each claim the same next-free migration `version:
N` for their own new migration — neither is wrong on its own branch, but
combined they're a duplicate. Symptom: a merge conflict in `db/migrate.rs`
where both sides declare the same `version:`. Fix: renumber the LOSING side's
migration to the next actually-free slot (`git mv` the `.sql` file to match,
update its `name:` field too) — pick either side, but do it consistently and
say so in the merge commit, since the same collision recurs every time this
branch is refreshed until the PRs merge upstream in some order for real.

**Refreshing as PRs get new commits or upstream moves:** don't try to
fast-forward a diverged integration branch — delete and rebuild it the same
way, `git branch -D local/all-features-testing` then redo the merge
sequence. It is disposable by design; the real history lives on each PR's
own branch.

**Getting the local build to auto-adopt:** the builder (`scripts/
rust-auto-build.sh`, `Committed after every completed task... within ~60s`
above) is only ever scheduled by a **launchd** agent
(`com.amux.server-rs-builder`) — macOS-only. On Linux there is no
`amux-builder.timer` equivalent unless it's installed — confirmed live
2026-08-28: the builder's own log showed its last invocation was 2 days
stale, `Terminated` mid-build, with nothing scheduled to ever retry it,
because this box had never had that service installed. Without it,
`/health`'s `commit` silently stops moving and nobody notices until they ask
"is my fix actually live" and it isn't.

This does NOT mean Linux has no systemd supervision at all — it was a wrong
generalization from one missing unit. `amux.service` (the server itself,
`Restart=always`) and `amux-worker-start.service` (starts the `amux` lane's
tmux+Claude process on boot) were BOTH already installed and enabled on this
box, confirmed by a real, deliberate reboot test on 2026-08-28: both came
back automatically with zero manual intervention, `amux.service` is in fact
what was silently auto-respawning the server during an earlier redeploy in
that same session (mistaken for a mystery supervisor at the time). Only the
BUILDER unit was the actual gap. `scripts/amux-builder.service.template` +
`.timer.template` are now installed on this box too (generated to
`~/.config/systemd/user/`, `daemon-reload`'d) and — as of 2026-08-31 —
`amux-builder.timer` is **enabled and live**, polling every 60s.

**Correction 2026-08-31, superseding the original "leave it disabled" note
below**: this paragraph used to warn that enabling the timer meant "a
restart kills every live Claude session" and left the decision to whoever
was driving. That warning is now stale. Root cause (INIT-1, closed
2026-08-30, see `frustrations.md`): `amux.service` used to run with
systemd's default `KillMode=mixed`, and the tmux server hosting every
worker session lives in that unit's own cgroup (spawned by
`ExecStartPre`, and cgroup membership is sticky across tmux's own
self-daemonization) — so ANY restart of `amux.service`, reboot or an
ordinary auto-builder deploy, SIGKILLed the whole cgroup, tmux and every
live Claude session included. The fix, already shipped and verified
**loaded** (`systemctl --user show amux.service -p KillMode` → `process`,
confirmed live 2026-08-31, not just present in the unit file): `amux.service`
now sets `KillMode=process`, so systemd only signals its own tracked main
PID on stop/restart and leaves every other process in the cgroup — tmux
server, every pane, every Claude process — untouched. A commit-triggered
auto-restart no longer kills any session, so the timer is safe to run
enabled, which is now this box's actual state.

This does NOT cover a real reboot, which is a different failure path with
its own still-open gap — see the reboot findings a few paragraphs below.

**Playwright MCP servers had NO supervision at all** until 2026-08-28 (found
during the same reboot test — the 5 lanes came back for everything
`amux.service`-adjacent, and stayed down for everything else). Fixed with a
template unit, `scripts/amux-playwright-mcp@.service.template` +
`scripts/amux-playwright-mcp.sh` (the wrapper resolves systemd's `%i` —
`"<lane>-<port>"` — into a port and a per-lane profile dir, then `exec`s
`npx @playwright/mcp` so systemd tracks the real process, not a wrapper
shell). Installed and enabled live on this box:
```bash
for i in frontstage-8931 synthesia-8932 backstage-8933 amux-8934 infra-8935; do
  systemctl --user enable --now amux-playwright-mcp@$i.service
done
```
Crash-recovery verified live: `kill -9` on the running process → systemd
schedules a restart within `RestartSec=5` → port answers again ~12s later.
Note `--host 0.0.0.0` is baked into the wrapper — binding to `localhost`
resolves IPv6-loopback-only on this box and silently breaks every IPv4 MCP
client (a separate bug found and fixed the same day, see frontstage's
diagnosis in git log around commit `8d1aea47`).

**Found while wiring this up: both `.service.template` files had a real,
never-caught path bug.** `ExecStart=$SCRIPT_DIR/rust-auto-build.sh` (and the
playwright template had the same shape) — but `$SCRIPT_DIR` in `install.sh`
resolves to the REPO ROOT, and the scripts actually live at
`scripts/rust-auto-build.sh` / `scripts/amux-playwright-mcp.sh`. Both
templates would have failed to start with "No such file or directory" had
anyone actually installed them via `install.sh` — which is exactly why it
was never caught: `amux-builder.service` had never been installed on any
Linux box until today. Fixed both templates to reference `$SCRIPT_DIR/
scripts/...`.

An LXC container `reboot` (not just a server restart) was also deliberately
tested end to end on 2026-08-28. Full findings: `amux-server-rs` and the
`amux` lane's tmux+Claude session both came back with zero manual steps
(systemd, as above); `frontstage` and any other non-`amux` lane did not
(no unit restarts them — worth a `feature/systemd-linux`-style template per
lane if this fleet keeps growing); the Proxmox LXC `onboot` flag is unset
for this container (vmid 250, node `virt`) — irrelevant for a clean
in-container `reboot` (LXC handles that as a real restart) but means a crash
or host-level reboot would NOT bring the container back on its own — a real,
separate gap, not yet fixed.

**A clean multi-way merge with ZERO conflict markers is not proof the
result is correct** — confirmed live 2026-08-28, building the integration
branch: a `git merge` of `feature/central-secrets-sops` reported
`Auto-merging crates/amux-server/src/lib.rs` with no conflict, and silently
dropped a `pub mod secrets;` declaration, an `AppState` struct field, and a
~15-line secret-store init block — three separate deletions, zero warning.
Every LOCAL build on this box kept passing for hours afterward, because
each one kept reusing/rebuilding from the SAME already-corrupted commit —
a build that only re-verifies its own prior state can't catch a defect
baked into that state. What actually caught it: building the exact same
commit from a truly independent starting point (a fresh `git clone` onto
different hardware, see below) — that failed to compile immediately with
"cannot find `secrets` in `crate`", which no amount of re-running the same
local build ever would have surfaced. If a merge into this branch touches
a file that ALSO changed in an earlier merged branch, diff the result
against the ORIGINAL branch that owns the code (`diff <(git show
<branch>:<file>) <file>`) rather than trusting "no conflict" as sufficient
— especially for any file every branch's own commits kept touching (like
`lib.rs`'s `async_main`, where every phase adds its own spawn).

**Offloading a build to faster/idle hardware.** This box is genuinely
resource-constrained (4 threads, i3-7020U) — worth doing for anything past
a quick `cargo check`, if you have spare hardware reachable over SSH.
Specific hostnames/users/specs are NOT documented here — this is a public
repo, and machine names + SSH access patterns are reconnaissance info about
personal infrastructure even though they aren't credentials. See
`CLAUDE.local.md` (gitignored, per-checkout — see the "Local/internal AI
notes" pointer below) if this checkout has one populated.

The technique, which IS safe to share: same architecture, different OS/libc
still needs a matching environment — a binary linked against a newer glibc
will not start on an older one. The reliable fix: build inside a Docker
container on the remote host that matches THIS container's own base image
(`debian:trixie` here), not the remote host's native OS — that guarantees
glibc compatibility regardless of what the remote host itself runs. Use a
persistent, named container (not `--rm`) plus `docker exec -d` for the
actual build command, so setup survives a dropped SSH session; `tar` the
source over SSH if the remote host lacks `rsync`. Cross-arch (e.g. Apple
Silicon → x86_64 Linux) adds QEMU emulation (3-10x slowdown) or a
Rosetta-backed VM (Colima `--vm-type=vz --vz-rosetta`, ~20-30% overhead) —
only worth it if same-arch hardware isn't available.

**Measured speedup, same commit, both `--release`:** this container (i3,
4 threads, throttled to `jobs=1`/`incremental=false` for its own memory
safety — see the cargo config note above) took **26m48s**. A remote 8-thread
host, full default parallelism, in a matching Docker container, took
**6m21s** — about 4.2x faster. Worth the ~1 minute of setup for anything
beyond a quick local `cargo check`.

**A remote build must ship the REAL source, not `git clone` the remote's
own idea of it.** Confirmed live 2026-08-28: a remote-build helper script
did `git clone .../mixpeek/amux.git && git checkout <local-only-sha> ||
git checkout main` — the local-only merge commit (never pushed) obviously
didn't exist on GitHub, the `checkout` failed, and the `||` silently fell
back to `main`. The build succeeded, deployed, and `/health` went green —
on the WRONG commit, missing every locally-merged feature branch, with
nothing in the build log or health check distinguishing it from a correct
build. `cargo build` cannot fail on "compiled the wrong source" — that
class of error is invisible to every gate that only checks whether the
build succeeded. Ship the actual worktree (`tar --exclude .git --exclude
target -cf - . | ssh host 'tar -xf - -C dest'`, or `rsync` if available),
never a fresh clone, when the commit being tested is local-only. If a
clone is unavoidable, verify after the fact: `ssh host 'cd dest &&
git rev-parse HEAD'` must equal the local `git rev-parse HEAD` — a
`checkout <sha> || checkout main` fallback must be treated as a failure
to report, never a silent success path.

**Swapping the live binary needs `mv`, not `cp`, and a real port-release
wait — get either wrong and the "fix" produces a stuck process burning
100% CPU with the server DOWN.** Confirmed live 2026-08-28, deploying the
build above:
1. `sudo -n bash -c 'cp ... ~/.local/bin/...'` runs as **root**, whose
   `$HOME` is `/root` — `~` inside that script did NOT expand to the
   syseng homedir it looked like it would from the outer shell. The copy
   silently failed (`cannot stat`), no binary changed, and nothing said so
   loudly enough to notice before declaring the deploy done. Don't sudo a
   file the calling user already owns — there is no privilege boundary to
   cross, only a `$HOME` to accidentally swap.
2. `cp new_binary ~/.local/bin/amux-server-rs` while that exact path is
   the running process's executable fails with `Text file busy` — `cp`
   truncates-and-writes the target in place, which the kernel refuses
   against a mapped executable. The fix is `cp new_binary path.new &&
   mv path.new path` — `mv` on the same filesystem is a rename, which
   swaps the directory entry atomically without touching the inode the
   old process still has open, so the old process keeps running its old
   image right up until you kill it, with no window where the path points
   at a half-written file.
3. `kill` + immediate `nohup new_binary &` raced the OS's own socket
   teardown: the new process bound `AddrInUse` because the killed
   process's SIGTERM handler hadn't released port 8824 yet, and a THIRD
   process (spawned same way, retried before checking) came up holding
   the OLD binary (because step 1's swap had failed) and spun at 99.6%
   CPU indefinitely with no further log lines — no crash, no exit, just a
   silent busy-loop with the server not listening. Kill, then poll
   `ss -tlnp | grep <port>` until it's actually empty before starting the
   replacement — don't assume `kill` returning means the socket is free.
4. **Verify the running binary is the one you just built**, don't infer
   it from the deploy script exiting 0: `md5sum /proc/<pid>/exe` compared
   against the source file you copied. `/health`'s `commit` field is the
   cheaper first check but only catches wrong-COMMIT bugs (see the item
   above); it says nothing about wrong-BINARY-on-disk bugs where the
   commit field itself came from a build that never actually landed.

**Local/internal AI notes.** This checkout may have a `CLAUDE.local.md`
(gitignored, never committed or pushed) alongside this file — read it if
present. It's where per-machine specifics that shouldn't be in a public
repo live: remote host inventory, SSH access patterns, anything that's
reconnaissance info about personal infrastructure rather than a credential
(credentials never belong in either file — see `docs/credentials.md`'s
names-only-here / values-in-`server.env` split, same idea). If you learn
something host-specific worth remembering, put it there, not here.

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
