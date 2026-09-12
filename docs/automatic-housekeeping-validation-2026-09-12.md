# Automatic housekeeping validation — 2026-09-12

This validates the hourly retention and memory-cache changes described in
[automatic housekeeping](automatic-housekeeping.md). It does not certify the
unrelated mobile/provider lifecycle as complete.

| Check | Result |
| --- | --- |
| `scripts/safe-cargo.sh check --workspace` | Passed; sampled peak RSS 2.20 GiB. |
| `scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings` | Passed; sampled peak RSS 4.70 GiB. |
| `scripts/test-contended.sh -p amux-server --no-fail-fast` | 60 targets: **2,640 passed, 7 failed, 33 ignored**; 595.62 seconds in the Cargo supervisor, sampled peak RSS 6.81 GiB. |
| `scripts/test-contended.sh -p amux-server --lib retention` | 15 passed, 0 failed (focused subset). |
| Lifecycle runner unit tests | 9 passed. |
| Lifecycle catalog | 87 unique cases; every supporting source exists. |
| Deliberate unsafe mutations | Five independently failed their intended assertions; each source edit was restored. |

The new tests cover old orphan deletion versus recent nested writes, open files
and working directories, symlinked roots/children, repositories, linked task
artifacts and saved messages; failed/empty/truncated/hung probes; oversized
reference snapshots; ordinary prose that mentions logs; spaces/Unicode/encoded
upload filenames; pending steering attachments; unavailable reference tables in
the actual storage tick; and inactive transcript cache expiry with longer TTLs.

The five negative controls disabled nested-write recency, deletion deferral after
a failed upload-reference probe, transcript-cache expiry, selective path queries,
and support for upload filenames containing spaces/Unicode. The corresponding
tests failed under each mutation and pass with the protections restored. Test
fixtures use private temporary directories, not production deletion.

The seven full-suite failures are the existing host admission boundary:

- `api::health::admission_tests::the_live_host_is_not_currently_denying`
- Five `api::workers` cases: cwd replacement, model change, terminal peek,
  start/stop/delete, and turn events.
- `replay_roundtrip::write_then_replay_round_trip`

The host reported normal current memory pressure but about 38,598 MiB of swap,
above the existing 8,192 MiB admission threshold. Worker starts returned HTTP 503.
No threshold was relaxed and no active worker was killed to make these tests pass.
The previous browser action-schema failure did not recur in this run.

A live read-only reference-query check found 249 matching board records for
`/logs/`, totaling about 2.9 MB, in 285 ms. Matching ordinary prose containing
“logs” also included more than 18 MB of board text; that could consume the safety
budget without representing a file reference. SQL now normalizes separators and
filters on actual directory paths before returning text. Reference reads and
new diagnostic-directory scans execute outside the async request executor.

During the final gates, `/health` remained HTTP 200 with `status: ok`, `store: ok`,
commit `0e3d2f5e76aa`, build `3b897206ae055502`, and PID 8407; one sampled response
was 17 ms. This brackets the pre-deployment measurement, not the new deployment.
The storage and browser jobs were active at 3,600-second and 120-second intervals.

An earlier full-test invocation was deliberately stopped during compilation to
correct the reference-query selector; it is recorded separately as exit 143 and
is not counted as a completed test run. The final Rust source fingerprints stayed
unchanged throughout the checks and complete server run.

Raw command logs, mutation logs, source fingerprints, and pre/post-deployment
health/storage/job snapshots are retained in the task's private
`results/automatic-housekeeping-20260912/` evidence directory. Deployment is checked
against `/health.commit_full`; a build command exiting zero alone is not adoption.

## Live deployment follow-up: the service PATH

The complete server run above applies to `506dfc0d`. That build was adopted as
`235fbf3edcddc2a5`, with the server/database healthy and PID 8407 unchanged.
Its first automatic sweep removed 14 aged rotated logs (253,032 logical bytes),
kept five referenced uploads, and applied database-history retention. Directory
cleanup explicitly deferred with `measured: false` and ENOENT.

The installed launch agent's PATH omitted `/usr/sbin`, where macOS supplies
`lsof`. The follow-up resolves `/usr/sbin/lsof` directly on macOS and includes the
executable in spawn-failure diagnostics. It leaves worker/service environment
settings unchanged. A new native test sets PATH to `/usr/bin:/bin` and requires
the probe to observe a real held file. Reverting to bare `lsof` makes that test
fail with the same missing-executable error observed in the service.

For this narrow executable-resolution follow-up, workspace check and all-target
Clippy passed again; the focused retention run passed **16 tests, 0 failed**.
The full server suite was not repeated after this adapter change. The six
mutation checks across both commits all failed for the intended reason and were
restored. Native probe coverage supplements the earlier controlled probe fixtures.
