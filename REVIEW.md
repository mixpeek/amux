# What the land review looks for

`scripts/land_review.py` reads this file, and the nearest `REVIEW.md` above each
changed file, on every review. Keep it short: it is context for every run.
Add a line when the review keeps missing, or keeps flagging, the same thing.

## A failure here is

- **A check that cannot fail.** A probe, gate or diagnostic that prints a clean
  answer when it never ran: a hardcoded verdict, an empty list from a failed
  command, a zero with no `measured` beside it (`.claude/rules/ethos.md` rules 4
  and 7).
- **Reading the wrong bytes.** Deciding from the shared worktree or `HEAD:` where
  `origin/main:` is meant, or writing a whole file built on a stale read.
- **A contract the tree enforces and the change forgot**, each of which reddened
  main on 2026-10-26 or 27:
  - a new `/api` route with no `RouteEntry` in `api/request_log.rs`
  - a new timestamp column with no unit in `invariants/checks.rs` `TIMESTAMP_COLUMNS`
  - a background loop in `lib.rs` outside the system job registry
  - a new tree-walking test missing from `scripts/ci-affected.mjs`
- **Harm to other lanes:** killing processes by pattern, a fixed path in a shared
  `/tmp`, rewriting a script in place while it may be running, `8822`.
- **The dashboard:** an SSE data source with no polling fallback, a second row of
  controls on a 375px phone, a control whose label and class disagree.
- **A lie to the owner or a worker:** a notice, verdict or log line that names the
  wrong card, session or cause.

## Not a finding

Style, naming, formatting, comment wording, "could be simpler", missing tests for
code the change did not touch. Pre-existing bugs only as `pre-existing`, never as
`important`.

## Ignored paths

`frustrations.md`, `frustrations-archive.md`, `docs/**`, `e2e/test-results/**`,
`**/*.snap`, vendored files under `crates/amux-dashboard/static/vendor/`.

## Every finding

Names a file and line that exist in the diff or the code it calls, says the
concrete input that goes wrong, and what the wrong output is. No guessed
filenames. The summary states the count, or says "no issues".
