# A vision and rule tree for amux

Owner ask, 2026-10-30: "take a look at the vision stuff i did with @mixpeek-vision
(messages, work, commits, peek, etc.) and think about how we can do the same for amux".

This is the plan. Nothing in it is built yet.

## What mixpeek-vision did, in one night

The owner gave one worker one job, "the manager of the Mixpeek overarching vision", and
the reason: the TubeScience work had built capabilities outside Mixpeek's primitives,
which defeated the product. Over 17 messages (10-29 19:47 to 10-30 09:20) and 13 commits,
the lane built five things:

| Mechanism | Commit | What it does |
|---|---|---|
| One vision file with numbered invariants | 87a6788d619 | VISION.md cut from 874 lines to about 150: north star, invariants V1-V12, binding on every change. The long form moved to `research/vision/` as non-binding rationale. |
| A rule tree | 87a6788d619 | Each folder's CLAUDE.md starts with a header (parent, owner, enforces). Root CLAUDE.md says how the tree works. An AGENTS.md symlink beside each file, so Codex reads the same rules. |
| A check that keeps the tree true | 87a6788d619 | `check_rules_tree.py` in pre-push refuses a missing header, a wrong parent or an unknown invariant. |
| Work names the invariants it touches | 87a6788d619 | Commit bodies and board cards carry `Vision: V2, V5`. Work that cannot satisfy an invariant stops and asks the owner lane. |
| One source per fact | 0dcd71f2dad | V13: each fact has one source, and everything else links to it or computes it. The rule map, baselines and copied text were deleted, so changing VISION.md requires no other edit. |
| A slop ratchet | b150b23bb7f | Pre-push counts slop per changed file (silent excepts, god-file lines, pass-through wrappers, env reads, layering violations) and refuses a file that gains any against its base. Sweeps lower it; no committed baseline. |
| A navigation method | b520ea46e55 | Four properties every folder ends with (named, placed, current, short at the top), and a per-folder pass to get there. Written, not run. |

The part that makes it work is the pairing: every rule has a check that fails, and every
check reads the source rather than a copy.

## Where amux stands (measured on origin/main, 2026-10-30)

- **The vision exists but is scattered.** The real invariants are spread across root
  `CLAUDE.md` (591 lines), `.claude/rules/ethos.md` (8 rules), `REVIEW.md`, `VERIFY.md`,
  and the owner's global `~/.claude/CLAUDE.md` (1,030 lines, much of it amux API recipes).
  The core one, "eight primitives, do not add a ninth", is a short paragraph in the middle of
  CLAUDE.md.
- **Root CLAUDE.md is mostly history.** About two thirds is incident narrative. Some of it
  describes a Linux box this checkout does not run on: systemd units, Proxmox, Playwright MCP
  ports. An agent reads all of it on every turn.
- **There are no folder rule files.** The only ones outside the root are under
  `templates/`. `crates/amux-server`, `crates/amux-dashboard`, `desktop/`, `cloud/`,
  `scripts/` and `e2e/` have none. `AGENTS.md` is empty, and `GEMINI.md` sits beside it.
- **The slop is concentrated in god files.**
  - `app.js` has 73,436 lines, 3,711 top-level functions, 682 empty `catch` blocks and
    1,397 inline `style="` strings.
  - `session_verbs.rs` has 57,468 lines.
  - 14 server files are over 5,000 lines.
  - Server code discards 712 results with `let _ =`.
- **Some of it is already enforced.** The land gate (tests, e2e and a review against
  `REVIEW.md`), the diagnostic contract (`measured`, `n_considered`), the tmux target audit,
  the token lint and the button audit each enforce one rule. None of them names an invariant
  or links to a vision.

## The plan for amux

### 1. An owner lane: `amux-vision`

One worker owns the vision, the rule tree and the checks, as mixpeek-vision does for
Mixpeek. Other lanes ask it before routing around an invariant, and it reviews changes to
the tree. A new lane is the owner's call. The alternative is to fold this into
`amux-frustrations`, which already owns friction and harness-level fixes; that lane is
busy, though, and the job is different.

### 2. `VISION.md` at the repo root, about 120 lines

- **North star:** the harness gets better as models get better, so amux stays out of the
  model's way (ethos.md's first line).
- **Invariants A1-An**, each one sentence, citing the check that enforces it. The candidates
  already exist in scattered form:
  - **A1. Eight primitives:** board, workers, schedulers, filesystem, groups, memories,
    environment, messages. A feature that re-expresses them is configuration and UX, never a
    ninth primitive.
  - **A2. Only main deploys.** Everything lands on origin/main through the gate. No branches,
    forks or hand-built binaries.
  - **A3. One codebase for local and cloud.** No `if IS_CLOUD`.
  - **A4. Every bug fix brings two things:** the fix at the root, and a log signal so the
    next instance announces itself.
  - **A5. An instrument says whether it measured** (`measured`, `n_considered`), so a zero
    is never ambiguous.
  - **A6. Reuse the existing paradigm** for a view, a dialog or a list. No second copy.
  - **A7. Isolation means isolation:** no board, no injected prompts and no routing for an
    isolated worker.
  - **A8. The owner's gates hold:** money, anyone outside the company, production data, and
    `[owner]` paths.
  - **A9. A durable change carries its recovery,** proven with a real kill and restart.
  - **A10. One source per fact** (Mixpeek's V13).
  - **A11. A change to shared infrastructure is proven against the failure it can cause.**
    It is added from 2026-10-29: the tmux control client passed every test and still
    crashed the tmux server holding the whole fleet.
- **The ethos's eight rules stay** as the review lens. VISION.md links to them and does not
  repeat them.
- **Commit bodies and board cards name what they touch,** for example `Vision: A1, A5`.
  The land gate's review reads the cited invariants.

### 3. A rule tree

- **Root CLAUDE.md** goes from 591 lines to about 120. It keeps how to work here: build,
  test, land, and where things are. The incident narratives move to
  `docs/ethos-incidents.md`, which ethos.md already points to. The Linux box notes move to
  `docs/ops/linux-box.md` or `CLAUDE.local.md`.
- **Folder rule files** for `crates/amux-server`, `crates/amux-dashboard`, `desktop`,
  `cloud`, `scripts` (the builder and the land gate) and `e2e`. Each one holds only what
  is true of its folder:
  - The dashboard file absorbs `css-mobile.md`, `dashboard-e2e.md` and `sse-realtime.md`.
  - The server file holds the diagnostic contract and the tmux target rule.
  - The desktop file holds how to run its e2e tests.
- **Every rule file starts with a header:** parent, owner, and the invariants it
  enforces. An `AGENTS.md` symlink sits beside each one. `GEMINI.md` becomes a symlink.
- **A check, `scripts/check_rules_tree.py`, runs in the land gate and pre-push.** It
  refuses a missing header, a wrong parent or an unknown invariant id, and `--map` prints
  the tree from the headers rather than from a committed copy.
- **The owner's global `~/.claude/CLAUDE.md`** repeats amux API recipes that the `amux`
  CLI and its skill already document. Under A10 they would move to one source. That file is
  the owner's, so this is a recommendation, not part of the work.

### 4. A slop ratchet for Rust and the dashboard

The same shape as Mixpeek's. At push, it counts per changed file, at the pushed bytes,
and compares each file with its own count on the base:

- **Dashboard (`app.js` and friends):** empty `catch` blocks, inline `style="` strings,
  raw px and colours outside tokens (the token lint already does part of this), new
  top-level functions in a file over 20,000 lines.
- **Server Rust:** `let _ =` on a `Result` outside tests, `.unwrap()` and `.expect()`
  outside tests, a handler that returns a bare status with no error body, a file over
  5,000 lines that grows.

A file that gains any is refused, naming the count and the fix. `Slop-Raise: <reason>`
lets a justified raise through, logged. Progress is computed by `--report`, so there is no
baseline file to keep in sync.

### 5. Breaking up the god files, as a method first

`app.js` and `session_verbs.rs` hold most of the slop and most of the cost of reading the
code. Like Mixpeek's navigation method, the first deliverable is the method:

- the target layout (one module per view or API family, as `src/api/` already does for most
  routes);
- the order (leaf views first, shared helpers last);
- the proof per move: the same e2e passes, and the function registry and button audit stay
  green.

Running the method is separate work, card by card, under the ratchet.

## Order

1. `VISION.md` with A1-A11, and the commit-trailer convention. One commit.
2. Root CLAUDE.md slimmed, folder rule files and headers, and `check_rules_tree.py` in the
   gate. One or two commits.
3. The slop ratchet, with negative controls (a raise refused, a new file with slop refused,
   slop already on main not blocking an unrelated edit).
4. The god-file method doc.
5. The owner decides on the `amux-vision` lane, and whether `~/.claude/CLAUDE.md` is in
   scope.
