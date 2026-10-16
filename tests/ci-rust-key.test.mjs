// scripts/ci-rust-key.mjs keys rust.yml's server-bin and nextest-archive
// caches. A key that stays put when a compile input moved hands a PR a stale
// binary and a false green, so every input class is pinned here in both
// directions, and the real repository is checked for includes the key cannot see.
import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  rustKey, rustKeyInputs, findIncludes, isRustInput, gitRustRepo, EXCLUDED_PREFIX,
} from '../scripts/ci-rust-key.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const RUSTC = 'rustc 1.97.1 (8bab26f4f 2026-07-14)';

// files: { path: content }. The blob id is a hash of the content, as git's is.
function fakeRepo(files) {
  const blob = (s) => [...s].reduce((h, c) => (h * 31 + c.charCodeAt(0)) >>> 0, 7).toString(16).padStart(8, '0');
  const lsFiles = Object.keys(files).sort().map(p => `100644 ${blob(files[p])} 0\t${p}`).join('\n') + '\n';
  return { lsFiles, read: (p) => (p in files ? files[p] : null) };
}

const BASE = {
  'Cargo.toml': '[workspace]', 'Cargo.lock': 'lock v1', 'rust-toolchain.toml': 'channel = "1.97.1"',
  '.github/workflows/rust.yml': 'name: rust',
  'crates/amux-server/Cargo.toml': '[package]\nname = "amux-server"',
  'crates/amux-server/src/lib.rs':
    'const CLI: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../amux"));\n' +
    'const DOC: &str = include_str!("../../../docs/guide.md");\n' +
    '// include_str!("../../../not-a-real-include.md") in a comment is prose\n' +
    'const S: &str = "include_str!(\\"../../../in-a-string.md\\")";\n',
  'crates/amux-dashboard/Cargo.toml': '[package]\nname = "amux-dashboard"',
  'crates/amux-dashboard/src/lib.rs': '#[folder = "static/"] struct A;',
  'crates/amux-dashboard/static/app.js': 'const APP_VER = "__AMUX_ASSET_VER__";',
  'crates/amux-dashboard/static/sw.js': 'self.x = 1;',
  'e2e/boot.spec.ts': 'test()',
  'amux': '#!/bin/bash\necho v1',
  'docs/guide.md': '# guide v1',
  'README.md': 'readme',
};
const key = (files, rustc = RUSTC) => rustKey(rustKeyInputs(fakeRepo(files)), rustc);
const base = key(BASE);

test('the key is a sha256 and is stable for the same inputs', () => {
  assert.match(base, /^[0-9a-f]{64}$/);
  assert.equal(key({ ...BASE }), base);
});

test('a dashboard static/ change keeps the key (the PR serves it through AMUX_DASHBOARD_DIR)', () => {
  assert.equal(key({ ...BASE, 'crates/amux-dashboard/static/app.js': 'const APP_VER = "__AMUX_ASSET_VER__"; // changed' }), base);
  assert.equal(key({ ...BASE, 'crates/amux-dashboard/static/new.css': '.x{}' }), base);
});

test('an e2e, docs or unrelated change keeps the key', () => {
  assert.equal(key({ ...BASE, 'e2e/boot.spec.ts': 'test() // changed' }), base);
  assert.equal(key({ ...BASE, 'e2e/new.spec.ts': 'test()' }), base);
  assert.equal(key({ ...BASE, 'README.md': 'readme v2' }), base);
});

test('a .rs change moves the key', () => {
  assert.notEqual(key({ ...BASE, 'crates/amux-dashboard/src/lib.rs': '#[folder = "static/"] struct B;' }), base);
  assert.notEqual(key({ ...BASE, 'crates/amux-server/src/new.rs': 'fn x() {}' }), base);
});

test('Cargo.lock, Cargo.toml, the toolchain file and rust.yml move the key', () => {
  for (const p of ['Cargo.lock', 'Cargo.toml', 'rust-toolchain.toml', '.github/workflows/rust.yml', 'crates/amux-server/Cargo.toml']) {
    assert.notEqual(key({ ...BASE, [p]: BASE[p] + ' changed' }), base, p);
  }
});

test('a non-Rust file inside a crate (fixture, migration, build input) moves the key', () => {
  assert.notEqual(key({ ...BASE, 'crates/amux-server/migrations/0001.sql': 'create table x();' }), base);
});

test('an include_str! target outside crates/ moves the key, in both path forms', () => {
  assert.notEqual(key({ ...BASE, 'amux': '#!/bin/bash\necho v2' }), base, 'concat!(env!("CARGO_MANIFEST_DIR"), ...) form');
  assert.notEqual(key({ ...BASE, 'docs/guide.md': '# guide v2' }), base, 'file-relative form');
});

test('include_str! in a comment or a string literal is not an include', () => {
  const inputs = rustKeyInputs(fakeRepo(BASE));
  const targets = inputs.includes.map(i => i.path).sort();
  assert.deepEqual(targets, ['amux', 'docs/guide.md']);
});

test('the rustc version moves the key', () => {
  assert.notEqual(key(BASE, 'rustc 1.98.0 (aaaaaaaaa 2026-09-01)'), base);
});

test('an include the key cannot resolve yields NO key (fail closed: no cache)', () => {
  const files = { ...BASE, 'crates/amux-server/src/m.rs': 'macro_rules! m { ($p:expr) => { include_str!($p) } }' };
  const inputs = rustKeyInputs(fakeRepo(files));
  assert.equal(inputs.unresolved.length, 1);
  assert.equal(rustKey(inputs, RUSTC), null);
  const viaConst = { ...BASE, 'crates/amux-server/src/c.rs': 'const P: &str = "x"; const X: &str = include_str!(P);' };
  assert.equal(rustKey(rustKeyInputs(fakeRepo(viaConst)), RUSTC), null);
});

test('findIncludes parses multi-line concat! and raw strings', () => {
  const got = findIncludes('const A: &str = include_str!(concat!(\n  env!("CARGO_MANIFEST_DIR"),\n  "/../../scripts/hooks/x.py"\n));\nconst B: &[u8] = include_bytes!(r"data/b.bin");');
  assert.deepEqual(got, [
    { kind: 'path', rel: '/../../scripts/hooks/x.py', base: 'manifest' },
    { kind: 'path', rel: 'data/b.bin', base: 'file' },
  ]);
  assert.deepEqual(findIncludes('include_str!(concat!(env!("OUT_DIR"), "/gen.rs"))').map(i => i.kind), ['generated']);
});

test('the input predicate: crates/ minus static/, plus the workspace files', () => {
  assert.equal(isRustInput('crates/amux-server/src/lib.rs'), true);
  assert.equal(isRustInput(EXCLUDED_PREFIX + 'app.js'), false);
  assert.equal(isRustInput('e2e/x.spec.ts'), false);
  assert.equal(isRustInput('Cargo.lock'), true);
});

// ---- the real repository ----
test('real repo: every include resolves, none reads the dashboard static/, and a key exists', () => {
  const inputs = rustKeyInputs(gitRustRepo(ROOT));
  assert.deepEqual(inputs.unresolved, [], 'an include the key cannot see would make every cache hit a guess');
  const intoStatic = inputs.includes.filter(i => i.path.startsWith(EXCLUDED_PREFIX));
  assert.deepEqual(intoStatic, [],
    'a compile-time include of the dashboard static/ makes test binaries depend on it; read it at run time instead');
  const untracked = inputs.includes.filter(i => i.blob === 'untracked');
  assert.deepEqual(untracked, [], 'an include target that git does not track cannot be keyed');
  assert.ok(inputs.includes.length > 50, `expected the repo's many include_str! targets, saw ${inputs.includes.length}`);
  assert.match(rustKey(inputs, RUSTC), /^[0-9a-f]{64}$/);
});
