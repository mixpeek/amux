# Resource controls validation — 2026-09-12

The resource regression tests pass. The full server suite remains partially
blocked by host-dependent tests; this is not a claim that the complete product
lifecycle is green. Policy and remaining storage ownership gaps are described in
[resource budgets](resource-budgets.md).

| Command / proof | Result |
| --- | --- |
| `python3 scripts/test-cargo-budget.py` | 12 passed |
| `python3 scripts/test-cargo-target-guard.py` | 10 passed |
| `bash scripts/test-build-disk-clear.sh` | 25 passed |
| `bash scripts/test-build-atomic-install.sh` | 5 passed |
| `bash scripts/test-build-activation-authority.sh` | 39 passed |
| `bash scripts/test-build-activation-launcher.sh` | 8 passed |
| `bash scripts/test-test-receipt.sh` | 26 passed |
| `bash scripts/test-unbuilt-commits.sh` | 9 passed |
| `make -n run dev check test`; `bash -n install.sh scripts/unbuilt-commits.sh` | Passed; build entry points resolve to guarded commands (installation itself was not rerun) |
| Lifecycle runner contract tests | 9 passed; catalog has 86 unique cases with existing source references |
| `scripts/safe-cargo.sh check --workspace` | Passed; 84.89 seconds, 2.21 GiB peak process-group RSS |
| `scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings` | Passed; 41.43 seconds, 4.56 GiB peak RSS |
| `scripts/test-contended.sh -p amux-server --no-fail-fast` | 60 targets: **2,627 passed, 8 failed, 33 ignored**; 679.56 seconds, 6.88 GiB peak RSS |
| `scripts/test-contended.sh -p amux-server --lib cargo_target_guard::tests:: -- --nocapture` after restoration | 2 passed |

The eight full-suite failures were seven host-admission failures (health, five
worker lifecycle tests, and replay round trip), plus
`action_schema_validates_before_browser_state`. That browser test assumes there
is no browser: its valid file request reached a live browser and returned the
downstream “no element matches” 400 instead. The live host's admission gate still
reported pressure `warn` and about 38 GiB of existing swap. No admission threshold
was raised to make these tests pass.

## Negative controls

All mutations used `scripts/mutate.sh run` and were restored before the final
passing run:

- Disable the RSS comparison: the memory-heavy child test fails because the
  supervisor reaches timeout instead of reporting the memory limit.
- Restore the old 10 GiB debug cleanup threshold: the cache test fails because
  a normal 20 GiB workspace cache is selected for clearing.
- Restore unguarded `remove_dir_all` in hourly stale-target cleanup: the live
  lease test fails with `(1, 15)` reclaimed instead of `(0, 0)`. Restoring the
  guard makes both Rust guard tests pass.

## Live observations

- `/api/debug/storage`: measured true, eight table policies considered; hourly
  storage retention and browser expiry jobs were active.
- Full-test compilation produced a 6.4–6.5 GiB debug cache. The earlier lifecycle
  session had observed a 36 GiB accumulated cache; these are different cache
  histories, not a controlled percentage-reduction benchmark.
- The completed lifecycle lab's leftover server on port 19983 was stopped after
  checking its executable identity. The production server was not stopped.
- Saved messages, uploads, board evidence, profile logins, and backup history
  were not deleted. Remaining unbounded saved-data areas are explicitly listed
  in the policy document.

## Deployed implementation

The signed release from `0b812450b95536369668c6c00d4e045994709db4` was installed
and observed through `/api/health`: commit `0b812450b955`, build
`6ca4e9114df95dab`, status/store `ok`. The server adopted it with the same PID.
The release completed in 233.29 seconds at 4.17 GiB sampled peak RSS, within the
new budget. No debug-cache purge occurred for this deployment.

While compiling, health responded in 17 ms and `GET /api/sessions` returned all
135 workers with HTTP 200 in 3.510 seconds. This is one successful observation,
not a long-duration availability test or a claim that worker-list latency is
fixed. The post-adoption storage sweep was measured (eight table policies),
retained five referenced uploads, and removed 29 aged rotated log files under
the existing retention policy.

The final Makefile adjustment preserves `BIN_DIR` when handing installation to
the atomic builder. It and this report do not change the compiled Rust source.
