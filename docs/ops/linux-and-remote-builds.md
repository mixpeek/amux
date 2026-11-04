<!-- Moved verbatim from root CLAUDE.md on 2026-10-30 (VISION.md A10: one source per fact).
     History and notes for a Linux box, local integration branches and remote builds. -->
# Linux boxes, integration branches and remote builds

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

## Cargo in an interactive pane (Linux)

<!-- Moved verbatim from root CLAUDE.md's Workflow section on 2026-10-30. -->
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
