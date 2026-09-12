# Helper failure validation — September 12, 2026

Continuation of the consolidated lifecycle, focused on LW-02 / LC-HELPER-FAILURE.
The live server at `fb7d746c` was healthy, but host admission denied new workers
with memory pressure warn and approximately 42 GB swap. No fresh Sonnet, Gemini,
or isolated worker was started; their consumption, backlog drain and real
semantic merge/recovery scenarios remain unverified.

## Reproduced failure

The real subprocess fixture printed `session limit reached` and exited 7. The
previous helper implementation returned it as `Ok`, allowing the classifier to
misreport a provider failure as invalid JSON. Failed JSON-shaped stdout was also
accepted, which could authorize a semantic decision from a failed process.

```bash
AMUX_SESSION=codex-server-sync CARGO_BUILD_JOBS=2 \
bash scripts/test-contended.sh -p amux-server --lib helper_failure
# Pre-fix: 1 passed, 2 failed; real nonzero stdout incorrectly returned Ok.
```

## Change

Both CLI model clients now require successful exit status before accepting an
answer. Failed status carries at most 400 diagnostic characters, preferring
stderr and falling back to stdout. Empty failures retain the status and say
there was no output. Successful JSON remains usable even with a stderr warning.
Timeout cleanup kills and reaps the direct child. Distinct structured verdicts
identify spawn failure, wait failure, failed exit, empty successful output, and
timeout, including measured/n_considered and exit/byte counts where applicable.
Raw prompts and model output are not included in these diagnostic log events.

The process tests use real local shell children and no live model or globally
modified provider environment. They cover stdout quota, stderr-only failure,
empty failure, failed JSON-shaped output, bounded diagnostics, successful JSON,
empty success, missing executable, timeout classification, and actual child
reaping. The existing MDAI engine suite remains part of validation.

## Scope

This fixes failure attribution and rejects invalid semantic authority; it does
not restore provider quota or make unavailable comparisons succeed. Semantic
intake still preserves requests separately when comparison is unavailable.
The six-message, three-task live append/update journey remains in LW-03. The
existing helper I/O path still writes stdin synchronously and drains output
after exit; heavy pipe traffic and inherited pipe handles need a separate
bounded-I/O check before claiming its timeout covers the entire subprocess I/O.

## Results

- `bash scripts/test-contended.sh -p amux-server --lib api::mdai::tests`: **25 passed**, 0 failed.
- `bash scripts/test-contended.sh -p amux-server --lib api::board_intake::tests`: **2 passed**, 0 failed.
- `bash scripts/test-contended.sh -p amux-server --test board_api`: **89 passed**, 0 failed.
- `bash scripts/safe-cargo.sh check --workspace`: **PASS**.
- `bash scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings`: **PASS**.

Commands ran with `AMUX_SESSION=codex-server-sync CARGO_BUILD_JOBS=2` and the shared
Cargo target. These are focused scopes, not a full-server or real-provider pass.
The helper regression includes five test functions; the successful core MDAI
and board API controls are counted separately above.

```bash
bash scripts/mutate.sh run crates/amux-server/src/api/mdai.rs \
  'if !out.status.success() {' 'if false {' -- \
  bash scripts/test-contended.sh -p amux-server --lib helper_failure
# Mutation LANDED; 3 passed, 2 failed; exit 101; inverse mutation LANDED.
# Fixed baseline subsequently passed all 25 MDAI tests, including all 5 helper tests.
```

Raw before/final/mutation logs and the timestamped native prerequisite receipt
are retained in the local `helper-failure-20260912` evidence directory. The
negative control demonstrates that returning failed stdout as an answer makes
the real subprocess tests fail.


## Repeated Cargo builds

The continuation exposed a second defect relevant to the resource lifecycle:
`build.rs` watched `.git/HEAD` and `.git/refs/heads/main` as if `.git` were always
a directory. In the detached worktree used for safe verification it is a file,
so both paths were permanently missing and Cargo reran the server build for
unchanged source. Two consecutive focused checks spent approximately 75 seconds
each rebuilding the same server crate.

The build script now asks Git for its actual HEAD/current-branch metadata paths.
Packed refs watch an existing parent until a loose ref is created; a detached
worktree does not watch unrelated branch changes. Exported source without Git
retains an explicit unknown identity diagnostic. Existing crate/source watches
remain in place.

`scripts/test-cargo-worktree-provenance.py` copies the shipped build script into
a tiny real crate, uses a real linked worktree and the shared Cargo target, and
checks eight Cargo builds. Repeated builds must be fresh. Empty commits, branch
switches and packed-to-loose updates must produce the correct new identity.
The fixture creates no separate Cargo target and removes its own temporary
repository. It is included in LC-CARGO-RESOURCE-BOUNDS and the full runner.

Before the fix, the unchanged second build failed the freshness assertion.
The repaired fixture passed. Restoring the broken HEAD path is a separate
negative control.


Final Cargo verification:

- `python3 scripts/test-cargo-worktree-provenance.py`: **1 passed**, eight real Cargo builds covering fresh repeats and identity changes.
- `python3 -m unittest discover -s scripts/lifecycle -p 'test_*.py'`: **9 passed**.
- `bash scripts/safe-cargo.sh check --workspace`: **PASS**, followed immediately by a second **PASS** with no compilation lines: Cargo finished in **0.55 seconds**, **1.46 seconds** including the wrapper process.
- `bash scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings`: **PASS**.

```bash
bash scripts/mutate.sh run crates/amux-server/build.rs \
  'root.join(head).display()' 'root.join(".git/HEAD").display()' -- \
  python3 scripts/test-cargo-worktree-provenance.py
# Mutation LANDED; unchanged-worktree assertion FAILED; inverse LANDED.
# Fixed fixture subsequently passed.
```

[Portable gate counts, source hashes and native prerequisite receipt](evidence/lifecycle-helper-cargo-2026-09-12.json).
The cache timing measures Cargo reuse, not native-model token savings. Both changes
retain the host's worker admission guard and the configured Cargo resource bounds.
