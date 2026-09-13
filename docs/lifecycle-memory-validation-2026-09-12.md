# Memory recovery investigation — September 12, 2026

Native provider lifecycle remains **INCOMPLETE**. No admission override was used.
This continuation identifies host consumers, releases owned test resources, and
repairs pressure diagnostics; it does not claim native work reached completion.

## Observed host condition

The initial healthy probe carried commit `4da21196277af581e436d258a9fb9167b19c0bad`,
build `4404d7eab02e945a`, admission `deny`, pressure `warn`, and 43,501.56 MiB
of swap. A subsequent probe after owned-test cleanup was degraded with 46,942.375
MiB swap and the same build. More than 748 GiB disk space remained available.

A macOS `top -l 1 -o mem -n 5 -stats pid,mem,cmprs,command` snapshot showed
approximately 54 GiB MEM for fseventsd, 24 GiB for Procwarden's Python menu-bar
process, and 12 GiB for Activity Monitor. MEM includes compressed accounting;
these figures are rounded and are not a sum of physical resident pages. The
corresponding RSS-only ranking mostly hid the latter two consumers.

Procwarden rebuilds its menu every six seconds. The installed rumps implementation
retains each MenuItem in `NSApp._ns_to_py_and_callback`; `Menu.clear()` removes
visible items without releasing those registry entries. A separate short-lived
process using the installed library created and cleared 100 menu items, then ran
garbage collection: visible items = 0, retained registry entries = 100. This proves
retention on that code path, not the exact fraction of Procwarden's 24 GiB it owns.
The reproduction imported `Menu`, `MenuItem`, and `NSApp` from installed
`rumps.rumps`, repeated `menu.clear(); menu.add(MenuItem("sample", callback=...))`
100 times, then called `menu.clear(); gc.collect()`. The result was
`{"cycles":100,"visible_menu_items":0,"registry_before":0,"registry_after_clear_and_gc":100}`.
It ran in a separate short-lived Python process with no displayed UI.

No Procwarden source or installed library was changed. Monitor restarts require
the user's pending approval; no system daemon was signalled. A lasting Procwarden
repair should reuse menu items or correctly release callback registrations and
verify a bounded registry over repeated refreshes.

## Owned resource recovery

Three sessions on the dedicated `/private/tmp/amux-gemini-lifecycle` socket were
left behind from the prior run. Each was older than 24 hours, unattached, and in
this task's `gemini-lifecycle-20260911/workspace/full-rerun` directory. Terminal
output was preserved before stopping these exact sessions:

- `amux-lc-gemini-1789145956044-author`
- `amux-lc-gemini-1789145956044-reviewer`
- `amux-lifecycle-1789145559196`

Their tmux server and remaining native Node descendants exited. Test records and
files were retained. The older production tmux server was left alone because
it still owns Codex processes; a replaced socket is not proof those processes
are disposable. No production card or queue was changed.

## Diagnostic repair and acceptance

The pressure warning now uses macOS MEM and CMPRS with PID and command, including
commands containing spaces and PID architecture markers. It logs measured,
n_considered, metric and failure reason. The bounded top-five probe uses C locale,
an absolute executable path, and kills/reaps its child at its deadline. Linux
continues to report RSS, explicitly labelled `rss_only`. Cleanup predicates and
worker admission are unchanged. LC-MEMORY-ATTRIBUTION is in the consolidated suite.

The first native regression caught the architecture marker (`567*`) as malformed;
this failure was retained and the parser was corrected using the local top manual
(`+`, `-`, `*` are architecture markers). Final checks and deployment are recorded below. An initial mutation from MEM to rsize stayed green because
this macOS top version aliases rsize to MEM; the live command confirmed the same
header and values. That attempt is not counted as negative-control evidence.
The meaningful replacement uses VSIZE (virtual address space), which must fail
the native metric contract. Raw logs and preserved terminal evidence are in the private
`memory-recovery-20260912` artifact directory; user conversations are not committed.

## Validation results

- `scripts/test-contended.sh -p amux-server --lib runtime_jobs::` → **594 passed,
  0 failed, 1 ignored, 1772 filtered out**. This selects runtime unit tests; it
  does not run every integration target or any native provider journey. The ignored
  test is the manual live-GitHub CI detector probe.
- `python3 -m unittest discover -s scripts/lifecycle -p 'test_*.py'` → **9 passed**.
- Exact `scripts/mutate.sh run` replaced `pid,mem,cmprs,command` with
  `pid,vsize,cmprs,command`: mutation landed, the native metric test failed
  (2 passed / 1 failed), and the inverse edit reverted it. The restored
  implementation passed in the final runtime selection above.
- The dedicated test socket and its remaining Node descendants were confirmed
  absent after cleanup. The host health check still returned admission `deny`.

- `scripts/safe-cargo.sh check --workspace` → **exit 0**.
- `scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings` → **exit 0**.

The [portable evidence receipt](evidence/lifecycle-memory-2026-09-12.json) keeps
measured results separate from the remaining host-recovery and native-run work.
