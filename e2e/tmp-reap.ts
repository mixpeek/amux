// Reap the temp roots the e2e harnesses mint and never delete.
//
// Both playwright configs call `mkdtempSync` at CONFIG LOAD to get a private
// directory: the lifecycle config needs a short tmux socket root (macOS TMPDIR
// overflows the Unix socket path limit), and the broad config needs one
// AMUX_HOME per target. Neither ever removed it, so every run, every
// `--project` filter and every crashed attempt left one behind. Measured on
// this box 2026-09-12: 2,389 `amux-lc-*` (empty — the tmux socket inside is
// unlinked when tmux exits, the directory is not) and 349 `amux-e2e-*` holding
// 448 MB.
//
// WHY REAP AT LOAD RATHER THAN TEAR DOWN AT EXIT. A teardown hook only runs
// when the run ends cleanly, and the litter is dominated by runs that did not:
// SIGINT from a watching human, a webServer that never came up, a killed CI
// job. Reaping OTHER runs' stale roots on the way in is idempotent, needs no
// cooperation from the process that leaked, and cleans up after crashes that a
// teardown by construction cannot.
//
// WHY `~/.tmp-reaper.sh` DID NOT ALREADY DO THIS: it defaults to
// `--min-size-mb 200`, and 2,389 of these are zero bytes. A size floor is the
// right guard for build-snapshot debris and it is exactly wrong for directory
// litter, which costs inodes and `ls` time rather than space.
import fs from 'node:fs';
import path from 'node:path';

/** Runs finish in minutes; anything idle this long belongs to a dead run. */
export const STALE_MS = 6 * 60 * 60 * 1000;

/**
 * Remove `<root>/<prefix>*` directories last modified more than `maxAgeMs` ago.
 *
 * Deliberately narrow: one exact root, one amux-owned prefix, and an age floor,
 * so a CONCURRENT run's root (seconds old) can never be taken. Best effort —
 * a directory that disappears under us or refuses to delete is skipped, because
 * failing to clean up litter must never fail the test run that tried.
 */
export function reapStaleTmp(root: string, prefix: string, maxAgeMs = STALE_MS): { removed: number; bytes: number } {
  const cutoff = Date.now() - maxAgeMs;
  let removed = 0;
  let bytes = 0;
  let names: string[];
  try {
    names = fs.readdirSync(root);
  } catch {
    return { removed, bytes };
  }
  for (const name of names) {
    if (!name.startsWith(prefix)) continue;
    const full = path.join(root, name);
    try {
      const st = fs.lstatSync(full);
      // Directories only. A symlink matching the prefix is not ours to follow.
      if (!st.isDirectory() || st.isSymbolicLink()) continue;
      if (st.mtimeMs >= cutoff) continue;
      bytes += dirBytes(full);
      fs.rmSync(full, { recursive: true, force: true });
      removed++;
    } catch {
      // Vanished, busy, or not ours to remove — leave it.
    }
  }
  // THE LOG SIGNAL (CLAUDE.md two-fix rule): a regression in the reaper shows
  // up as this line reporting a steadily climbing `removed` every run, which is
  // the shape of "the leak is back", rather than as silence.
  if (removed) {
    console.log(`[e2e] reaped ${removed} stale ${prefix}* temp dir(s) from ${root} (${(bytes / 1e6).toFixed(1)} MB)`);
  }
  return { removed, bytes };
}

/** Size of a directory tree, best effort; an unreadable entry counts as zero. */
function dirBytes(dir: string): number {
  let total = 0;
  let entries: fs.Dirent[];
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return total;
  }
  for (const e of entries) {
    const full = path.join(dir, e.name);
    try {
      if (e.isDirectory() && !e.isSymbolicLink()) total += dirBytes(full);
      else if (e.isFile()) total += fs.statSync(full).size;
    } catch {
      // skip
    }
  }
  return total;
}
