# Codex project coordinator acceptance

AAB-1 adds explicit `claude` / `codex` coordinator selection to project policy.
The executor retains its independent provider and model. Settings offer
`gpt-6-astra` for Codex and preserve custom model names, unsent drafts and reloads.
The interpretation call receives the provider and model from the same durable
project policy snapshot. A model name never selects the provider.

The Codex path runs one cold `codex exec` subprocess for each recorded intake
attempt, through the existing concurrent pipe reader, deadline, output limit and
process-group cleanup. It does not prepare a warm process, invoke a second CLI,
or fall back to Claude. Read-only mode, disabled execution features, empty MCP
configuration, ignored user configuration/rules, a fresh temporary working
directory and zero project-document bytes isolate interpretation from worker
instructions and tooling. `CODEX_HOME` and subscription authentication remain
available. Claude's existing data-only invocation remains unchanged.

`--ignore-user-config` and `--ignore-rules` were confirmed in the installed
`codex exec --help`. The exact config overrides and feature disables were checked
with the installed CLI's offline `features list`; this checks configuration,
not model behavior. See the [official non-interactive documentation](https://learn.chatgpt.com/docs/non-interactive-mode)
and [configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).

Amux does **not** claim to disable every internal Codex transport retry. The
installed CLI rejects overrides of the reserved `model_providers.openai` ID.
Those attempted overrides were removed; built-in transport retry behavior stays
with Codex, under amux's outer helper deadline. Unbounded connection retries are
disabled. One amux subprocess invocation is the unit of an intake attempt.

JSONL parsing requires a completed turn and a final assistant response, rejects
unexpected tool items, and preserves bounded quota/failure messages, including
errors emitted late in a nonzero process's stdout. Returned usage subtracts
cached input from total input before recording cache reads separately, so project
token totals count each token once. Raw provider usage is retained; absent usage
stays absent. No dollar usage is invented for subscription calls. Logs include
`project_provider_failed` with provider, model, measurement and population fields.

## Private worker routing

Fresh and resumed workers receive `AMUX_HOME` and `CC_HOME` from their launching
server, and `AMUX_URL` and `AMUX_API` from its endpoint. This prevents the Bash
CLI from silently addressing another local server. Authoritative values are
also exported after scoped env files are sourced. `worker_harness_routing` and
`worker_harness_refresh_failed` make launch and refresh observable.

A linked Git worktree needs write access to the paths reported by
`git rev-parse --path-format=absolute --git-common-dir --git-dir`, not its `.git`
pointer file. Codex launches add these repository-specific metadata directories.
They do not grant access to the original checkout's working files. Existing
explicit sandbox and provider options are preserved; no user configuration is
rewritten. `codex_git_write_paths` records resolution, and
`codex_git_paths_unmeasured` reports an unavailable probe.

New project workers receive the project model but do not inherit the bootstrap
worker's flags. Real Codex executors need an explicitly authorized runtime
configuration allowing loopback API requests for reports, such as
`-c sandbox_workspace_write.network_access=true` under `workspace-write`.
The parent is configuring the isolated test runtime for this; this change does
not silently enable network access globally or override an explicit sandbox.

## Verification scope

`e2e/project-coordinator.mjs` executes the shipped project UI in Chromium with
fixture requests. Parent reported its pre-fix failure on the absent coordinator
selector and its post-fix pass for independent profiles, Astra suggestions,
save/reload and drafts. Rust fixtures exercise real interpretation-to-CLI routing,
recorded attempt counts, provider failure/quota handling, usage, project policy
round trips and linked-worktree Git metadata. They spend no live model calls.

**Real provider acceptance remains pending.** Fixtures and offline config
validation do not prove subscription access, runtime tool isolation or a live
Astra interpretation/execution/integration cycle. After independent review and a
separate test-server restart, the parent will submit the real extractor request
through the UI. No worker server restart or deployment is performed by AAB-1.

Parent-run focused evidence (2026-09-20), through `scripts/safe-cargo.sh` with
`AMUX_SESSION=amux-astra-bootstrap`, the private amux home/endpoint and the shared
Cargo target:

| Command | Observed result |
| --- | --- |
| `scripts/safe-cargo.sh test -p amux-server --lib project_` | `test result: ok. 23 passed; 0 failed` |
| `scripts/safe-cargo.sh test -p amux-server --lib helper_` | `test result: ok. 23 passed; 0 failed` |
| `scripts/safe-cargo.sh test -p amux-server --lib board_lifecycle` | `test result: ok. 16 passed; 0 failed` |
| `scripts/safe-cargo.sh test -p amux-server --lib private_worker_harness_routes_cli_and_hooks_without_changing_provider_policy` | `test result: ok. 1 passed; 0 failed` |
| `scripts/safe-cargo.sh test -p amux-server --lib codex_linked_worktree_writes_use_its_actual_git_metadata` | `test result: ok. 1 passed; 0 failed` |
| `scripts/safe-cargo.sh check --workspace` | exit 0 |
| `node e2e/project-coordinator.mjs` | pre-fix: missing coordinator selector; post-fix: `project coordinator UI: PASS` |
| `node --check crates/amux-dashboard/static/app.js` | exit 0 |
| `git diff --check` | exit 0 |

These are focused checks, not a claim that the entire server test suite ran.
Strict `scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings`
passed with exit 0 (parent-run, 42.60 seconds). The dedicated `codex_helper`
test result and exact committed build remain tracked in the private handoff
until their results arrive.

## Live intake repair (AAB-2)

The first live intake on `ab49eb5090a768eb7293d48e0980c7768e969b6f`
exhausted two interpretations of receipt 6. The parent paused only the isolated
acceptance project; its original receipt, attempts and error remain intact.
A nested worker reproduction could not initialize the Codex app server and
failed before model initialization. The parent ran the captured argv and tiny
prompt once in the actual server context. Codex 0.153.4 exited 0, emitting two
`item.completed` diagnostics with `item.type=error`, followed by the requested
JSON and `turn.completed` usage (6,264 input tokens, 9 output tokens).
These are configuration diagnostics, not tool execution or a fatal turn:
`skip_host_skill_discovery` is unstable, and code mode reports its disabled host.
We retain the restrictive options and subscription authentication. We do not
suppress the warnings or enable tools to quiet them.

The helper accepts diagnostic items only alongside a completed data-only turn
and final assistant response. It logs at most four diagnostic messages, each
bounded to 400 characters, with `codex_helper_diagnostics`, the observed count
and whether parsing accepted the turn. Top-level errors, failed turns, quota
items, missing finals and unexpected execution items still fail. Structured
provider failure carries observed turn usage to the receipt, even when the
response is rejected; missing usage remains unmeasured. Successful helper text
still passes the existing task-plan validation before any tasks are created.
Transport retries internal to Codex retain the limitations described above.

An operator can now select **Retry intake** on an exhausted failed receipt.
`POST /api/projects/{name}/commands/{id}/retry` takes `idempotency_key`,
`expect_attempts` and `expect_revision`. A serialized write compares those
values, refuses active leases, prepared plans and duplicate receipts waiting on
another receipt, and records one additional attempt grant in the original
receipt's metadata with its previous result. It never resets attempts, creates
a replacement receipt, unpauses the project or bypasses project budgets. The
same request key is idempotent even after its attempt completes; a different
stale request conflicts. The browser retains the key across uncertain responses.
Exhaustion is displayed as an attempt limit, not as a demand for clarification.
Logs expose `project_intake_retry_granted` and `project_intake_retry_refused`.

Codex 0.153.4 also renders an unstyled model/path footer under the dim empty
`Ask Codex to do anything` prompt. Treating that footer as draft text prevented
idle-boundary delivery. The parser now recognizes this bounded layout only
beside that dim empty prompt, with a single final path-bearing footer row.
`codex_plain_footer_recognized` announces recognition once per process. Busy
indicators still block delivery, and real drafts remain typed input. The fixture
uses the observed footer layout and busy/draft negative controls; actual
unattended executor delivery remains part of the parent's acceptance run.
The bootstrap assignment was manually delivered with its lifecycle disabled;
that intervention is not evidence of unattended success.

AAB-2 checks and exact commands are recorded in the private
`logs/aab2-verification-handoff.json`. `e2e/project-intake-retry.mjs` runs actual
Chromium against shipped UI with fixture responses, checking original-receipt
routing, key reuse after a lost response, one grant, and preserved pause.
It makes no provider calls. Parent build/restart and retry of the preserved live
receipt are separate acceptance steps; this worker does neither.
