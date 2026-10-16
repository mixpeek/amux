#!/usr/bin/env node
// A list is a population, never an executed result. Missing evidence fails closed.
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

// What the expected population covers, against the unfiltered suite. A pull
// request may run a selection (scripts/ci-affected.mjs); this is what lets a
// green verdict say how much it did NOT run instead of reading as the full suite.
export function selectionSummary(expected, universe) {
  const per = {};
  const bump = (project, key) => { (per[project || '?'] ||= { run: 0, not_run: 0 })[key]++; };
  for (const t of universe.values()) bump(t.project, expected.has(t.id) ? 'run' : 'not_run');
  const notRun = Object.values(per).reduce((n, p) => n + p.not_run, 0);
  return { scope: notRun ? 'selected' : 'full', universe: universe.size, expected: expected.size, not_run: notRun,
    per_project: per,
    note: notRun ? `${notRun} of ${universe.size} tests were NOT selected for this run; the full suite runs on every push to main`
      : 'the expected population is the full suite' };
}

export function checkEvidence(root, expectedManifest, total = 4, universeManifest = null) {
  const errors = [], unfinished = [], seen = new Set();
  const counts = { expected: 0, skipped: 0, unexpected: 0 };
  let n = 0;
  const read = (file) => {
    try { return JSON.parse(fs.readFileSync(file, 'utf8')); }
    catch (error) { errors.push(`${file}: ${error.message}`); return null; }
  };
  const full = read(expectedManifest);
  const expected = new Map();
  if (!Array.isArray(full?.tests) || !full.tests.length) errors.push('empty or missing full population');
  for (const test of full?.tests || []) {
    if (!test.id || expected.has(test.id)) errors.push(`duplicate/invalid full test: ${test.id}`);
    expected.set(test.id, test);
  }
  for (let current = 1; current <= total; current++) {
    const dir = path.join(root, `shard-${current}`);
    const manifest = read(path.join(dir, 'manifest.json'));
    if (!manifest) continue;
    if (manifest.shard?.current !== current || manifest.shard?.total !== total)
      errors.push(`shard-${current}: wrong shard identity`);
    if (!Array.isArray(manifest.tests) || !manifest.tests.length) {
      errors.push(`shard-${current}: empty population`); continue;
    }
    const planned = new Map(), started = new Set(), finished = new Map();
    for (const test of manifest.tests) {
      n++;
      if (seen.has(test.id)) errors.push(`duplicate test across shards: ${test.id}`);
      seen.add(test.id); planned.set(test.id, test);
      if (JSON.stringify(expected.get(test.id)) !== JSON.stringify(test))
        errors.push(`test absent from full population or identity changed: ${test.id}`);
    }
    try {
      for (const line of fs.readFileSync(path.join(dir, 'events.ndjson'), 'utf8').split('\n').filter(Boolean)) {
        const event = JSON.parse(line);
        if (!planned.has(event.id)) errors.push(`unplanned event: ${event.id}`);
        if (event.event === 'started') started.add(event.id);
        else if (event.event === 'finished') {
          if (finished.has(event.id)) errors.push(`duplicate completion: ${event.id}`);
          finished.set(event.id, event);
          if (event.retry !== 0) errors.push(`unexpected retry: ${event.id}`);
          if (event.status === 'skipped' && event.outcome === 'skipped') counts.skipped++;
          else if (event.outcome === 'expected' && event.status === event.expected &&
                   ['passed', 'failed', 'timedOut'].includes(event.status)) counts.expected++;
          else { counts.unexpected++; errors.push(`unexpected outcome: ${event.id} ${event.status}/${event.outcome}`); }
        } else errors.push(`unknown event: ${event.event}`);
      }
    } catch (error) { errors.push(`${dir}/events.ndjson: ${error.message}`); }
    for (const [id, test] of planned) if (!finished.has(id))
      unfinished.push({ ...test, shard: current, phase: started.has(id) ? 'in_flight' : 'not_started' });
    const final = read(path.join(dir, 'final.json'));
    if (final?.status !== 'passed') errors.push(`shard-${current}: final status ${final?.status ?? 'missing'}`);
  }
  for (const id of expected.keys()) if (!seen.has(id)) errors.push(`missing planned test: ${id}`);
  if (unfinished.length) errors.push(`${unfinished.length} tests unfinished`);
  if (!counts.expected) errors.push('no executed expected outcomes (all skipped is not execution proof)');
  let selection;
  if (universeManifest) {
    const universe = new Map();
    for (const test of read(universeManifest)?.tests || []) universe.set(test.id, test);
    if (!universe.size) errors.push('empty or missing unfiltered population');
    for (const id of expected.keys()) if (!universe.has(id)) errors.push(`selected test absent from the unfiltered suite: ${id}`);
    selection = selectionSummary(expected, universe);
  }
  return { event: 'e2e_evidence_verdict', measured: n > 0, n_considered: n,
    full_population: expected.size, ...(selection ? { selection } : {}),
    passed: errors.length === 0, counts, unfinished, errors };
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  const [root, full, total, universe] = process.argv.slice(2);
  if (!root || !full) throw new Error('usage: check-e2e-evidence.mjs <shard-root> <expected-manifest> [total=4] [unfiltered-manifest]');
  const result = checkEvidence(root, full, total === undefined ? 4 : Number(total), universe || null);
  console.log(JSON.stringify(result, null, 2));
  process.exitCode = result.passed ? 0 : 1;
}
