# Worker drain and steering verification — 2026-09-12

This follow-up addresses workers retaining backlog/To Do/Done work and steering messages being acknowledged before submission. Live observations used the local server and read-only board diagnostics; private raw evidence is in the dedicated lifecycle lab's results directory.

## Findings and changes

- **Busy composer falsely confirmed.** The verifier returned `Confirmed` for `StillThereGenerating`, bypassing its own bare Enter retry. A busy frame now requires composer release or provider-owned transcript/queue acceptance. Exact native enqueue text and its timestamp are checked; old identical entries, substrings and quoted entries are negative controls. Repeated unsubmitted frames log `generating_composer_unsubmitted`.
- **Exact claim vetoed stale recovery.** The live `mixpeek-cicd` trace reported 18 eligible todos behind exhausted reminders; its Doing cards were untouched for 11–14 hours. The runtime current-claim guard introduced in `af223803` (2026-09-08, exact identity recovery) returned before the existing stale-reclaim action. It now permits canonical recovery of the same abandoned claim and the existing capture-shell WIP exemption. Recovery rechecks the full selector on the serialized writer; current child work is protected. Reclaimed tasks remain in To Do, never falsely completed or discarded.
- **Failed reminder insertion consumed its retry.** Advancement, verification, decomposition, backlog and continuation reminders now check durable queue acceptance before recording cooldown/budget events. Failure produces `nudge-delivery-failed` and `board_nudge_enqueue_failed`, so the next tick can retry.
- **Successful verification still waited a day.** Previously a successful eight-card verification batch blocked the entire lane for 24 hours. Once those exact cards are resolved, the next batch is eligible immediately. An unresolved batch stays quiet under the existing retry limit. Normal completion/evidence gates remain authoritative.

## Acceptance coverage

The consolidated catalog now contains 85 cases. Added: `LC-STEER-NATIVE-ACK`, `LC-BOARD-ENQUEUE-RETRY`, `LC-VERIFY-BATCH-DRAIN`, and `LC-STALE-CLAIM-DRAIN`. The full runner explicitly runs the real tmux submission replay.

Driver regressions exercise queue refusal then recovery, ten Done cards across successive batches, an abandoned exact claim then the next To Do pickup, and a captured prompt that must not block real work. The tmux replay walks the actual asynchronous capture/verifier using a dedicated terminal with controlled busy/cleared frames. It launches no model and is not provider completion evidence.

## Validation

Initial full server run: 60 targets, 2,622 passed, 10 failed, 33 ignored. Seven failures were host admission; the sessions-cache discovery race passed on an isolated rerun; restart persistence detected a shared executable replacement during the test. The new tmux target audit caught a hand-spelled exact target in the replay cleanup; it now uses the canonical session-target helper.

The real tmux replay passed (1 test). Restoring the old busy-composer confirmation branch with `scripts/mutate.sh run` caused that same replay to fail: observed `Confirmed`, expected `Stuck`; the mutation was then reverted.

The initial new claim regressions caught a capture-card cooldown path that still vetoed To Do pickup. The exemption now reads the same raw-prompt metadata as WIP selection instead of relying on a particular diagnostic skip string.

`python3 -m unittest discover -s scripts/lifecycle -p 'test_*.py'`: 9 passed. All-target workspace clippy passed. Final `scripts/test-contended.sh -p amux-server --no-fail-fast`: 60 targets, 2,625 passed, 8 failed, 33 ignored. All six new regression tests passed. The seven admission failures remain; the cache discovery race also recurred under full concurrency despite its isolated pass. Restart persistence and the tmux target audit now pass. The final real tmux replay passed again (1 test); both exact-claim recovery tests passed on the final source (2 tests). No replay terminals remain.

Final `scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings`: exit 0. Deployment identity and observed live recovery will be recorded after publication.

## Live limits

At inspection, `/api/debug/steering` measured zero server-queued messages. This proves queue depth only, not that every provider consumed a message. The local host still denies new worker admission due to reported swap usage; Sonnet/Gemini native end-to-end completion cannot be claimed while that prerequisite is denied. Existing workers can still receive steering and board dispatch. One worker (`desktop`) explicitly showed its provider session limit in the terminal. Work waiting on human decisions, live imports, dependencies or provider limits must retain truthful blocked state rather than being marked complete.

## Integration with the concurrent mobile fix

Rebased onto `5eaf25e0` without altering its UI changes. Cargo-owned runs on the rebased implementation passed: board drive 169 tests; submission checks 13; opt-in real tmux replay 1. The incoming mobile asset test initially failed because its 500-character slice stopped inside a long comment. It now reads the mobile CSS declaration block; all 38 asset tests pass. Replacing that rule's fixed positioning with absolute positioning made the pinned test fail, and the mutation restored the CSS.

A supplemental direct invocation of the shared unit-test executable found zero tests after another build replaced it. Those zero-match invocations are invalid evidence and are excluded above; the Cargo-owned reruns are the authoritative results.
