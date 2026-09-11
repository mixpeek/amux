# September 11: offline sync while the server still answered health

The triggering condition was host storage exhaustion. At investigation, health reported about 0.11 GiB free and raw logs contained `No space left on device`, `heartbeat: stamp failed error=writer thread is gone`, and `request-log queue rejected a row (dropped) error=channel closed`. The read-only health probe still reported `store: ok`. The diagnostic request-log query returned zero records because its writer was unavailable; it was not evidence of an error-free window.

The affected browser retained 26 operations and a cached worker list. Its connection error was real. The VPN health probe was reachable after recovery; there was no evidence here of an expired tailnet key or a VPN outage.

Deleting disposable caches did not restore usable space: local Time Machine snapshots retained their blocks. With the owner's explicit approval to reclaim about 20 GB, `tmutil thinlocalsnapshots / 20000000000 4` removed the oldest local snapshot and released approximately 62 GiB. Current files and external backups were not removed. The API service was restarted without restarting the tmux workers. `/api/sessions` returned 200, database revisions resumed, and the original browser later displayed Live and a queued-operation delivery confirmation. That confirmation is not a per-operation audit of all 26 items.

## Persistent defects fixed

- The single writer allowed a panicking mutation to terminate its thread. Later writes could never recover. Catch unwinding at the request boundary, roll back the mutation, return an error, and preserve the writer for subsequent requests.
- A manual `BEGIN IMMEDIATE` rolled back only a failure inside the caller's closure. Errors while writing the revision/event journal or committing could leave the transaction open, poisoning every later request. A transaction guard now rolls back every unsuccessful exit, including unwinding and failed commit. Events are still published only after commit.
- Health tested reads but not the writer. Its existing bounded, single-flight blocking probe now also executes a no-op transaction through the real writer. A dead, stalled, or read-only writer cannot produce a green health result. The probe creates no revisions or sync events.
- Detailed worker-read errors stay in the connection modal. An open modal updates when retries recover and preserves expanded details while the error is unchanged. Cached workers remain visible on the list.

Failure signals are `writer_mutation_failed`, `writer_mutation_panicked`, and `writer_probe_failed` (or the existing bounded probe deadline). Logs preserve the SQLite cause and autocommit state. No write is acknowledged on failure.

## Evidence and limits

The original code failed all four injected regressions: mutation panic, journal failure, deferred-constraint commit failure, and health with a readable but unwritable store. The corrected code passes them; an additional stalled-writer case checks bounded health latency, single-flight behavior, recovery, and unchanged revision. Existing board, boundary and health integration tests are also exercised. Browser acceptance covers desktop, mobile Chromium and iPhone/WebKit; it asserts the worker list is clear of the detailed notice and that modal Retry clears the failure.

Reader-pool exhaustion (28 connections in use) also occurred during the first recovery boots and triggered the watchdog. The old build did not record individual connection holders, and its stripped sample was insufficient to attribute those borrowers. Do not claim this patch identifies a particular leaking reader. That condition stopped after recovery; subsequent builds also cause intentional short reconnects during binary adoption. Host memory/swap pressure and low-disk warnings remained separate from the recovered API. Neither should be disguised as a healthy overall host.

Build/test caches and snapshots contributed to disk pressure; this investigation does not attribute all host disk usage to Amux. Snapshot deletion is not an automatic server recovery action and must not be added as an unapproved background policy.
