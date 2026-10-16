import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import EvidenceReporter from '../e2e/ci-evidence-reporter.mjs';
import { checkEvidence } from '../scripts/check-e2e-evidence.mjs';

function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-ci-evidence-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const tests = ['passed', 'failed', 'skipped', 'passed'].map((status, i) => ({
    id: `test-${i}`, titlePath: () => ['suite', `case-${i}`],
    parent: { project: () => ({ name: ['desktop', 'mobile', 'ios-safari'][i % 3] }) },
    expectedStatus: status, outcome: () => status === 'skipped' ? 'skipped' : 'expected',
  }));
  const reporters = tests.map((item, i) => {
    const reporter = new EvidenceReporter({ outputDir: path.join(root, `shard-${i + 1}`) });
    reporter.onBegin({ shard: { current: i + 1, total: 4 } }, { allTests: () => [item] });
    const result = { status: item.expectedStatus, duration: 1, retry: 0 };
    reporter.onTestBegin(item, result);
    reporter.onTestEnd(item, result);
    reporter.onEnd({ status: 'passed', duration: 2 });
    return reporter;
  });
  const full = new EvidenceReporter({ outputDir: path.join(root, 'full') });
  full.onBegin({ shard: null }, { allTests: () => tests });
  const check = () => checkEvidence(root, path.join(root, 'full', 'manifest.json'));
  const edit = (name, fn) => {
    const file = path.join(root, name);
    fs.writeFileSync(file, JSON.stringify(fn(JSON.parse(fs.readFileSync(file, 'utf8')))));
  };
  return { root, tests, reporters, check, edit };
}

test('complete real reporter callbacks account for pass, expected failure, and explicit skip', t => {
  const f = fixture(t), result = f.check();
  assert.equal(result.passed, true, JSON.stringify(result));
  assert.equal(result.n_considered, 4);
  assert.deepEqual(result.counts, { expected: 3, skipped: 1, unexpected: 0 });
});
for (const status of ['failed', 'timedout', 'interrupted']) test(`final ${status} refuses green`, t => {
  const f = fixture(t); f.edit('shard-1/final.json', x => ({ ...x, status }));
  assert.equal(f.check().passed, false);
  assert.ok(f.check().errors.some(x => x.includes(`final status ${status}`)));
});
test('hard cancellation names both in-flight and not-started operands', t => {
  const f = fixture(t);
  fs.rmSync(path.join(f.root, 'shard-1/final.json'));
  fs.writeFileSync(path.join(f.root, 'shard-1/events.ndjson'), JSON.stringify({ event: 'started', id: 'test-0' }) + '\n');
  fs.writeFileSync(path.join(f.root, 'shard-2/events.ndjson'), '');
  const result = f.check();
  assert.equal(result.passed, false);
  assert.deepEqual(result.unfinished.map(x => [x.id, x.phase]), [['test-0', 'in_flight'], ['test-1', 'not_started']]);
});
test('list-mode passed final never counts as execution', t => {
  const f = fixture(t);
  for (let i = 1; i <= 4; i++) fs.writeFileSync(path.join(f.root, `shard-${i}/events.ndjson`), '');
  assert.equal(f.check().passed, false);
  assert.equal(f.check().unfinished.length, 4);
});
test('missing shard fails even if every available shard passed', t => {
  const f = fixture(t); fs.rmSync(path.join(f.root, 'shard-4'), { recursive: true });
  assert.equal(f.check().passed, false);
  assert.ok(f.check().errors.includes('missing planned test: test-3'));
});
test('zero population is unmeasured and fails', t => {
  const f = fixture(t);
  for (let i = 1; i <= 4; i++) f.edit(`shard-${i}/manifest.json`, x => ({ ...x, tests: [] }));
  f.edit('full/manifest.json', x => ({ ...x, tests: [] }));
  assert.equal(f.check().passed, false); assert.equal(f.check().measured, false);
});
test('duplicate shard operands cannot replace an omitted test', t => {
  const f = fixture(t);
  const first = JSON.parse(fs.readFileSync(path.join(f.root, 'shard-1/manifest.json'))).tests;
  f.edit('shard-2/manifest.json', x => ({ ...x, tests: first }));
  assert.ok(f.check().errors.includes('duplicate test across shards: test-0'));
  assert.equal(f.check().passed, false);
});
test('unexpected assertion and duplicate completion each fail', t => {
  const f = fixture(t);
  const file = path.join(f.root, 'shard-1/events.ndjson');
  fs.appendFileSync(file, JSON.stringify({ event: 'finished', id: 'test-0', retry: 0, status: 'failed', expected: 'passed', outcome: 'unexpected' }) + '\n');
  const result = f.check();
  assert.equal(result.passed, false);
  assert.ok(result.errors.includes('duplicate completion: test-0'));
  assert.ok(result.errors.includes('unexpected outcome: test-0 failed/unexpected'));
});
test('new run invalidates a previous successful final result', t => {
  const f = fixture(t);
  f.reporters[0].onBegin({ shard: { current: 1, total: 4 } }, { allTests: () => [f.tests[0]] });
  assert.equal(fs.existsSync(path.join(f.root, 'shard-1/final.json')), false);
  assert.equal(f.check().passed, false);
});
test('truncated write fails with a parse diagnostic, never quiet zero', t => {
  const f = fixture(t); fs.appendFileSync(path.join(f.root, 'shard-1/events.ndjson'), '{"event":');
  assert.equal(f.check().passed, false);
  assert.ok(f.check().errors.some(x => x.includes('events.ndjson:')));
});

// Exercise the installed runner, not just a mock of its callback ordering.
test('actual Playwright pass/failure/timeout/list callbacks produce honest evidence', async t => {
  const { createRequire } = await import('node:module');
  const { spawnSync } = await import('node:child_process');
  const { fileURLToPath } = await import('node:url');
  const require = createRequire(import.meta.url);
  const root = fs.mkdtempSync(path.join(path.dirname(fileURLToPath(import.meta.url)), '.ci-evidence-fixture-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const reporter = fileURLToPath(new URL('../e2e/ci-evidence-reporter.mjs', import.meta.url));
  fs.writeFileSync(path.join(root, 'playwright.config.cjs'), `module.exports = { testDir: '.', retries: 0, workers: 1, reporter: [[${JSON.stringify(reporter)}]], timeout: 2000 };`);
  fs.writeFileSync(path.join(root, 'probe.spec.cjs'), `
    const { test, expect } = require('@playwright/test');
    test('real operand', async () => {
      if (process.env.PROBE_MODE === 'timeout') await new Promise(() => {});
      expect(process.env.PROBE_MODE).not.toBe('failure');
    });
  `);
  for (const mode of ['pass', 'failure', 'timeout', 'list']) {
    const evidence = path.join(root, mode);
    const result = spawnSync(process.execPath, [require.resolve('@playwright/test/cli'), 'test',
      '--config', path.join(root, 'playwright.config.cjs'), '--shard=1/1',
      ...(mode === 'list' ? ['--list'] : []),
      ...(mode === 'timeout' ? ['--global-timeout=1000'] : [])], {
      cwd: root, env: { ...process.env, PROBE_MODE: mode, AMUX_E2E_EVIDENCE_DIR: path.join(evidence, 'shard-1') },
      encoding: 'utf8', timeout: 20000,
    });
    assert.ifError(result.error);
    assert.equal(result.status === 0, ['pass', 'list'].includes(mode), `${mode}: ${result.stdout}\n${result.stderr}`);
    const verdict = checkEvidence(evidence, path.join(evidence, 'shard-1/manifest.json'), 1);
    assert.equal(verdict.passed, mode === 'pass', `${mode}: ${JSON.stringify(verdict)}`);
    if (mode === 'list') assert.equal(verdict.unfinished.length, 1);
    if (mode === 'timeout') assert.ok(verdict.errors.some(x => x.includes('final status timedout')));
  }
});

// A PR runs a selection (scripts/ci-affected.mjs). Given the unfiltered
// population too, a green verdict must say what it did not run, per project.
test('a selected population reports what was not run instead of reading as the full suite', t => {
  const f = fixture(t);
  const universe = new EvidenceReporter({ outputDir: path.join(f.root, 'universe') });
  const extra = { id: 'test-x', titlePath: () => ['suite', 'unselected'], parent: { project: () => ({ name: 'ios-safari' }) } };
  universe.onBegin({ shard: null }, { allTests: () => [...f.tests, extra] });
  const result = checkEvidence(f.root, path.join(f.root, 'full', 'manifest.json'), 4, path.join(f.root, 'universe', 'manifest.json'));
  assert.equal(result.passed, true, JSON.stringify(result));
  assert.equal(result.selection.scope, 'selected');
  assert.equal(result.selection.not_run, 1);
  assert.deepEqual(result.selection.per_project['ios-safari'], { run: 1, not_run: 1 });
  assert.match(result.selection.note, /NOT selected/);
});
test('the full suite against itself reports scope full', t => {
  const f = fixture(t);
  const result = checkEvidence(f.root, path.join(f.root, 'full', 'manifest.json'), 4, path.join(f.root, 'full', 'manifest.json'));
  assert.equal(result.selection.scope, 'full');
  assert.equal(result.selection.not_run, 0);
});
test('a selected test missing from the unfiltered suite refuses green', t => {
  const f = fixture(t);
  const universe = new EvidenceReporter({ outputDir: path.join(f.root, 'universe') });
  universe.onBegin({ shard: null }, { allTests: () => f.tests.slice(1) });
  const result = checkEvidence(f.root, path.join(f.root, 'full', 'manifest.json'), 4, path.join(f.root, 'universe', 'manifest.json'));
  assert.equal(result.passed, false);
  assert.ok(result.errors.some(e => e.includes('absent from the unfiltered suite: test-0')));
});
