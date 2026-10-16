// The shard planner must assign every (project, file) unit exactly once,
// deterministically, and keep the slowest shard close to the average.
import test from 'node:test';
import assert from 'node:assert/strict';
import { plan, units } from '../scripts/e2e-plan.mjs';

const list = { suites: [
  { file: 'a.spec.ts', specs: [{ file: 'a.spec.ts', tests: [{ projectName: 'desktop' }, { projectName: 'mobile' }] }] },
  { file: 'b.spec.ts', specs: [{ file: 'b.spec.ts', tests: [{ projectName: 'desktop' }] }], suites: [
    { specs: [{ file: 'b.spec.ts', tests: [{ projectName: 'ios-safari' }] }] }] },
  { file: 'new.spec.ts', specs: [{ file: 'new.spec.ts', tests: [{ projectName: 'desktop' }] }] },
] };
const durations = { 'desktop|a.spec.ts': 100, 'mobile|a.spec.ts': 90, 'desktop|b.spec.ts': 60, 'ios-safari|b.spec.ts': 50 };

test('every unit lands in exactly one shard, including an unmeasured one', () => {
  const shards = plan(units(list), durations, 3);
  const all = shards.flatMap(s => s.units).sort();
  assert.deepEqual(all, ['desktop|a.spec.ts', 'desktop|b.spec.ts', 'desktop|new.spec.ts', 'ios-safari|b.spec.ts', 'mobile|a.spec.ts']);
});

test('the plan is deterministic and balanced', () => {
  const a = plan(units(list), durations, 2), b = plan(units(list), durations, 2);
  assert.deepEqual(a, b);
  const secs = a.map(s => s.seconds), avg = secs.reduce((x, y) => x + y) / secs.length;
  assert.ok(Math.max(...secs) - avg <= 100, `slowest ${Math.max(...secs)} vs average ${avg}`);
});

// A PR plan can be a handful of units; equal seconds must still spread them,
// because an empty shard has no population and fails the evidence check.
test('ties go to the shard with fewer units, so no shard is left empty', () => {
  const counts = new Map([['desktop|x.spec.ts', 1], ['mobile|x.spec.ts', 1], ['desktop|y.spec.ts', 1]]);
  const shards = plan(counts, { 'desktop|x.spec.ts': 0, 'mobile|x.spec.ts': 0, 'desktop|y.spec.ts': 0 }, 3);
  assert.deepEqual(shards.map(s => s.units.length), [1, 1, 1]);
});
