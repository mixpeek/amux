# Project lifecycle branch validation

Date: 2026-09-20. Branch: `codex/amux-project-lifecycle`.

This validates the project execution path on a disposable server, not a production fleet migration. The existing group, issue, command, attempt, delivery, token and Git integration primitives remain authoritative. Projects own outcomes; temporary workers execute finite claims.

## Automated checks

| Command | Observed result |
| --- | --- |
| `AMUX_SESSION=codex-lifecycle-adherence scripts/test-contended.sh -p amux-server` | 3,225 passed, 0 failed, 36 ignored across 76 result targets, including integration tests. Build contention was observed and reported; the run exited 0. |
| `AMUX_SESSION=codex-lifecycle-adherence scripts/safe-cargo.sh test -p amux-server --lib project_` | 20 passed after the subsequent delivery acknowledgment and interrupted-delivery recovery changes. |
| `AMUX_SESSION=codex-lifecycle-adherence scripts/safe-cargo.sh test -p amux-server --lib steering_restart_reconciliation_works_before_any_fleet_request` | 1 passed on the fresh-schema bootstrap regression. |
| `AMUX_SESSION=codex-lifecycle-adherence scripts/safe-cargo.sh test -p amux-core` | 237 passed, 0 failed. |
| `node --test tests/dashboard-outage-recovery.mjs e2e/state-kernel.test.mjs e2e/outbox-acceptance-recovery.test.mjs e2e/pending-message-projection.test.mjs` | 89 passed, 0 failed. |
| `npm run build:state`; `npm run lint:spa`; `node --check crates/amux-dashboard/static/app.js` | Fresh generated state bundle; 0 lint errors, 50 existing warnings; valid JavaScript. |
| `AMUX_SESSION=codex-lifecycle-adherence scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings` | Passed on the final source, including receipt redaction. |

`project_result_settles_only_its_exact_delivery_even_after_sender_restart` was rerun after routing copied receipt text through the existing secret redactor: 1 passed. It checks stale-generation refusal, exact-delivery settlement, unrelated-input preservation, redaction and idempotency.

The full server result precedes the final recovery deltas; the targeted tests above cover those changes. It is not presented as a second full-suite run on later source. Normal commit hooks also check workspace/all-target compilation and lints.

## Browser acceptance

The runner creates a new HTTPS server, database, local bare Git remote, source checkout and isolated tmux socket. Playwright uses the mounted dashboard. It does not mock/intercept API requests. Providers are deterministic, nonbillable CLI fixtures; SQLite, processes, filesystem writes, commits, checks, merges and worktree deletion are real.

Product actions are through UI controls. Reads of Git, SQLite and process state independently check the outcomes. Test fault injection is confined to the disposable fixture. All test instances are stopped; failed and passing artifacts are retained separately.

Acceptance scenarios:

1. Create/configure a project, persist an unsent draft through reload, submit an outcome and reconcile its duplicate using one intake call.
2. Run two independent tasks concurrently in separate real worktrees. Show one global orchestration with two executors and the actual task in terminal details.
3. Verify committed artifacts on the disposable remote main; expire executors and remove their clean integrated worktrees.
4. Pause running work; prove its heartbeat stops before the UI says Paused. Resume without consuming another logical attempt.
5. Fail an artifact check, execute exactly one repair, rerun verification and integrate the corrected output.
6. Restart the server while an executor runs. Preserve its claim, saved draft and result; settle its delivery and retire it afterward.
7. Hold subsequent work at an observed token limit, explain the hold, and continue after a UI policy change.
8. Bound two malformed requests to two interpretations each. A duplicate inherits the waiting reason. Idle state makes no provider calls, and exhausted requests do not starve later work.
9. Check desktop/mobile card bounds, independent executor-provider settings across all four offered providers, and migration preview/apply/rollback with preserved identity and evidence.
10. Preserve an uncommitted artifact and its worktree, refuse false integration/retirement, then pause the project.

Final command:

```bash
python3 e2e/project-lifecycle/run.py --binary ../project-lifecycle-e2e/amux-server-build12 --out ../project-lifecycle-e2e/full11 --port 18971
```

Result: **10 scenarios passed, 0 uncaught browser errors, exit 0**. Desktop/mobile and active/completed orchestration screenshots were inspected. Exactly 10 fixture intake calls and 10 fixture execution calls were recorded across success and deliberately failing scenarios. The duplicate parallel request used one intake interpretation and exactly two executor calls. The failed artifact check used attempts `[1,2]`; paused work resumed within attempt 1. Empty idle checks added zero calls.

Server build before/after restart: `b633cded5b4033e7` (same image, different PIDs). Binary SHA256: `b633cded5b4033e7c1a59c5d67ce4067950b1cebbe40de2a2a90192403d7ae7c`. Artifacts: `../project-lifecycle-e2e/full11/` from this checkout, including `results.json`, health snapshots, logs and screenshots. Fixture: `/private/tmp/amux-project-29penmxk`. The server process and private tmux instance were independently confirmed absent after cleanup.

The final receipt text-redaction change followed this UI build and passed its focused real-database test plus strict workspace Clippy; it changes copied history text, not UI/execution control flow. This distinction is retained instead of relabeling an older binary as the final source.

## What failed before the fixes

The earlier isolated runs are retained as negative controls. Run `full6` detected duplicate repair execution (`[1,1,2]` instead of `[1,2]`), exposing the competing timer/idle-hook delivery paths. Run `full7` reached Verified after restart but could not retire because the delivery remained claimed. Later runs exposed the fresh-database sender-column gap. Screenshot inspection found mobile clipping and duplicate orchestration rows.

`full10` failed a test assertion that expected a space between a filter label and count; the browser rendered a newline. A separate UI inspection showed one orchestration and zero duplicate legacy rows. The corrected assertion reads the count element directly, retaining the exact expected value of one.

## Limits and rollout

No paid model trials were added. These tests establish harness behavior, not live model reasoning quality, real token savings or universal CLI compatibility. Coordinator intake currently uses Claude's read-only helper; executor profiles retain the existing Claude, Codex, Gemini and Ollama adapters. All four profile configurations are exercised through the UI; their live providers are not invoked.

Usage gaps remain visible. Observed budget stops prevent subsequent calls; they cannot impose a hard cap on an already running provider turn. An operational failure or real authorization boundary is a recorded waiting reason, not fabricated Verified work.

Production workers and boards remain on their existing path until explicit project migration. Legacy dispatch has no authority over project-owned work. Apply/rollback require a paused project and matching revisions; changed work is never overwritten. No main deployment or production migration is included in this branch validation.
