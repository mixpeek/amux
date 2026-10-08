# Autonomous harness recovery

Recovery is part of every durable harness operation. The server being alive,
retaining a database row, and recovering unfinished work are separate claims.
A crash test must kill a real process without a shutdown flush, restart the
same binary against the same state, and exercise the recovered operation.

## State and side effects

Use the existing board, workers, schedulers, filesystem, groups, memories,
environment and messages. Persist intent before claiming work. A claim and
its state transition share the store transaction. Before applying a plan made
on a read connection, recheck assignment, status, readiness and dependencies.
Persist the actual result and its evidence independently of disposable workers
and checkouts. A missing result is unmeasured, never success.

Recovery distinguishes three states:

| State at failure | Recovery | Required negative control |
| --- | --- | --- |
| Intent persisted, delivery never attempted | Resume the existing intent | No duplicate card, message or schedule |
| Delivered/completed with durable receipt | Adopt the receipt or surviving process | No second provider call or external effect |
| Claim exists, receipt absent | Reconcile against authoritative transport/process evidence; otherwise retain an explicit interrupted result | Never relabel uncertainty as success or blindly replay arbitrary shell/outbound work |

A provider limit is a definite non-delivery. A capacity-refused one-off is
retained with a 15-minute retry and an audit entry. Owner edits and deletion
fence that recovery by schedule version and content. Queued delivery, an
archived target, or an unknown transport result does not activate this retry.
Recurring model prompts coalesce missed occurrences to the latest one;
explicit shell catch-up retains its policy and records omitted occurrences.

Periodic consumers catch construction and poll panics. Long-lived consumers
receive a factory so a panicked future can be recreated. Backoff is capped at
five minutes; explicit aborts, isolation and deliberate normal termination
stay stopped. `GET /api/system-jobs` and health issues expose failures,
consecutive failures, recoveries and the last failure time. Review, pre-review,
verification and land tasks release their live-task claims even after a panic.
Their next pass re-reads durable state before acting.

## Orchestration ownership

A native project's `project_group` remains its durable board owner. Legacy
workers that explicitly enable the contract and name the same configured hub
share an orchestration owner when the hub itself opts in and names itself.
Executor reassignment then preserves dependency edges. The idle pool can take
a ready prerequisite without moving a 300-card connected component or its
roll-ups. Proof priority follows transitive prerequisites, counts each proof
once per prerequisite, and terminates on cycles.

Unconfigured workers, other hubs, unassigned boards and isolated workers keep
their existing boundaries. Isolation remains direct owner transport plus
passive lifecycle observation and its documented owner-configured exceptions.
A shared board does not authorize peer steering, spending or outbound actions.

## Durable scheduler scripts

Prefer a shell schedule for deterministic work. For a **self-contained** script,
creation or PATCH can bind the script's exact bytes to durable filesystem storage:

```json
{
  "title": "Measure readiness",
  "kind": "shell",
  "script_path": "/tmp/readiness.py",
  "command": "python3 {script} --json",
  "schedule_expr": "every 1h"
}
```

The one unquoted `{script}` token is replaced with a safely quoted path under
`$AMUX_HOME/schedule-artifacts/<sha256>.<extension>`. The script is limited to
1 MiB. Missing scripts, ambiguous placeholders, prompt schedules and existing
artifact digest mismatches are refused before schedule mutation. Editing the
scratch source does not change the bound revision; bind a new revision explicitly.
This snapshots one script, not its interpreter, imports, data or credentials.
Use durable paths for those inputs. Shell pipelines enable `pipefail`, and full
output streams to the existing shell-run log while in-memory tails stay bounded.

## Proof and token efficiency

An unfinished or failed frozen check retains its command, acceptance, commit
and failure output during recovery. Age does not make it pass. Recovery reopens
it through the normal review-failure transition before spending a model review.
An unavailable evidence commit cannot silently become a different checkout.

Review recovery binds the frozen contract, source, evidence and round. A result
at the same commit is not reusable after its obligations changed. The full card
source is available as `card-source.md` to the read-only reviewer; an excerpt
must not stand in for all measurement requirements. Review inputs and complete
outputs survive temporary checkout cleanup under `$AMUX_HOME/review-evidence`.
A nonzero, missing or timeout exit cannot grant a verdict from partial stdout.
Completed attempts without a trustworthy verdict spend a bounded review round;
pre-launch failures stay unmeasured without spending a model round.

Fresh-session briefs preserve explicit acceptance criteria and next action even
before a contract freezes, with a pointer to the full card. Repeated nudges are
compared after whitespace normalization; card IDs, revisions and measured values
remain significant. Card spend budgets charge the task-attributed ledger rows,
not unrelated or ambient turns. Failed accounting holds the automated nudge
instead of silently charging zero. Token price estimates remain estimates.

For pace, use the existing versioned done-line API rather than reconstructing
scope from assignments or titles:

```bash
scripts/orch-pace.py --orchestrator hub --lane-prefix worker- \
  --deadline 2026-10-09T23:59:00-04:00 --epic EPIC-ID --json
```

`--epic` measures the exact frozen IDs across executor moves and title changes.
Missing, discarded, deleted or archived members produce `UNMEASURED`, not a
smaller target. The legacy title-based mode counts all orchestration executors.
Neither mode creates a scope freeze or substitutes for owner-approved scope.

## Development and verification

Run `scripts/test-harness-recovery.sh` for a privately linked server binary,
real HTTPS/SIGKILL recovery tests, scheduler/script/identity endpoint scenarios,
and pace-scope regressions. It shares the sanctioned Cargo dependency cache;
the executable under test has its own immutable artifact path and hash. Preserve
its receipts. Test wrappers capture the source before execution; receipts
withhold all byte coverage when the source differs afterwards or the initial
snapshot is unavailable. This detects observed source drift, not every possible
transient edit restored during a run. Then run the full server and workspace
suites, check and clippy. Package cleanup takes an exclusive target lease;
active build/test consumers make it defer rather than delete their executables.
A held/failed cache refresh preserves the previous source fingerprint and records
that no test ran. Retry after those consumers finish.

| Surface | Recovery evidence |
| --- | --- |
| Board, worker configuration, schedules, messages, steering, journal, history, request log | Existing real-process restart persistence matrix |
| Provider conversations, leases, media-job metadata | Explicitly labelled seeded persistence cases in that matrix |
| Interrupted manual shell and cron delivery | Real restart: error/unknown result retained, no false pass or automatic replay |
| Native lifecycle spool and installed observer | Hooks run during downtime; boot consumes edges once and repairs installed bytes |
| Pending independent review | Cached results and a real detached reviewer surviving SIGKILL are adopted without a second CLI launch; full evidence retained |
| Reviewer provider capacity | A nonzero quota exit creates a durable capacity wait without reopening work or spending a quality round; SIGKILL retains the deadline, unknown readings respect backoff, and the normal consumer retries once. A positive fresh model-scoped capacity reading releases the wait earlier. Completion reviews get shared slots before advisory pre-run reviews. |
| Media prepare | Seeded stale intent resumes a real ffmpeg consumer after SIGKILL; completed output is adopted, and lost output is regenerated |
| Lease expiry | Real API counts expired and live RFC3339 leases correctly before and after SIGKILL |
| Idle pool and dependency graph | Real idle-worker claim from an over-cap graph, SIGKILL between claim and dispatch, one pickup command at the fake terminal, and intact roll-up edges; transactional stale-plan, dependency change, archive, busy lane, transitive/cyclic graph and 300-card tests |
| Internal consumers | Injected constructor, poll and long-loop panic; recovery counters and abort/termination controls |
| Budget, scheduler reserve, coalescing, shell pipelines | Attributed/unknown accounting; definite versus uncertain delivery; actual failed shell pipeline |
| Scope and handoff | Exact frozen IDs, bad/missing scope and acceptance-preserving brief controls |

The media restart case seeds the lost heartbeat explicitly; it proves the resumed
ffmpeg consumer, not killing an encoder at every possible point. CI requires ffmpeg
for this case; a missing local binary is reported unmeasured. Fake
providers prove harness transport and state, not real provider flags, model
reasoning, an external plane's correctness, or every possible interleaving.
For work involving those surfaces, add a real consumer test and a failing
control at the actual failure boundary. Human authorization gates remain gates.

Provider-capacity waits retain failed output in `review-evidence`, independently
of the disposable checkout. `review_retry_at` and the blocked model persist in
SQLite. Fresh exhausted windows and the existing human reserve hold the retry;
missing or stale readings allow one new attempt only after fifteen minutes.
They never grant a verdict. The capacity recovery also refunds a positively
identified, quota-only legacy failure once per recorded attempt, retaining its
original log and preserving current task status, criteria, source and check.
Ordinary failed reviews still consume their bounded rounds and escalate normally.
Pre-run quota failures are retriable, rather than permanently caching an
unmeasured plan. A healthy completed review is never repeated after restart.

`e2e/chaos/review-capacity-recovery.mjs` uses the real board API, contract clock,
CLI process, SQLite and SIGKILL. Its retry-clock boundary is explicitly seeded;
the Rust integration tests cover fresh headroom and reserve decisions. The
private fake-provider PATH also shadows macOS `security`: a private HOME alone
does not isolate the operating system's keychain.

A transcript-confirmed retryable foreground API failure is a parent turn boundary
when its normal composer is empty and no foreground generation or selector is
present. Surviving background shells/agents do not prevent the bounded retry.
The existing paste transport retains those children and queues safely if another
turn begins. Authentication errors, quotas, isolation, paused/disabled lanes,
nonempty composers and unknown frames retain their respective gates.
`api-error-background-recovery.mjs` tests the real periodic sweep and terminal,
a real background PID, authentication/isolation controls and durable retry-key
recovery after SIGKILL. Its failed transcript is a fixture
seed; the normal two-minute retry clock elapses, and it makes no provider call.

Maintenance yields the shared writer between tables and deletes at most 256 aged
eligible rows per table per pass. The survivor/unit guard and pending capture/chat
protection remain authoritative. A saturated batch logs that fact; remaining work
is discovered from the database on later passes, including after a crash. Checkpoint
contention defers immediately and restores the normal mutation busy timeout.
Archived project-owner discovery and steering retention use covering indexes, and
slow retention tables/project reconciliation phases announce their measured latency.

The foreground process replay also holds a future quota reset with a real provider child alive, explicitly advances only its private transcript clock to a passed reset, and observes one normal-consumer continuation across SIGKILL. Unclocked limits remain parked. The provider reset grace and empty-composer/selector checks apply before background work can be ignored.

Usage-reset delivery retains the positively observed passed reset through the
steering consumer. Clearing it early would reparse unchanged clock-only chrome as
a future reset. Account replacement alone releases the previous account's future
stamp; leaving quota chrome clears it normally. Ordinary queue identities include
entropy even within one clock tick, and unexpected primary-key collisions fail
with a named persistence error instead of replacing another lane's receipt.

Browser CI uses the official Playwright image pinned to the package-lock version
and OCI digest. Readiness launches both Chromium and WebKit and records their
versions and executable paths; a package mismatch fails. All browser projects,
assertions, retries and runner deadlines remain the same. See
https://playwright.dev/docs/docker for the upstream image and version requirements.

On Apple Silicon, API worker starts/recovery and the Bash start command apply the
same native architecture preference as backend argv launches. This prevents an
Intel tmux server from handing its preference through a native provider into all
its shell tools. Existing processes continue unchanged; their next launch adopts
the preference. AMUX_NATIVE_ARCH=0 opts out and Intel-only executables retain the
fallback. `e2e/chaos/native-launch.mjs` measures actual provider child architecture
through private API/CLI starts and the opt-out. Linux reports this hardware probe
unmeasured rather than claiming to have tested Rosetta.

The architecture fixture uses system universal Python as its provider stand-in,
so it inherits the launch preference like the native-capable real provider. The
host's other Python launcher can force an Intel interpreter; that is a legitimate
fallback rather than evidence that the preference wrapper failed. No actual
Claude/model call is made by this fixture.

Automatic resume stages the existing steering queue under one stable worker/stop
identity before atomically recording its resume key and retry counter. A refusal
leaves no key; a crash before handoff rediscovers the stop, and a crash after
handoff adopts queue/history instead of repeating input. New nudges retain card
budgets; adoption is not another nudge. A positively observed account replacement
releases only the former account's capacity hold while retaining ordinary card
budgets and terminal gates. Staging and durable acceptance have separate named
signals from actual terminal submission.

`auto-resume-handoff-recovery.mjs` locks only its private SQLite writer, enables
a seeded expired retry, and SIGKILLs the controller at decision/handoff before
any provider input. It requires one continuation after restart with a surviving
real child. Another SIGKILL explicitly seeds loss of the producer's metadata
acknowledgement; queue/history adoption must prevent duplicate input. The fixture
clock and acknowledgement loss are declared; real process crashes and terminal
receipts supply the consumer proof.
