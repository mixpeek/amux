#!/usr/bin/env node
// Duration-balanced e2e shard plan. Playwright's --shard splits by test COUNT,
// so on 2026-10-15 one shard carried 1624s of tests while others carried
// 370-810s and set the whole run's wall time. This assigns (project, file)
// units to shards by measured duration (e2e/durations.json), longest first,
// each to the currently lightest shard. Units with no measurement get the
// median, so a new spec still lands somewhere.
//
// usage: e2e-plan.mjs <current> <total> <list.json> [durations.json] > plan.json
//   list.json = `npx playwright test --list --reporter=json` output.
import fs from 'node:fs';

export function units(list) {
  const out = new Map();
  const walk = (suite, file) => {
    const f = suite.file || file;
    for (const spec of suite.specs || []) for (const t of spec.tests || []) {
      const key = t.projectName + '|' + (spec.file || f);
      out.set(key, (out.get(key) || 0) + 1);
    }
    for (const s of suite.suites || []) walk(s, f);
  };
  for (const s of list.suites || []) walk(s, s.file);
  return out;
}

export function plan(unitCounts, durations, total) {
  const measured = Object.values(durations).sort((a, b) => a - b);
  const median = measured.length ? measured[Math.floor(measured.length / 2)] : 30;
  const items = [...unitCounts.keys()].map(k => [k, durations[k] ?? median])
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  const shards = Array.from({ length: total }, () => ({ seconds: 0, units: [] }));
  for (const [key, secs] of items) {
    let best = 0;
    // Ties go to the shard with fewer units, so a PR plan of a few specs never
    // leaves a shard empty while another holds two zero-second units (an empty
    // shard has no population and fails the evidence check by design).
    for (let i = 1; i < total; i++) {
      const a = shards[i], b = shards[best];
      if (a.seconds < b.seconds || (a.seconds === b.seconds && a.units.length < b.units.length)) best = i;
    }
    shards[best].seconds += secs; shards[best].units.push(key);
  }
  return shards;
}

if (process.argv[1] && import.meta.url === `file://${process.argv[1]}`) {
  const [cur, tot, listFile, durFile = new URL('../e2e/durations.json', import.meta.url).pathname] = process.argv.slice(2);
  const current = Number(cur), total = Number(tot);
  const list = JSON.parse(fs.readFileSync(listFile, 'utf8'));
  const durations = fs.existsSync(durFile) ? JSON.parse(fs.readFileSync(durFile, 'utf8')) : {};
  const shards = plan(units(list), durations, total);
  const mine = shards[current - 1];
  const byProject = {};
  for (const k of mine.units) { const [p, f] = k.split(/\|(.*)/s); (byProject[p] ||= []).push(f); }
  const max = Math.max(...shards.map(s => s.seconds));
  console.error(JSON.stringify({ verdict: 'e2e_shard_plan', measured: true, n_considered: shards.reduce((n, s) => n + s.units.length, 0),
    shard: current, total, units: mine.units.length, planned_seconds: Math.round(mine.seconds), slowest_shard_seconds: Math.round(max) }));
  console.log(JSON.stringify({ current, total, projects: byProject }));
}
