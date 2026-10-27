# Review rubric for gated changes

Every independent review and gate verdict in the fleet loads this file and checks
each item. A change that fails an item fails the review. Owner rule, Ethan
2026-10-27 08:53: "can we ensure the code that's being written is profiled, has
metrics, instruments for inspection granular, are kiss etc best practice".

Load the copy everyone runs, never a worktree copy:

```bash
git -C ~/Dev/amux show origin/main:docs/review-rubric.md
```

A reviewer quotes each item below with PASS or FAIL and the file:line or
measurement that decides it. "n/a" needs a reason.

## 1. Metrics

Every new or changed path on a hot or async route (boot, export, scroll, write,
gate, import, retry, wake) emits named counters and timings for each stage.
Mixpeek uses Prometheus. amux uses log fields plus an /api diagnostic. Together
they answer "which stage, how long, how many" with no debugger attached.

A failure path has its own counter, and a WARN line that carries the operand
(the id, the seq, the namespace, the value that tripped it).

Fail if: a new path has no counter; a stage has no timing; a failure only
raises or returns, with nothing counted.

## 2. Profiling

Any change justified by speed carries a before and after measurement on the
real shape of the workload. State the population (how many runs, which data,
which machine) beside the number. A microbenchmark is labelled as one, and is
never quoted as a served gain.

Fail if: a performance claim has no measured number, no population, or was
measured on a shape the production path does not take.

## 3. KISS

The smallest change that fixes the root cause:
- one retry layer per call path, with the total attempts and time stated
  across every layer that can repeat the call;
- one owner per mechanism;
- no feature flag without a removal date or condition;
- no new script where a parameter on an existing one works;
- no public contract change inside a fix unless it is the fix.

Fail if: the change adds a layer the fix does not need. Specimen (2026-10-27):
a retry nested inside another retry, inside an edge that retried again, gave
24 to 148 attempts on one write.

## 4. Inspection

Each mechanism exposes its state read-only (a status command or endpoint).
Every reading publishes whether it was measured and how many items it
considered (`measured`, `n_considered`), so an empty reading can be told
apart from a probe that never ran.

Fail if: a check can report green, empty or zero without proving it consumed
its input. Specimen (2026-10-27): a fast check went green with an empty query
corpus because every check sat inside `if queries`.

## 5. Proof standard (carried from the gates already in use)

- A fail-before / pass-after test for each fix, run on the exact bytes being
  submitted.
- One mutation per rule, with the suite naming the rule it broke.
- Failure-injection controls for every external read: a failed read, an empty
  read and a short read must not pass.
- State what was NOT tested.
