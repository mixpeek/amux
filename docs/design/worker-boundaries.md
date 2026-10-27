# Worker boundaries and resource awareness

Owner ask, 2026-10-16: amux workers should hold to a small set of robust
boundaries. The boundaries should be consolidated in one place that is easy
to read, change and iterate on. Two examples: every payment is authorized, and
outbound email goes through the amux API. A new boundary has every worker
understand what it uses on the machine, request only what it needs, and keep
the whole machine's capacity in mind. Later, each worker should be able to
define, start and stop its own VM sized to its work.

## The registry

`boundaries.toml` at the repo root is the only place a boundary is written.
Each `[[boundary]]` has:

- `id`, `title`
- `rule`: the text every non-isolated worker reads at launch. It is composed
  into the worker's binding context (`compose_worker_context`), ahead of the
  scoped rules.
- `enforced_by`: which mechanisms back it, so a reader can tell a hook from a
  server gate.
- `deny`: tool calls the guard refuses. Bash rules use `match_all` and
  `match_none`; file-tool rules use `path` and `content_any`. Every rule
  carries `block_examples` and `allow_examples`.

`scripts/hooks/test-boundary-guard.py` runs every example through the real
guard, so the examples are the test suite. To add or tighten a rule, add its
example first and watch it fail.

On one machine, `~/.amux/boundaries.toml` overrides by `id`: its fields replace
the committed ones, `enabled = false` turns a boundary off, and a new `id` adds
one. The `boundaries-self` boundary stops workers editing that file or the
installed guard.

Where it is visible:

- `amux boundaries` / `GET /api/boundaries`: the merged registry, whether the
  guard is installed and wired into Claude settings, whether the installed
  registry matches this build, and per-boundary counts from the last 24h of
  `~/.amux/logs/boundaries.jsonl`.
- The guard's refusal message names the boundary, the reason, the
  sanctioned route and the registry.

## Enforcement layers

| Boundary | Server gate (holds on the API path) | Guard (stops the direct routes) |
|---|---|---|
| payments | vault approval limit and single-use grants (`api/vault.rs`) | payment provider APIs, vault files, approving grants or editing vault rules |
| email | external-send approval hold, send audit (`api/email_approval.rs`, `/api/email/log`) | Mail.app, sendmail and SMTP, provider APIs (Gmail, SendGrid, Mailgun, ...), stored Gmail and auth tokens, approving or editing held sends |
| resources | `host_guard` holds automated dispatch at critical; admission refuses new workers under memory pressure | heavy commands refused at critical pressure, with an acknowledged override |
| subagents | none | a new Agent/Task call while the worker already runs its subagent limit (a `[P1]` call may take one slot over it), or while the account's usage headroom is under the floor |
| orchestrator-modal | none | AskUserQuestion, EnterPlanMode and ExitPlanMode for a worker marked as an orchestrator |
| tenant-workload-gate | none | kubectl/helm mutations and named load tools in a tenant namespace without a fresh gate receipt from this worker; hand-written receipts |
| boundaries-self | none | editing the installed registry or guard, removing hooks from Claude settings |

**What this cannot do yet.** Workers run as the owner's macOS user. The guard
matches command text and file paths, so a worker that writes a script to disk
and runs it later is not seen. The server gates trust request headers on
localhost (`api/email.rs` says so of `X-Amux-Approver`). These are tripwires
with a recorded reason, not walls. The per-worker VM below is what makes them
walls: a worker whose credentials, network egress and filesystem are those of
its own VM cannot reach the owner's tokens at all.

**Isolated workers** are covered by the guard while the registry's top-level
`apply_to_isolated` is true (the default, named as an exception in CLAUDE.md
"Isolated workers"). A refusal injects no prompt and routes nothing. An
isolated launch unsets `AMUX_SESSION` and exports `AMUX_ISOLATED_WORKER`, and
the guard reads that name; before 2026-10-18 it exited on the missing
`AMUX_SESSION`, so no boundary reached a real isolated worker even with the
switch on. With `apply_to_isolated = false` they are exempt and
`/api/boundaries` lists them under `not_covered_by_hook`. The server gates
apply either way.

## Subagents

The mxp-gs12 review (2026-10-18) found usage limits had stopped every subagent
of one large isolated worker five times in four days: it ran up to 21 Claude
Code agents on one shared plan, and nothing compared the fan-out with the
account's remaining capacity. The `subagents` boundary refuses a new `Agent`
or `Task` tool call when either holds:

- the worker already has `max_running` subagents running. The limit is
  `AMUX_MAX_SUBAGENTS` in the worker's env (Scope tab), else
  `[boundary.subagents].max_running_default` (4). `0` turns the count cap off.
- the account's usage headroom is under `min_headroom_pct` (10). Headroom is
  100 minus the highest account-wide window percent in `GET /api/usage` (the
  5-hour session and weekly windows; model-scoped windows are ignored). `0`
  turns the floor off.

The refusal tells the model the running count, the limit, and when to retry
(when one of its agents finishes, or the window's reset time). It is a refusal
only: no prompt, no routing.

**Priority and age (2026-10-27, MSG-98201).** mxp-gs12 was refused 11 times
on 10-27, including for its most important task. With the cap full, an Agent
call whose description or prompt start matches a `priority_markers` regex
(`[P1]` or `#1` at the start) still starts, up to `over_cap_max` (1) above the
cap; it is logged as `subagent_cap_preempt_allowed` with the running count.
Past that bound a marked call is refused with `cause` `over_cap_bound`;
`over_cap_max = 0` turns the allowance off. The headroom floor still applies
to a marked call. A PreToolUse hook cannot stop another agent: it can only
allow or refuse the call in front of it, and subagents run inside the Claude
process with no handle the hook could reach. So the refusal lists every
running subagent with its description (from its meta file), how long it has
run (the first transcript entry's timestamp, else the file's birth time) and
how long since it last wrote, and tells the lane to stop its lowest-priority
or stale one with TaskStop. Each log line carries `running_agents` and
`oldest_age_s`, and an Agent call while any subagent is older than
`long_running_s` (7200) also logs `subagent_long_running` (WARN), so a sweep
finds agents running for hours. The guard sees subagents only when the lane
makes an Agent call; it has no view of them between calls.

The count comes from Claude Code's own files, so it needs no amux reporting
(isolated workers send none). The hook payload's `transcript_path` names the
conversation; each subagent has `<conversation>/subagents/agent-<id>.jsonl`
and `agent-<id>.meta.json`, and the meta gains `"finished": true` when the
agent stops. A subagent counts as running while its meta has no finished flag
and its transcript was written within `stale_s` (1800, since a killed agent
never gets the flag). A transcript with no meta counts only within
`no_meta_warm_s` (240). A subagent's own Agent call counts its siblings.

Unmeasured means allowed: no `transcript_path`, a transcript that does not
exist, or an unreachable or unmeasured `/api/usage`. Verdicts in
`boundaries.jsonl`: `subagent_cap_refused` (`level` WARN, `cause`
`running_cap` or `usage_headroom`, with `running`, `limit`, `headroom_pct`) and
`subagent_cap_allowed_unmeasured` (`level` INFO, `measured` false when the
count was the unmeasured half, `unmeasured` naming which). The guard is wired
for `Agent` and `Task` by `install-claude-status-hooks.py`; a box installed
before this change needs `install.sh` rerun for the matcher to include them.

## Orchestrator lanes and modal prompts

On 2026-10-24/25 the orchestrator `amux-helper` called `AskUserQuestion` while
the owner slept. The tool blocks the whole turn until a human answers, so for
about 8 h its peer messages queued unread and its wakeup chain could not fire
(`scripts/worker-stall-watch.sh` detects that stall after the fact). The
`orchestrator-modal` boundary (`kind = "modal"`) prevents it: for a worker
marked as an orchestrator, the tools in `[boundary.modal].tools`
(`AskUserQuestion`, `EnterPlanMode`, `ExitPlanMode`) are refused. The refusal
restates an `AskUserQuestion`'s questions as numbered options and tells the
worker to put them in its turn output, decide inside its standing authority,
say which option it took, and keep working.

A worker is an orchestrator when its scoped `AMUX_ORCHESTRATOR` is on (read
live from `sessions/<worker>.env`, then the process env), or, when that is
unset, when its name is in `[boundary.modal].orchestrators` (default
`amux-helper`, `mxp-gs12`, `tubescience-parity`). `AMUX_ORCHESTRATOR=0`
releases a listed worker. Isolated workers are covered under
`apply_to_isolated`. Verdict: `modal_prompt_refused` (WARN, with `tool` and
`orchestrator_by`). The guard sees these tools only once
`install-claude-status-hooks.py` has rewired its matcher, so a box installed
before this change needs `install.sh` rerun.

## Tenant workloads

Owner ask, 2026-10-27: before any lane scales, patches, restarts or load-tests
a live tenant workload, it reads what scales it and records a gate. That day a
worker scaled `deploy/api` in `ephemeral-r40255478404` to zero and paused its
ScaledObject without reading the namespace; a second ScaledObject,
`mvs-primary-scale-to-zero`, deactivates the mvs-primary shard when no
`app=api` pods exist, so the shard died mid index rebuild. The same day a
teardown deleted a booting pod, a mass `rollout restart` OOMed a node, and a
replay burst wedged a shard.

`scripts/tenant-gate.sh <context> <namespace> "<why>"` (installed as
`~/.amux/hooks/tenant-gate.sh`) is read-only: it gets the namespace's
ScaledObjects, HPAs, PDBs, Deployments and StatefulSets, prints what scales
what (including `kubernetes-workload` triggers whose `podSelector` watches
another workload, and paused annotations), and writes a receipt to
`$AMUX_HOME/tenant-gates/<worker>/<context>__<namespace>.json` with the
reason and a sha256 of the raw read. A failed or empty read writes none.
Context `default` means kubectl's current context, and is what the guard uses
for a command that names no `--context`.

The `tenant-workload-gate` boundary's deny rule carries `requires_receipt`.
Its patterns are tested per command segment (split on shell operators, and
inside `bash -c`, `sh -c`, `eval` and `ssh` strings). A segment that mutates
(`kubectl` scale, patch, delete, apply, replace, edit, annotate, label, drain,
cordon, taint, create, `rollout restart|undo|pause|resume`,
`set env|image|resources`; `helm` upgrade, rollback, uninstall, install; a
named load tool such as `hey`, `k6 run`, `vegeta attack`, or a `*replay*` or
`*load-test*` command) in a namespace matching `namespaces` (`mixpeek-*`,
`ephemeral-*`) is refused unless this worker holds a receipt for that exact
context and namespace younger than `max_age_s` (1800;
`AMUX_TENANT_GATE_MAX_AGE_S` overrides). The refusal names the gate command
with the parsed context and namespace. A mutation over `-A` is refused
outright, since no one receipt covers it. This rule fails closed; the guard
stays fail-open for unreadable payloads and registries. Verdicts in
`boundaries.jsonl`, counted by `/api/boundaries` under the boundary:
`tenant_gate_refused` (WARN, with each target's `cause`) and
`tenant_gate_allowed_with_receipt` (INFO, with the receipt's age and sha256).

What it cannot see: a command whose namespace comes from the kubeconfig
(`kubectl config set-context --current --namespace=...`) or from a manifest
(`kubectl apply -f x.yaml` with no `-n`); a script on disk that runs kubectl
or sends load (only its own command line is read, so a `replay` script that
names its namespace with `--namespace` or a `<svc>.<ns>.svc` host is caught,
and one that reads it from config is not); load sent through a
`port-forward` to localhost; and `kubectl exec` into a pod. The receipt proves
the read happened, not that the worker understood it.

## Resources

What a worker can see now:

- `amux resources` (`GET /api/resources?worker=<name>`) shows the machine's
  cores, RAM, swap and disk, the load and pressure level, the heaviest workers,
  and this worker's own CPU, memory and process count, including its share of
  all worker memory.
- A **resource profile** per worker: `AMUX_WORKER_CPUS`, `AMUX_WORKER_MEM_GB`
  and `AMUX_WORKER_DISK_GB`, set in the Scope tab and resolved worker, then
  group, then global. `amux resources` reports usage against it and names any
  limit the worker is over.
- **Disk is enforced** (2026-10-18, after one worker's subagent cargo targets
  filled the data volume twice). The boundary guard measures the worker's
  scratch (`~/.amux/tmp/<worker>` plus `AMUX_WORKER_DISK_PATHS`) with `du` in a
  detached process, caches it in `~/.amux/state/disk-usage/<worker>.json`, and
  checks heavy commands against `AMUX_WORKER_DISK_GB` (registry default
  `disk_budget_gb_default`, 100): a warning past 80%, a refusal past 100%,
  `AMUX_DISK_ACK="<why>"` to override (recorded). `amux resources` shows the
  same reading under `worker.disk`. Verdicts in `boundaries.jsonl`:
  `disk_budget_warn`, `disk_budget_refused`, `disk_budget_acknowledged`,
  `disk_budget_unmeasured`.
- The `resources` rule text tells every worker to check before heavy work,
  lower parallelism when the machine is strained, prefer remote hardware for
  big builds, and shut down what it started.

What is enforced: the guard samples load per core, swap used and macOS memory
pressure directly, with no server call. When the machine is critical it
refuses commands matching `[boundary.resources].heavy`. A worker that cannot
wait reruns with `AMUX_RESOURCES_ACK="<why>"`, and the reason is logged as
`resources_acknowledged`. Strained runs are allowed and logged as
`resources_strained_allowed`, so the log shows who builds into a loaded
machine.

Fixed alongside: `host_guard` called `sysctl` by bare name, and the server's
launchd PATH has no `/usr/sbin`. So on this Mac it had never read load or
memory pressure, and `/api/metrics/host/pressure` reported `ok` with null
readings while the host sat at load 20 with swap 88% used. It now resolves
absolute paths and reports a partial reading in `why_unmeasured`, with a
`host_guard_probe_missing` warning.

## Toward a VM per worker

The profile keys are the contract a VM layer reads. The intended lifecycle,
extending what already exists:

1. **Define.** The worker's profile (`AMUX_WORKER_CPUS`, `_MEM_GB`,
   `_DISK_GB`, plus an image key) is its VM spec. It is declared in the same
   scope files as today, so nothing about it is new to the owner.
2. **Admit.** Before starting, compare the sum of running profiles plus this
   one against the machine's capacity (`/api/resources` already has both
   sides) and queue rather than overcommit. This is `host_guard::admit_*`
   with a capacity ledger instead of a load sample.
3. **Start.** `AMUX_WORKSPACE_ISOLATION` already gives each worker its own
   worktree and TMPDIR; the VM mounts that worktree. `integrations/computer.rs`
   already runs one CUA container per worker with memory and CPU limits, a
   fleet cap (`AMUX_COMPUTER_MAX`) and an idle reaper. That is the shape to
   generalize: one sandbox per worker, sized from its profile.
4. **Work.** Inside the VM the boundaries become structural. Credentials are
   injected per worker (`api/vault_secrets.rs` already scopes secrets per
   worker), egress can be limited to `$AMUX_URL`, and the owner's home
   directory is not mounted.
5. **Close.** The reaper stops the VM when the worker's turn ends or it goes
   idle, and the worker's measured peak is recorded against its declared
   profile so the next request can be sized from history.

Until then, the profile is declarative. It is shown and compared, never
enforced as a hard limit, because a limit on a process tree that shares the
owner's user would be a second guess at what a VM does properly.
