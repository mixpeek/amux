# Automatic Amux housekeeping

The server's existing storage job runs hourly, including after a server restart.
The browser reaper runs every two minutes. These jobs do not need a worker, an
LLM call, an open dashboard, or a Codex reminder. The table gives defaults;
storage diagnostics show the effective settings.

| Resource | Automatic policy |
| --- | --- |
| Server and worker logs | Rotate at 64 MiB and 20 MiB respectively; remove aged rotations after 3 days. |
| Diagnostic run folders under `logs/` | Keep for at least 30 days. Preserve referenced folders, open files and working directories, recently written descendants, and symlinks. |
| Evidence and audit run folders | Existing 7-day and 30-day policies, with the same reference, activity and descendant checks. |
| Uploads | Remove unreferenced files after 7 days. Unarchived board references, registered artifacts, saved/durable messages, command history and pending steering protect attachments, including filenames with spaces/Unicode and encoded file URLs. |
| Transcript evidence memory cache | Expire entries inactive for five minutes (or a longer configured cache TTL) and release map capacity on inserts and hourly sweeps. |
| Browser processes | Reaper applies idle/hard TTLs; persistent registered login profiles remain protected. |
| Cargo | Shared targets, bounded jobs/RSS/runtime/disk, guarded idle cleanup and failed-build backoff; see [resource budgets](resource-budgets.md). |
| Append-only database history | Per-table retention and conditional vacuum, as shown in storage diagnostics. |

Inline references on archived or discarded cards alone do not pin uploads, matching
the existing policy. Registered artifacts and retained message references still do.

`AMUX_STORAGE_SWEEP_SECS` defaults to 3600; zero disables the storage job.
`AMUX_RUN_LOG_RETAIN_DAYS` defaults to 30; zero disables run-folder cleanup.
Existing evidence/audit/upload retention settings remain available.

Hidden housekeeping folders are excluded from run-folder retention. The diagnostic-folder scan is limited to 20,000 metadata entries and two seconds
per directory category. It does not follow symlinks. An open-file/working-directory
probe uses `lsof` (included with macOS; required on Linux), with a five-second
timeout and an 8 MiB output limit. Reference snapshots
are capped at 16 MiB. Probe errors defer the affected cleanup and expose a reason;
they never substitute an empty list of references or active processes.

These are retention rules, not universal disk quotas. A folder being actively
written or kept as evidence can remain large. A recent open-file snapshot and
mtime checks cannot provide a transactional lock against arbitrary worker file
writes. Repository folders containing `.git` are preserved. Cleanup does not
purge OS swap, restart active workers to lower RSS, erase browser logins, delete
backups, or erase arbitrary project files. Rust releasing cached allocations
also does not guarantee an immediate drop in macOS's reported process RSS.

Inspect `GET /api/debug/storage` for the last sweep, retained/deleted counts,
actual bytes deleted, expired cache entries, and deferred-probe reasons. The
Storage retention item in `GET /api/system-jobs` uses the same result. Log rotation
is not counted as freed space while the rotated copy still exists. Deleted-byte
counters sum logical file sizes; `free_gb` separately measures filesystem space
available, which can differ because of snapshots or open deleted files.
New reference queries and diagnostic-directory scans run off the async request executor.

Acceptance coverage: `LC-AUTOMATIC-HOUSEKEEPING` in the consolidated lifecycle.
Run `scripts/test-contended.sh -p amux-server --no-fail-fast` for the server
contracts, including the cleanup fixtures. Production deletion is not used as a
test substitute: fixtures create old, recent, linked and active files in private
temporary directories and assert both deletion and preservation.

Recorded results: [housekeeping validation](automatic-housekeeping-validation-2026-09-12.md).
