# Project lifecycle branch validation

Evidence updated: 2026-09-21. Branch: `codex/amux-project-lifecycle`.

This validates the project execution path on a disposable server, not a production fleet migration. The existing group, issue, command, attempt, delivery, token and Git integration primitives remain authoritative. Projects own outcomes; temporary workers execute finite claims.

## Current checkpoint — consolidated Projects UI

The private UI at `https://localhost:18972/` runs commit
`fdd4c33d21f53d6a60fc9da6b41f8b243b7f24a2`, build `18c849dccb3a0085`.
Projects is the entry point for new project work. The global Orchestrations tab
and Board launch form are removed; legacy records remain accessible as history.
Workers are disposable task executors, not permanent orchestrators. The bounded
planning call and executor can use different provider/model profiles.

Evidence root: `BASE=../extractor-case-run` relative to this checkout. The exact
build and private deployment are recorded in `BASE/astra-project-ui-build.json`
and `BASE/astra-project-ui-private-deploy.json`. No production migration or
upstream/main push is claimed.

The native Amux-authored UI passed **36 focused desktop/phone checks**, **45
dashboard asset checks**, workspace check and strict all-target Clippy. State
tests passed 27, outbox tests 14; SPA lint reported zero errors and 51 existing
warnings. The refresh-failure negative control failed the intended assertion.
See `BASE/aab11-focused-ui-r3-results.json`, `BASE/aab11-rust-gates.json`,
`BASE/aab11-state-lint-results.json` and `BASE/aab11-refresh-negative.json`.

The full fresh-server lifecycle run passed **14 scenarios**, with zero uncaught
browser errors, 14 fixture intake calls and 13 fixture execution calls in
571.84 seconds. Build identity remained unchanged across the deliberate restart;
the fixture server and isolated tmux socket were stopped. The exact command,
binary and test SHA are in
`BASE/aab11-full-new-server-r4/completion-proof.json`. Four desktop/phone/evidence
screenshots were independently inspected. Providers were deterministic and
nonbillable; the HTTPS UI/API, SQLite, tmux, Git, verification and retirement were
real. This does not establish live model quality or whole-project acceptance.

Earlier runs and their failing selectors are retained in
`BASE/aab11-full-new-server*`; they are not full-suite passes. In particular, r3
passed 13 scenarios before an ambiguous Cancel locator stopped the last check.
The parent repaired test selectors and strengthened
terminal/evidence assertions after a failed native local-model edit. These are
parent-authored test changes, not native product implementation. The rejected
native report and source are preserved in
`BASE/aab11-local-rejected-evaluation.json`; no completion credit was given.

**Task Verified is not project Accepted.** The live extractor project correctly
shows project acceptance **Not configured**. AAB-10's independent acceptance
contract is unfinished and is not included in the deployed image. Its partial
workspace check and three unit tests do not validate the unregistered acceptance
module. Current acceptance must eventually bind the approved contract, current
intent, integrated main and retained evidence; reopening work or changing any
of those inputs must invalidate current success without rewriting history.

PAA-5 is Verified at report `0743d8c61b25ddc967c383b4ca02e27a42860a66`,
integrated into disposable local main `c1d308e3a6348f92d36dc802e99c744d34836706`.
Its 16 declared assets remain retained after its worktree was removed and worker
`px-extractor-astra-acceptan-460fb6aa19` expired. Actual UI inspection opened the
read-only report, decoded the 1280×2186 PNG, played the 19.48-second Studio video,
and found all four retired executor workers in the existing Expired section.
See [current extractor evidence](../../../extractor-case-run/CURRENT-EXTRACTOR-EVIDENCE.md),
`BASE/paa5-verified-retirement-proof.json` and
`BASE/aab11-live-retained-evidence-ui.json`. The initial prototype was direct
Codex work; later native Amux adoption, repair, verification, integration and
retirement were supervised. This was not native execution from inception.

AAB-11 remains open: project terminals still expose a legacy Fan-out tab on
first open, worker headers can show a default model instead of the configured
model, and the terminal directory bar shows the source repository instead of
the active worktree. Closed dialogs also remain in the accessibility tree
despite their transparent appearance. The scoped Cancel test does not fix that
product defect. These are recorded in `BASE/aab11-ui-followup-review.md`.
Passing lifecycle scenarios does not close those UI defects or establish whole
project acceptance.

## Earlier checkpoint — extractor acceptance and startup image

Evidence root below: `BASE=../extractor-case-run` relative to this checkout.
`BASE/final-evidence-brief.json` separates real acceptance executed on 182 from
the final ab2 launcher-image validation and read-only post-deploy audit. No
result below is a blanket all-tests-pass claim or a full-suite rerun on ab2.
This section preserves that earlier checkpoint; the current runtime is above.

### Real extractor outcome

On Amux runtime `182be3316eb490b7cc2e2b24811a0cb9a6223ed0` (build
`2c54a375216d3c9c`), **PAA-1 through PAA-4 are Verified**: one accepted outcome
and three tasks. Disposable local main is
`dc483a023e74d9a35d7d99dd3607cf94c7c1f99b`. All three task commits are ancestors
of that main; all three executor worktrees were removed and workers expired.
The independent audit has **48 passing checks**, `complete: true`, and the
export preserves **26 explicitly retained task outputs** with matching hashes.
This is local acceptance and integration, not a production or upstream push.

The original implementation prototype and historical service/provider tests
were **direct Codex work**. Subsequently, actual Amux native Astra coordination
and executors adopted and repaired that code, submitted structured reports,
verified source and merged candidates, ran normal hooks, merged disposable
main, retained evidence and retired executors **under parent supervision**.
The parent supplied independent audit/export/browser observation, not product
source. This was supervised recovery, not an uninterrupted autonomous run.

PAA-4's first independent generation-3 gate timed out at 600 seconds. The parent
used the actual Projects UI to set the command bound to 1800 seconds and request
**Rerun checks** on the retained `f303abcd9ebda8e994e462111bee542412a3f603`
report. The receipt says `model_attempt_granted: false`; attempt 3, generation 3,
report identity and earlier failures were preserved. The retained executable
check is `python3 acceptance/extractor/gate.py all`, alongside the project gate
`git diff --check`, verified in each immutable candidate phase. No additional model attempt
was granted by this verification-only recovery. Fresh browser gates then passed
**11 checks each**, with zero browser errors, on both that source and merged
`dc483a023e74...`. The committed report's earlier pending-browser text is immutable
submission-time evidence; later gate receipts establish completion without
rewriting it.

Exact evidence under `BASE`:

- [Retained output index](../../../extractor-case-run/verified-assets/dc483a023e74/INDEX.md),
  `amux-acceptance-audit.json` and
  `verified-assets/dc483a023e74/export-manifest.json`: report links, 48 audit
  checks, retirement and 26 hashes.
- `amux-produced-assets/paa4-7shfydo4/{report,sha256,execution}.json`: source
  browser checks, source identity and isolated-process cleanup.
- `amux-produced-assets/paa4-0rjnrbvd/{report,sha256,execution}.json`: merged
  browser checks and identity. Screenshots and raw WebM remain beside the reports.
- `astra-verification-ui-retry-proof.json`: actual UI action and exact retained
  report bound to the verification grant.
- `astra-final-ui-proof.json` and `astra-final-human-ui-review.json`: four
  Verified cards and a read-only retained Markdown preview after retirement.

### Harness results by exact revision

| Revision / command | Recorded result and scope |
| --- | --- |
| `182be3316eb4`: `scripts/test-contended.sh -p amux-server --no-fail-fast` | **3,281 passed, 3 failed, 36 ignored**, 76 result targets, exit 101. Failures: live-host admission plus two route inventory checks. |
| `182be3316eb4`: focused project/fanout tests, workspace check, strict all-target Clippy | 65 project tests and 14 fanout tests passed; check and Clippy exit 0. State 27 and outbox 14 passed; SPA lint 0 errors, 50 existing warnings. Targeted verification/cleanup mutations failed the expected assertions and source was restored. |
| `182be3316eb4`: full isolated UI | **13 PASS**, zero uncaught browser errors, 14 fixture intake and 13 execution calls. Same build `2c54a375216d3c9c` before/after restart; private server/socket cleanup confirmed. Providers fake/nonbillable; UI, DB, Git, tmux, worktrees and retirement real. |
| `424e4b9d9d5ded4f9805457fb008dcade58d5357`: `scripts/safe-cargo.sh test -p amux-server --test route_table --test route_table_completeness` | **4 passed** across the two targets. Three missing POST inventory entries corrected; workspace check and strict Clippy passed. This does not relabel the earlier full suite as green. |
| `424e4b9d9d5d`: full isolated UI | **10 of 13 scenarios passed**, then verification-retry fixture failed before provider input. Unsent packet and stopped executor retained; no provider execution for that task. Not a 13-scenario pass. |
| `fb31cbd536969e02ebd052e98882156b8e3f8693`: full isolated UI | **2 of 13 scenarios passed**, then PU-5 setup receipt timed out before provider input. Focused gates and long-input/stale-UI mutations had passed; this failure exposed healthy slow profile setup exceeding the acknowledgement window. |
| `ab2b73576000cd5808e8b583269f9b2e144b65e5`: focused startup gates | 2 startup tests; 5 shell/cwd/env/pause checks; 65 project tests passed. Entry-as-completion and late-receipt mutations each failed as expected; restored startup tests passed. Workspace check and strict all-target Clippy passed. |
| `ab2b73576000`: full isolated UI | **13 PASS**, zero uncaught browser errors, 14 fixture intake / 13 execution calls. Restart image identity and private fixture/server/socket cleanup verified; parent inspected four desktop/mobile screenshots. |

Full 182 UI command:

```bash
python3 e2e/project-lifecycle/run.py --binary ../extractor-case-run/amux-server-astra-182be3316eb4 --out ../extractor-case-run/lifecycle-ui-182be3316eb4 --port 18973
```

Evidence: `BASE/lifecycle-ui-182be3316eb4/{results,completion-proof}.json`;
`BASE/harness-validation/424e4b9d9d5d/` retains
`astra-full-server-result-r2.json`, its full log,
`astra-route-inventory-results.json`, focused logs and copy manifest.
`BASE/lifecycle-ui-424e4b9d9d5d/` retains the failed UI run and
`/private/tmp/amux-project-67b4uvwa` its fixture. The host-admission failure
measured 32,396 MB swap; its guard remains unchanged and failing evidence is
retained. Earlier failures below remain part of the chronology.

### Final exact image and private deployment

Final runtime is `ab2b73576000cd5808e8b583269f9b2e144b65e5`, binary SHA256
`36c0148191d201f507e99a9d7d0a3edd7de0c4e921e81c8c0adb9f7b9fd81eb5`.
Both UI health snapshots identify build `36c0148191d201f5` across restart.

```bash
python3 e2e/project-lifecycle/run.py --binary ../extractor-case-run/amux-server-astra-ab2b73576000 --out ../extractor-case-run/lifecycle-ui-ab2b73576000 --port 18973
```

Evidence: `BASE/lifecycle-ui-ab2b73576000/completion-proof.json` and
`parent-visual-review.json`. `BASE/astra-slow-setup-results.json` records exact
commands, expected mutation failures and restoration, including
`scripts/safe-cargo.sh test -p amux-server --lib startup_shell_`, `project_`,
`scripts/safe-cargo.sh check --workspace` and
`scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings`.

The parent deployed this exact image only to **private port 18972**, PID 73659
at observation. `BASE/astra-slow-setup-private-deploy.json` records the change.
The read-only `BASE/amux-acceptance-audit-ab2b73576000.json` passed **48/48**;
`BASE/astra-deployed-final-ui-proof.json` passed with all **four Verified** cards
and the retained report preview accessible after executor retirement.
**The real source and merged acceptance gates executed on 182; they were not
rerun on ab2.** The ab2 observations establish retained state/assets and UI
access after deployment, not fresh product verification. Documentation and
feature-branch publication remain parent-owned; no upstream-main claim follows.

The 424 and fb31 failures remain at `BASE/lifecycle-ui-424e4b9d9d5d/` and
`BASE/lifecycle-ui-fb31cbd53696/`. Neither is relabelled green by the ab2 result.
The full 182 server suite still has its recorded three failures; the later
four route-test passes fix two, and the live-host admission failure is retained
unchanged. No full-server-suite rerun on ab2 is asserted.

### Shared architecture and limits

KISS here means reusing groups, issues, claims, command receipts, delivery,
usage, Git verification and retirement. Deterministic scheduling/readiness,
same-project output continuation and current-turn delivery evidence need no
model polling. Duplicate receipts reuse interpretation; owner steering stays
on its active claim; byte-identical commands run once per immutable verification
phase. Checks-only recovery reuses a retained report without buying a repair
turn. Startup uses the same short-script transport and one bounded shell budget,
with separate entry/completion receipts and measured stages; a slow profile does
not fabricate another model attempt. These are token-conservative mechanisms, **not a measured token-saving
percentage**. Unpriced Astra cost and observed outside-attempt usage remain
visible; neither becomes zero or disappears from history.

No production migration/deployment, GitHub main or upstream Mixpeek push is
claimed. Current persistent DB, authentication, Ray and all live provider/service
flows were not rerun by Amux. Historical direct service checks retain their
HDBSCAN noise and clustered-counter limitations. Shared Serve reservations
changed from 14 to 13 apps, 10.6 to 8.6 warm CPU and 26.5 to 22.5 GiB memory;
these are configuration reductions, not realized dollar savings. Standalone
package SDKs are unchanged; the Studio generated client was checked. Studio's
1,095 known type errors remain under its baseline. The raw 18.52-second merged
browser recording includes startup and is not a narrated production demo.
Private self-signed TLS disables the offline service worker; no user-browser
trust bypass is claimed. Explicit isolated Codex loopback authorization and
subscription/transport limitations remain as described in
[coordinator acceptance](project-codex-coordinator.md).

## Historical evidence and failure chronology

The following sections retain their original revision-specific results. Pending
real-case statements describe those earlier checkpoints, superseded only by the
current acceptance evidence above; they are not current work instructions.

## AAB-3 validated runtime revision: 7ee3eb6f313b

Runtime commit: `7ee3eb6f313b3bbbd26a7a31e692dabfff2b256b`; normal commit
hooks passed. The following parent checks supersede pending validation for this
revision only; earlier runs and their limits below remain historical evidence.

| Command | Parent result |
| --- | --- |
| `scripts/safe-cargo.sh test -p amux-server --lib project_` | 50 passed, 0 failed; includes retired-executor ownership/reverify, cadence/pause/cancellation, report admission and complete command preflight. |
| `scripts/safe-cargo.sh test -p amux-server --lib verification_cannot_pass_against_original_checkout_or_fallback_to_it` | 1 passed, 0 failed. |
| `scripts/safe-cargo.sh check --workspace` | Exit 0. |
| `scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings` | Exit 0. |

Evidence: `/private/tmp/amux-astra-20260920/logs/aab3-cadence-parent2-results.json`
and its four referenced logs. These are focused source gates, not a new full
server-suite run. Parent mutation controls separately restored the old serial
cadence and late per-command validation: `slow_failing_legacy_sweep_preserves_project_report_progress_and_pause`
and `project_verification_preflights_all_commands_before_any_execution` each
failed its expected assertion (exit 101). Exact source was restored; the restored
`project_ --lib` run passed all 50 tests. Evidence:
`/private/tmp/amux-astra-20260920/logs/aab3-cadence-preflight-negative-results.json`
and its cadence, preflight and restored-test logs.

Full isolated UI command:

```bash
python3 e2e/project-lifecycle/run.py --binary ../extractor-case-run/amux-server-astra-7ee3eb6f313b --out ../extractor-case-run/lifecycle-ui-7ee3eb6f313b --port 18973
```

Result: **11 PASS, 0 uncaught browser errors**, with 13 fixture intake calls and
11 fixture execution calls. This includes the canonical recheck after executor
retirement, unchanged seven card IDs before later work, migration rollback and
the dirty-checkout refusal/retention scenario. Both health snapshots identify
commit `7ee3eb6f313b3bbbd26a7a31e692dabfff2b256b` and build
`0322f12d3fa342f7` across restart (different PIDs). Binary SHA256:
`0322f12d3fa342f76249dd6c80272d9532a108de378d8f5d431189e1b396e4e6`.
Evidence: `../extractor-case-run/lifecycle-ui-7ee3eb6f313b/results.json` and
`completion-proof.json`; the latter independently records fixture process and
socket stopped. Desktop/mobile screenshots are retained; current screenshot
inspection remains with the parent, not claimed here.

Providers were deterministic, fake and nonbillable; UI, database, processes,
Git, worktrees, verification and retirement were real. This validates the
shared harness, not completion of the actual extractor project: at that handoff
PAA-2 generation 6 was still verifying, and the project was **not complete or
Verified**. The current evidence above records its later completion. No production rollout or paid-provider outcome is implied. Earlier
failed fixtures, including `lifecycle-ui-b559-settled` and
`lifecycle-ui-f1277e53bf18`, and the generation-5 late candidate-command rejection
remain retained; successful later evidence does not relabel those failures.

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

No paid model trials were added by those fake-provider fixtures. Those tests establish harness behavior, not live model reasoning quality, real token savings or universal CLI compatibility. The initial coordinator used Claude's read-only helper; AAB-1 subsequently added explicit Claude/Codex coordinator selection independently of the executor. Executor profiles retain the existing Claude, Codex, Gemini and Ollama adapters. All four profile configurations are exercised through the UI; their live providers are not invoked.

Usage gaps remain visible. Observed budget stops prevent subsequent calls; they cannot impose a hard cap on an already running provider turn. An operational failure or real authorization boundary is a recorded waiting reason, not fabricated Verified work.

Production workers and boards remain on their existing path until explicit project migration. Legacy dispatch has no authority over project-owned work. Apply/rollback require a paused project and matching revisions; changed work is never overwritten. No main deployment or production migration is included in this branch validation.

### Real extractor-consolidation acceptance attempt — 2026-09-20

Submitted the user's full shared extractor consolidation request through the
Projects UI on a separate server (`https://localhost:18972`), with a private
Mixpeek clone and local bare remote. The configured coordinator was Claude
Haiku; executor Sonnet, capacity 1, maximum 2 attempts. The command was retained
and its draft cleared. **That initial real autonomous acceptance attempt did not complete:** the
Claude CLI returned its account weekly limit before any model input/output
tokens, with reset Sep 23 at 11am America/New_York. No paid overages were used.
Implementation of the Mixpeek case proceeded separately and is not evidence of
Amux decomposing, executing, or merging that command autonomously.

The attempt exposed a diagnostic defect: a long CLI JSON metadata envelope
hid the useful quota result after the 400-character truncation, and exhausted
intake appeared as a clarification request. The helper now extracts a structured
provider error before truncating it; receipts and duplicates preserve the quota
reason and reset text. The existing two-attempt bound remains; this does not add
an automatic quota-reset retry or paid fallback.

Regression validation:
- Helper subprocess error tests: 6 passed, including oversized JSON metadata.
- Quota receipt/duplicate test: passed.
- New isolated server browser run: 11 scenarios passed, using captured quota
  shape at the explicitly fake provider boundary and real tmux/Git/UI behavior.
- The quota scenario preserves the command, shows the reset, and stops retrying.

Local evidence: `work/extractor-case-run/` next to the isolated checkouts,
including live submission screenshot, CLI zero-token result, and
`amux-quota-ui/results.json`. Mixpeek's independent test report distinguishes
browser/schema checks from the service-backed batch run that hit its memory
limit. Neither that batch run nor the provider-blocked Amux run is a completion
or production deployment claim.
