// scripts/ci-affected.mjs decides what a pull request's CI runs. Every rule
// here is pinned by a fixture diff, and the real-repo checks at the bottom keep
// its tables (tree guards, smoke set, infra list) honest against the tree.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as espree from 'espree';
import {
  classify, parseDiff, outputs, changedFunctions, lineTokens, routePrefixes, rustClause, gitRepo,
  TREE_GUARDS, RUST_ONLY_WALKERS, SMOKE, E2E_INFRA, MAX_SELECTED_SPECS, SECONDS_PER_SHARD, FULL_SHARDS,
} from '../scripts/ci-affected.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// An in-memory repository: { path: content }.
function fakeRepo(files, durations = {}) {
  const base = {
    'e2e/worker-toolbar-boot.spec.ts': 'boot', 'e2e/worker-card-counts.spec.ts': 'counts',
    'e2e/peek-default-tab.spec.ts': 'peek', 'e2e/settings-layout.spec.ts': 'settings',
    'e2e/lifecycle/board-header.spec.ts': 'board on a phone',
    'crates/amux-server/src/api/mod.rs': '', ...files,
  };
  return {
    read: (p) => (p in base ? base[p] : null),
    specs: () => Object.keys(base).filter(p => p.startsWith('e2e/') && p.endsWith('.spec.ts')).map(p => p.slice(4)).sort(),
    rustFiles: () => Object.keys(base).filter(p => p.startsWith('crates/') && p.endsWith('.rs')),
    durations, espree, pkgOf: (dir) => dir,
  };
}
const change = (p, added = [], removed = [], extra = {}) =>
  ({ path: p, deleted: false, hunks: [{ newStart: 1, newCount: added.length, added, removed }], ...extra });
const pr = (files, repo) => classify({ event: 'pull_request', files, repo });

test('push to main runs everything at full width, whatever changed', () => {
  const v = classify({ event: 'push', files: [change('README.md')], repo: fakeRepo({}) });
  const o = outputs(v);
  assert.equal(o.rust, 'full');
  assert.equal(o.e2e, 'true');
  assert.equal(o.full_e2e, 'true');
  assert.equal(o.e2e_specs, '');
  assert.equal(o.ios_specs, '');
  assert.equal(o.e2e_shard_total, String(FULL_SHARDS));
  assert.deepEqual(JSON.parse(o.rust_shards), [1, 2, 3, 4, 5, 6, 7, 8]);
  assert.equal(o.none, 'false');
});

test('docs only: nothing runs and the verdict says none', () => {
  const v = pr([change('docs/ux/notes.md', ['x']), change('frustrations.md', ['y'])], fakeRepo({}));
  assert.equal(v.none, true);
  assert.equal(v.rust.scope, 'none');
  assert.equal(v.e2e.run, false);
  assert.equal(v.spa, false);
  assert.equal(outputs(v).e2e, 'false');
});

test('a Rust source change runs the full Rust suite and only the e2e smoke set', () => {
  const v = pr([change('crates/amux-server/src/runtime_jobs/board_drive.rs', ['let x = 1;'])], fakeRepo({}));
  assert.equal(v.rust.scope, 'full');
  assert.ok(v.rust.reasons.some(r => r.endsWith('Rust compile input')), JSON.stringify(v.rust.reasons));
  assert.equal(v.e2e.run, true);
  assert.equal(v.e2e.full, false);
  assert.deepEqual(v.e2e.specs, [...SMOKE].sort());
});

test('a Rust test-only change runs Rust but no e2e', () => {
  const v = pr([change('crates/amux-server/tests/foo.rs', ['#[test] fn a() {}'])], fakeRepo({}));
  assert.equal(v.rust.scope, 'full');
  assert.equal(v.e2e.run, false);
});

test('an api module change selects the specs that call a route it registers', () => {
  const repo = fakeRepo({
    'crates/amux-server/src/api/mod.rs': '.nest("/api/board", board::routes())\n.nest(\n  "/api/workers",\n  workers::routes())',
    'crates/amux-server/src/api/board.rs': '.route("/", get(list)).route("/{id}/notes", get(n))',
    'e2e/board-thing.spec.ts': "await page.request.get('/api/board?all=1')",
    'e2e/boards-plural.spec.ts': "await page.request.get('/api/boards')",
    'e2e/workers-thing.spec.ts': "'/api/workers'",
  });
  const v = pr([change('crates/amux-server/src/api/board.rs', ['// x'])], repo);
  assert.ok(v.e2e.specs.includes('board-thing.spec.ts'));
  assert.ok(!v.e2e.specs.includes('boards-plural.spec.ts'), 'a longer route name is not the same route');
  assert.ok(!v.e2e.specs.includes('workers-thing.spec.ts'));
  assert.deepEqual(routePrefixes('board', [repo.read('crates/amux-server/src/api/board.rs')], repo.read('crates/amux-server/src/api/mod.rs')),
    ['/api/board']);
});

test('dashboard: a changed function, and ids on changed lines, select the specs naming them', () => {
  const app = ['function untouched() { return 1; }', 'function openVault() {', "  const el = document.getElementById('vault-q');", '}', ''].join('\n');
  const repo = fakeRepo({
    'crates/amux-dashboard/static/app.js': app,
    'e2e/vault.spec.ts': 'await page.evaluate(() => openVault())',
    'e2e/vault-q.spec.ts': "page.locator('#vault-q')",
    'e2e/untouched.spec.ts': 'untouched()',
    'e2e/generic.spec.ts': "toHaveClass(/active/)",
  });
  const f = { path: 'crates/amux-dashboard/static/app.js', deleted: false,
    hunks: [{ newStart: 3, newCount: 1, added: ["  const el = document.getElementById('vault-q'); el.classList.add('active');"], removed: [] }] };
  const v = pr([f], repo);
  assert.ok(v.e2e.specs.includes('vault.spec.ts'), 'function whose body the hunk touched');
  assert.ok(v.e2e.specs.includes('vault-q.spec.ts'), 'id on the changed line');
  assert.ok(!v.e2e.specs.includes('untouched.spec.ts'), 'a function the hunk did not touch');
  assert.ok(!v.e2e.specs.includes('generic.spec.ts'), 'a generic class token does not select');
  assert.equal(v.spa, true);
});

// AF-984: the land gate runs the head of this list, so a routing change must
// put the specs that navigate there (5297440a6 ran 12 of 39 by name and missed
// lifecycle/journey, which it broke).
test('a navigation-core change orders the specs that navigate first; any other change keeps name order', () => {
  const app = ['function routeTo(v) {', '  showPane(v);', '}', ''].join('\n');
  const repo = fakeRepo({
    'crates/amux-dashboard/static/app.js': app,
    'e2e/a-plain.spec.ts': 'routeTo(1)',
    'e2e/m-reload.spec.ts': 'routeTo(1); await page.reload();',
    'e2e/z-deeplink.spec.ts': "routeTo(1); await page.goto('/#peek=lane');",
  });
  const hunk = (line) => ({ path: 'crates/amux-dashboard/static/app.js', deleted: false,
    hunks: [{ newStart: 2, newCount: 1, added: [line], removed: [] }] });
  const nav = pr([hunk("  window.addEventListener('popstate', () => showPane(v));")], repo);
  assert.equal(nav.e2e.nav_core, true);
  assert.ok(nav.e2e.reasons.some(r => r.includes('navigation core')), nav.e2e.reasons.join('; '));
  const at = (s) => nav.e2e.specs.indexOf(s);
  assert.ok(at('z-deeplink.spec.ts') < at('a-plain.spec.ts'), nav.e2e.specs.join(', '));
  assert.ok(at('m-reload.spec.ts') < at('a-plain.spec.ts'), nav.e2e.specs.join(', '));
  assert.ok(at('lifecycle/board-header.spec.ts') < at('a-plain.spec.ts'), 'a lifecycle journey counts as navigating');
  const plain = pr([hunk('  showPane(v + 1);')], repo);
  assert.equal(plain.e2e.nav_core, false);
  assert.deepEqual(plain.e2e.specs, [...plain.e2e.specs].sort(), 'name order when routing is untouched');
  assert.deepEqual([...nav.e2e.specs].sort(), [...plain.e2e.specs].sort(), 'ordering changes, selection does not');
});

test('a changed spec leads even when a dashboard token picked it first', () => {
  const app = ['function routeTo(v) {', '  history.replaceState(null, "", "#x");', '}', ''].join('\n');
  const repo = fakeRepo({ 'crates/amux-dashboard/static/app.js': app, 'e2e/zz-own.spec.ts': 'routeTo(1)' });
  const v = pr([
    { path: 'crates/amux-dashboard/static/app.js', deleted: false, hunks: [{ newStart: 2, newCount: 1, added: ['  history.replaceState(null, "", "#y");'], removed: [] }] },
    change('e2e/zz-own.spec.ts', ['x']),
  ], repo);
  assert.equal(v.e2e.specs[0], 'zz-own.spec.ts', v.e2e.specs.join(', '));
});

test('changedFunctions pins a deletion-only hunk to the function it sat in', () => {
  const src = 'function a() {\n  1;\n}\nconst b = () => {\n  2;\n};\n';
  assert.deepEqual(changedFunctions(src, [{ newStart: 5, newCount: 0 }], espree).names, ['b']);
  assert.deepEqual(changedFunctions(src, [{ newStart: 2, newCount: 1 }], espree).names, ['a']);
});

test('css tokens come from selectors, never from colours in declarations', () => {
  assert.deepEqual([...lineTokens('.vault-row > #vault-head {', 'css')].sort(), ['vault-head', 'vault-row']);
  assert.deepEqual([...lineTokens('  color: #fafafa; border: 1px solid #abc;', 'css')], []);
  assert.deepEqual([...lineTokens('  background-image: url(sprite.webp);', 'css')], [], 'a declaration is not a selector');
  // A multi-line value ending in a comma reads like a selector list; its colour is still not an id.
  assert.deepEqual([...lineTokens('  box-shadow: 0 0 0 1px #abcdef,', 'css')], []);
  assert.deepEqual([...lineTokens('<button data-action="open-vault" class="vault-btn btn">', 'markup')].sort(), ['open-vault', 'vault-btn']);
});

test('a changed spec runs itself; a deleted spec runs nothing', () => {
  const repo = fakeRepo({ 'e2e/a.spec.ts': 'a', 'e2e/b.spec.ts': 'b' });
  const v = pr([change('e2e/a.spec.ts', ['x']), change('e2e/gone.spec.ts', [], ['y'], { deleted: true })], repo);
  assert.ok(v.e2e.specs.includes('a.spec.ts'));
  assert.ok(!v.e2e.specs.includes('b.spec.ts'));
  assert.ok(v.e2e.reasons.some(r => r.includes('gone.spec.ts: deleted')));
});

test('an e2e helper module selects the specs that import it', () => {
  const repo = fakeRepo({ 'e2e/a.spec.ts': "import { x } from './reader-scroll';", 'e2e/b.spec.ts': "import { y } from './helpers';" });
  const v = pr([change('e2e/reader-scroll.ts', ['x'])], repo);
  assert.ok(v.e2e.specs.includes('a.spec.ts'));
  assert.ok(!v.e2e.specs.includes('b.spec.ts'));
});

test('e2e infrastructure runs every spec on every project, ios-safari included', () => {
  const v = pr([change('e2e/playwright.config.ts', ['x'])], fakeRepo({}));
  assert.equal(v.e2e.full, true);
  assert.equal(v.e2e.specs, null);
  assert.equal(v.e2e.ios_specs, null);
  assert.equal(outputs(v).e2e_shard_total, String(FULL_SHARDS));
});

test('over the spec limit goes full on desktop+mobile and keeps ios-safari to the relevant part', () => {
  const files = {};
  for (let i = 0; i <= MAX_SELECTED_SPECS; i++) files[`e2e/s${i}.spec.ts`] = i === 0 ? 'setViewportSize phone' : 'desktop only';
  const v = pr(Object.keys(files).map(p => change(p, ['x'])), fakeRepo(files));
  assert.equal(v.e2e.full, true);
  assert.equal(v.e2e.specs, null);
  assert.ok(v.e2e.ios_specs.includes('s0.spec.ts'));
  assert.ok(!v.e2e.ios_specs.includes('s1.spec.ts'));
});

test('ios-safari keeps only iOS-relevant selected specs', () => {
  const repo = fakeRepo({ 'e2e/a.spec.ts': 'tap( on the bottom sheet', 'e2e/b.spec.ts': 'desktop table' });
  const v = pr([change('e2e/a.spec.ts', ['x']), change('e2e/b.spec.ts', ['x'])], repo);
  assert.ok(v.e2e.ios_specs.includes('a.spec.ts'));
  assert.ok(!v.e2e.ios_specs.includes('b.spec.ts'));
  assert.ok(v.e2e.ios_specs.includes('lifecycle/board-header.spec.ts'), 'smoke spec mentioning a phone');
});

test('shard count follows planned seconds, capped by units and by 16', () => {
  const durations = {};
  const files = { 'e2e/big.spec.ts': 'x' };
  for (const p of ['desktop', 'mobile', 'ios-safari']) durations[`${p}|big.spec.ts`] = 100;
  const v = pr([change('e2e/big.spec.ts', ['x'])], fakeRepo(files, durations));
  // big on desktop+mobile, 5 smoke specs on desktop+mobile, board-header (a phone) on ios:
  // 13 units at 100s (unmeasured units take the median) = 1300s -> ceil(1300/240) = 6 shards.
  assert.equal(v.e2e.planned_units, 13);
  assert.equal(v.e2e.shards, 6, JSON.stringify(v.e2e));
  assert.equal(v.e2e.shards, Math.min(FULL_SHARDS, v.e2e.planned_units, Math.ceil(v.e2e.planned_seconds / SECONDS_PER_SHARD)));
  assert.deepEqual(JSON.parse(outputs(v).e2e_shards).length, v.e2e.shards);
});

test('rust targeted: tests that read a changed non-Rust file, by include_str!, by path literal and by tree guard', () => {
  const repo = fakeRepo({
    'crates/amux-server/src/api/session_verbs.rs': 'const APP: &str = include_str!("../../../amux-dashboard/static/app.js");',
    'crates/amux-server/tests/dashboard_assets.rs': 'Path::new(env!("CARGO_MANIFEST_DIR")).join("../amux-dashboard/static")',
    'crates/amux-server/tests/unrelated.rs': '"something/else.txt"',
  });
  const v = pr([change('crates/amux-dashboard/static/app.js', ['let y = 2;'])], repo);
  assert.equal(v.rust.scope, 'targeted');
  assert.match(v.rust.filter, /test\(\/\^api::session_verbs::\/\)/);
  assert.match(v.rust.filter, /binary\(=dashboard_assets\)/);
  assert.doesNotMatch(v.rust.filter, /unrelated/);
  assert.deepEqual(JSON.parse(outputs(v).rust_shards), [1]);

  // include_str! of a root file with no extension: only the include resolver can see it.
  const cli = pr([change('amux', ['echo'])], fakeRepo({ 'crates/amux-server/src/cli_refs.rs': 'const CLI: &str = include_str!("../../../amux");' }));
  assert.match(cli.rust.filter, /test\(\/\^cli_refs::\/\)/);
  // A path literal in a test names the file it reads.
  const lit = pr([change('scripts/x.sh', ['echo hi'])], fakeRepo({ 'crates/amux-server/tests/script_reader.rs': 'root.join("scripts/x.sh")' }));
  assert.match(lit.rust.filter, /binary\(=script_reader\)/);

  // Built from parts so legacy_port_guard does not flag this fixture itself.
  const port = pr([change('scripts/x.sh', ['curl https://localhost:' + '88' + '22/'])], fakeRepo({}));
  assert.match(port.rust.filter, /binary\(=legacy_port_guard\)/);
  const noPort = pr([change('scripts/x.sh', ['echo hi'])], fakeRepo({}));
  assert.equal(noPort.rust.scope, 'none');
});

test('rustClause maps files to nextest clauses', () => {
  const id = (d) => d;
  assert.equal(rustClause('crates/amux-server/src/api/mod.rs', id), '(package(=amux-server) & test(/^api::/))');
  assert.equal(rustClause('crates/amux-server/src/lib.rs', id), '(package(=amux-server))');
  assert.equal(rustClause('crates/amux-server/tests/x.rs', id), '(package(=amux-server) & binary(=x))');
});

test('parseDiff reads paths, deletions and hunk ranges from git diff -U0', () => {
  const text = [
    'diff --git a/e2e/a.spec.ts b/e2e/a.spec.ts', 'index 1..2 100644', '--- a/e2e/a.spec.ts', '+++ b/e2e/a.spec.ts',
    '@@ -3,0 +4,2 @@ ctx', '+one', '+two', '@@ -9 +10,0 @@', '-gone',
    'diff --git a/old.md b/old.md', 'deleted file mode 100644', '--- a/old.md', '+++ /dev/null', '@@ -1 +0,0 @@', '-x',
  ].join('\n');
  const [a, b] = parseDiff(text);
  assert.equal(a.path, 'e2e/a.spec.ts');
  assert.deepEqual(a.hunks.map(h => [h.newStart, h.newCount, h.added.length, h.removed.length]), [[4, 2, 2, 0], [10, 0, 0, 1]]);
  assert.equal(b.path, 'old.md');
  assert.equal(b.deleted, true);
});

// ---- the tables against the real tree ----
const real = gitRepo(ROOT, espree);

test('every tree guard names a real test binary, and every read_dir test is classified', () => {
  const dir = path.join(ROOT, 'crates/amux-server/tests');
  for (const bin of Object.keys(TREE_GUARDS)) assert.ok(fs.existsSync(path.join(dir, bin + '.rs')), `TREE_GUARDS.${bin} has no test file`);
  const walkers = fs.readdirSync(dir).filter(f => f.endsWith('.rs') && /read_dir|WalkDir/.test(fs.readFileSync(path.join(dir, f), 'utf8')))
    .map(f => f.replace(/\.rs$/, ''));
  const unclassified = walkers.filter(w => !(w in TREE_GUARDS) && !RUST_ONLY_WALKERS.includes(w));
  assert.deepEqual(unclassified, [], 'a test walks the tree: add it to TREE_GUARDS (it reads non-Rust files) or RUST_ONLY_WALKERS');
});

test('the smoke set exists and is measured', () => {
  const specs = new Set(real.specs());
  for (const s of SMOKE) {
    assert.ok(specs.has(s), `smoke spec ${s} missing`);
    assert.ok(`desktop|${s}` in real.durations, `smoke spec ${s} has no measured duration`);
  }
});

test('every e2e infrastructure pattern matches a real file', () => {
  const tracked = fs.readdirSync(ROOT, { recursive: true }).map(String).filter(p => !p.includes('node_modules'));
  for (const r of E2E_INFRA) assert.ok(tracked.some(p => r.test(p.split(path.sep).join('/'))), `E2E_INFRA ${r} matches nothing`);
});
