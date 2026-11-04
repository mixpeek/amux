<!-- rules: parent=VISION.md owner=amux-vision enforces=all -->
# amux

**Read `VISION.md` first.** It holds what amux is for and the invariants A1-A11 every change
keeps. Name the ones your change touches in its commit body and board card (`Vision: A2, A5`).
This file says how to work here. Each folder's `CLAUDE.md` says what is true of that folder:
`crates/amux-server`, `crates/amux-dashboard`, `desktop`, `scripts`, `e2e`, `cloud`.

## The rule tree

- `VISION.md` is the root. Every `CLAUDE.md` starts with a header naming its parent (the
  nearest `CLAUDE.md` above it, or `VISION.md`), its owner and the invariants it enforces, and
  has an `AGENTS.md` symlink beside it so Codex and Gemini read the same rules.
- A rule file cites invariants by id and never restates them (A10). A new rule goes in the
  deepest folder it is true of. `scripts/check_rules_tree.py` refuses a missing header, a wrong
  parent, an unknown invariant or a missing `AGENTS.md`; `--map` prints the tree.
- The values every review applies are `.claude/rules/ethos.md`. `.claude/rules/` also holds
  `frustrations.md` (logging friction) and `harness-order.md` (form completion).

## Basics

- Rust workspace (`crates/amux-core`, `amux-server`, `amux-cli`, `amux-dashboard`) serving
  the dashboard on **port 8824**. Use `$AMUX_URL` or `$(amux url)`. 8822 is retired
  (`tests/legacy_port_guard.rs`). The Python server was deleted at `792ce1f`; do not resurrect it.
- `amux` is the Bash CLI. Publish a resolved, reviewed checkout with `make install-cli`
  (`BIN_DIR=/usr/local/bin` for that copy). Never symlink the installed client into a mutable
  worktree or copy it by hand.
- `~/.amux/server.env` holds persistent settings, loaded at startup as defaults. Credential
  values live only there (the repo is public); names are in `docs/credentials.md`. After an
  edit: `launchctl kickstart -k gui/$(id -u)/com.amux.server-rs`.
- `CLAUDE.local.md` (gitignored) holds per-machine notes, such as host inventory, that must not
  be public. `docs/ops/linux-and-remote-builds.md` covers Linux boxes, integration branches and
  remote builds.

## Working (A4, A6)

- **A bug fix brings two things:** the fix at the root cause, and a log signal (a counter, a
  WARN line, a verdict field) so the next instance announces itself.
- **A manual override is a defect.** If normal worker progress needs a restart, retry,
  board-status edit or file move, build bounded, logged discovery and recovery, then rerun the
  original case. Keep failed evidence; never bypass budget, customer-outbound or artifact-review
  gates. A failure the host can resolve never becomes a permanent wait or a `needs you`.
- **You run inside amux.** Fix rough edges at the root. Log friction to `frustrations.md`
  (format in `.claude/rules/frustrations.md`).

## Landing a change (A6)

- **Everything on main: no branches, no forks** (Ethan, 2026-10-24). Work in a detached worktree
  at `origin/main`, commit, then `git fetch origin main && git rebase origin/main && git push
  origin HEAD:main`, retrying a lost race. The pre-commit hook refuses a commit on a branch that
  forked from `origin/main`.
- **Gate before main: `amux land`** (or `scripts/land_gate.py`, then push). It replays your
  commits onto origin/main and runs the tests of every changed Rust module, the e2e specs
  `scripts/ci-affected.mjs` selects, and a review against `REVIEW.md`. A test that passes on
  rerun, or also fails on main, is named and does not block. The pre-push hook refuses an
  agent's commits with no passing receipt (`AMUX_SKIP_LAND_GATE=1` with
  `AMUX_SKIP_LAND_GATE_REASON` skips once, logged). Tiers are in `scripts/review-tiers.txt`:
  `[owner]` paths need the owner's approval of that exact diff, `[deep]` paths a `Revert:`
  trailer. Close a finding with `scripts/land_gate.py --resolve <id> "<why>"`.
- **Before pushing,** `scripts/push-consent.sh` names whose commits you would carry and who
  cannot be asked; ask the others. Run gates on a detached worktree: the shared checkout holds
  other lanes' uncommitted files. Why, with the incidents: `docs/ops/pushing.md`.
- **Only main deploys.** The builder activates the newest `origin/main` commit whose `checks`
  passed (`scripts/CLAUDE.md`). A local commit, branch, fork or hand-built binary deploys
  nothing; never copy a build over `~/.local/bin/amux-server-rs`. A fix is live when `/health`'s
  `commit` matches; Settings and `GET /api/pull` show what the server runs against main.
- **A red main is the first thing to fix.** SCHED-713 (`scripts/main-ci-watch.sh`) reruns a
  first attempt that failed only in e2e, then keeps one card on the amux lane per red stretch.
- **No auto-pull of the shared checkout.** `amux sync-checkout` (`--check` to only report)
  fast-forwards when nothing can be lost and otherwise names the blockers.
- **Shared checkout habits:** a pre-fix specimen is `<your-sha>^`, never `HEAD~1`; bracket
  measurements with `/health`'s `build` (the builder swaps the binary when main moves).

## Verification (A4)

`VERIFY.md` names the proof for each surface: the command, and what a pass looks like. The board
refuses `done` without evidence (AF-321). Paste the command and its result line:

```bash
amux board done <ID> --evidence-stdin <<'EOF'
scripts/test-contended.sh -p amux-server -> test result: ok. 1571 passed
EOF
```

`none: <reason>` is the honest answer when a card produced no artifact.

## Observability (A5)

Use the server's diagnostic endpoints before writing a grep, and read `measured` before the
number:

| Endpoint | Use |
|---|---|
| `GET /api/logs/analyze?since_h=24` | Error groups with verdicts |
| `GET /api/logs/stats?since_h=24` | Traffic and latency |
| `GET /api/debug/routes` | The route table |
| `GET /api/health/invariants` | Failing invariants (`/api/debug/invariants` shows all) |
| `GET /api/debug/sse?since_h=24` | Live SSE connections against client stale reconnects |
| `GET /api/debug/send-profile` | Where each send to a worker spends its time |
| `GET /api/debug/tmux` | Fleet discovery from inside the server |

Raw logs: `~/.amux/logs/server-rs.log`. Grep them with `-a`: one NUL byte makes grep treat the
file as binary and hide matches. Reading CI from here: `docs/ops/reading-ci.md` (use the App
token, and never poll `gh` in a loop).
