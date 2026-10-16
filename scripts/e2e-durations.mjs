#!/usr/bin/env node
// Rebuild e2e/durations.json (the shard planner's input) from CI evidence:
// download a run's e2e-evidence-* artifacts into one folder, then
//   node scripts/e2e-durations.mjs <evidence-root> > e2e/durations.json
// Keys are "<project>|<spec file>", values the summed seconds of that unit.
import fs from 'node:fs';
import path from 'node:path';

const root = process.argv[2];
if (!root) throw new Error('usage: e2e-durations.mjs <evidence-root>');
const shardDirs = [];
const walk = d => { for (const e of fs.readdirSync(d, { withFileTypes: true })) {
  const p = path.join(d, e.name);
  if (e.isDirectory()) { if (fs.existsSync(path.join(p, 'manifest.json'))) shardDirs.push(p); else walk(p); }
} };
walk(root);
const title = new Map(), seconds = {};
for (const d of shardDirs) for (const t of JSON.parse(fs.readFileSync(path.join(d, 'manifest.json'), 'utf8')).tests) title.set(t.id, t);
for (const d of shardDirs) for (const line of fs.readFileSync(path.join(d, 'events.ndjson'), 'utf8').split('\n').filter(Boolean)) {
  const e = JSON.parse(line), t = title.get(e.id);
  if (e.event !== 'finished' || !t) continue;
  const key = t.project + '|' + t.title[2];
  seconds[key] = (seconds[key] || 0) + e.duration_ms / 1000;
}
const out = Object.fromEntries(Object.keys(seconds).sort().map(k => [k, Math.round(seconds[k] * 10) / 10]));
console.error(JSON.stringify({ verdict: 'e2e_durations_built', measured: true, n_considered: Object.keys(out).length, shards: shardDirs.length }));
console.log(JSON.stringify(out, null, 1));
