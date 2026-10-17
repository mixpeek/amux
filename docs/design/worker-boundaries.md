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
| boundaries-self | none | editing the installed registry or guard, removing hooks from Claude settings |

**What this cannot do yet.** Workers run as the owner's macOS user. The guard
matches command text and file paths, so a worker that writes a script to disk
and runs it later is not seen. The server gates trust request headers on
localhost (`api/email.rs` says so of `X-Amux-Approver`). These are tripwires
with a recorded reason, not walls. The per-worker VM below is what makes them
walls: a worker whose credentials, network egress and filesystem are those of
its own VM cannot reach the owner's tokens at all.

**Isolated workers** are skipped by the guard, as CLAUDE.md promises them no
behavioral hooks. The server gates still apply to them. `/api/boundaries`
lists them under `not_covered_by_hook`. Whether a boundary should override
isolation is the owner's call.

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
