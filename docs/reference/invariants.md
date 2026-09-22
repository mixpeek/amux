# Invariants

An invariant in amux is a continuous check that two subsystems agree with each other. The system detects seams where components look healthy individually but contradict one another (e.g., a steering queue accepting writes with no consumer, or identical routes answering 200 vs 405 on different spellings).

## Definition and Binding Rules

All invariant probes enforce three rules (`crates/amux-server/src/invariants/mod.rs:22-39`):

1. **`Unknown` is not `Pass`** — A probe that cannot reach a verdict reports `Status::Unknown`, never a passing result. A failed observation must never render as healthy empty state.

2. **A check that cannot fail is not a check** — Every invariant ships with a negative-control test (`cargo test invariant_negative_control`) that injects the failure and asserts detection.

3. **Evaluations are recorded** — Every run writes to `_amux_invariant_result` so "this check stopped running" is visible; a silent monitor is worse than no monitor.

## Verdict Types

The `Status` enum (`mod.rs:54-61`) represents a single evaluation:
- **`Pass`** — expected and observed agree
- **`Fail`** — contradiction detected
- **`Unknown`** — probe could not reach a verdict
- **`Skipped`** — check was not run

Only `Status::Fail` opens an incident; `Unknown` does not page because an unreachable probe is a gap in observation, not proof of corruption.

## Key Structures and Functions

**`InvariantResult`** (`mod.rs:86-98`): one evaluation's outcome with `invariant_id`, `status`, `entity_key`, `expected` (mandatory), `observed` (mandatory), and `evidence` (JSON).

**Verdict constructors** (`mod.rs:101-136`):
- `InvariantResult::pass(id)` — passing check
- `InvariantResult::fail(id, expected, observed)` — failure naming both sides
- `InvariantResult::unknown(id, why)` — probe could not reach verdict

**Health rollup** (`mod.rs:274-290`): combines multiple results into one `Confidence`:
- Empty result → `Unknown`
- One `Fail` in 50 passes → `Unhealthy` (failures not diluted)
- Confidence levels: `Healthy` > `Degraded` > `Unknown` > `Unhealthy`

**API endpoints** (`crates/amux-server/src/api/invariants_api.rs`):
- `GET /api/health/invariants` — rollup + monitor liveness (stored verdicts, ~16ms)
- `GET /api/debug/invariants` — live incidents + latest per invariant
- `?live=1` forces synchronous re-evaluation (slower, ~2.5-3.9s)
- `?id=<name>` filters to one invariant with `ran` field to distinguish "never ran" from "passed"
