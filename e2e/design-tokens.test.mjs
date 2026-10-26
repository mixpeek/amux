// The design system's contract (Ethan, 2026-10-26): every visual value comes
// from a token, and themes are versioned JSON that validate before they apply.
// Runs in CI with the other e2e/*.test.mjs (rust.yml, "Dashboard client unit tests").
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import { lint } from '../scripts/lint-design-tokens.mjs';

const STATIC = 'crates/amux-dashboard/static/';
const source = fs.readFileSync(STATIC + 'app.js', 'utf8');

test('no visual literal outside the token system', () => {
  const found = lint();
  assert.deepEqual(found.map(f => `${f.rule} ${f.file}:${f.line} ${f.value}`), []);
});

test('every @font-face names its font (a token there would define no font)', () => {
  const css = fs.readFileSync(STATIC + 'app.css', 'utf8');
  const faces = [...css.matchAll(/@font-face\s*\{[^}]*font-family:\s*([^;]+);/g)].map(m => m[1].trim());
  assert.ok(faces.length >= 3, 'the bundled fonts are declared');
  for (const f of faces) assert.match(f, /^"[^"]+"$/, `@font-face font-family ${f}`);
});

test('the lint finds each kind of literal it exists to stop', () => {
  // Each rule must be able to fail: plant one offender per rule in a copy.
  const plant = {
    'color-literal': '.x{color:#123456}',
    'font-size': '.x{font-size:13px}',
    radius: '.x{border-radius:7px}',
    'font-family': '.x{font-family:Arial, sans-serif}',
    spacing: '.x{padding:7px}',
    'undefined-token': '.x{color:var(--not-a-token)}',
  };
  for (const [rule, css] of Object.entries(plant)) {
    const found = lint(f => fs.readFileSync(STATIC + f, 'utf8') + (f === 'app.css' ? '\n' + css + '\n' : ''));
    assert.ok(found.some(x => x.rule === rule), `the ${rule} rule did not fire on ${css}`);
  }
  // An allow note on the line exempts it, and only it.
  const allowed = lint(f => fs.readFileSync(STATIC + f, 'utf8') + (f === 'app.css' ? '\n.x{color:#123456} /* design-tokens: allow (test) */\n' : ''));
  assert.deepEqual(allowed, []);
});

// The theme engine, run as shipped: the section of app.js between its markers.
function engine() {
  const a = source.indexOf('// ── Theme ──');
  const b = source.indexOf('// The highlight.js theme is ALWAYS the dark one');
  assert.ok(a > 0 && b > a, 'theme section markers moved');
  const store = new Map();
  const ctx = vm.createContext({
    localStorage: { getItem: k => store.get(k) ?? null, setItem: (k, v) => store.set(k, String(v)), removeItem: k => store.delete(k) },
    document: { body: { classList: { toggle() {}, contains: () => false }, dataset: {} }, head: { appendChild() {} },
      getElementById: () => null, querySelector: () => null, createElement: () => ({}) },
    window: { matchMedia: () => ({ matches: false }), addEventListener() {} },
    getComputedStyle: () => ({ getPropertyValue: () => '' }),
    fetch: async () => ({ ok: false }), _hljsApplyTheme() {}, _themeSelectFill() {}, esc: s => String(s), console,
  });
  vm.runInContext(source.slice(a, b) + '\n;globalThis.__t = { BUILTIN_THEMES, THEME_TOKENS, themeValidate, themeExport, themeSaveCustom, themeById, themeCss };', ctx);
  return { ...ctx.__t, store };
}

test('every built-in theme validates, with the five families asked for', () => {
  const { BUILTIN_THEMES, themeValidate } = engine();
  const names = BUILTIN_THEMES.map(t => t.name).join(' | ');
  for (const family of ['GitHub', 'One Dark Pro', 'Dracula', 'Atom One', 'Ayu']) assert.match(names, new RegExp(family), names);
  for (const t of BUILTIN_THEMES) {
    const v = themeValidate(t);
    assert.ok(v.ok, `${t.name}: ${v.errors.join(' ')}`);
    // Built-ins are the examples: no warnings either. The one exception is
    // GitHub's own dark palette (white on its #3fb950 green), kept as amux has
    // always looked.
    const known = ['amux-dark', 'github-dark'].includes(t.id) ? v.warnings.filter(w => !/green fill/.test(w)) : v.warnings;
    assert.equal(known.length, 0, `${t.name} warnings: ${known.join(" ")}`);
    assert.equal(new Set(BUILTIN_THEMES.map(x => x.id)).size, BUILTIN_THEMES.length, 'theme ids are unique');
  }
});

test('a theme fills every token, so no surface is left on another theme', () => {
  const { BUILTIN_THEMES, THEME_TOKENS, themeValidate, themeCss } = engine();
  for (const t of BUILTIN_THEMES) {
    const css = themeCss(t);
    for (const [, prop] of THEME_TOKENS) assert.match(css, new RegExp(prop + ':#[0-9a-f]{6}'), `${t.name} sets ${prop}`);
  }
  // Every color token app.css defines is one a theme can set.
  const root = fs.readFileSync(STATIC + 'app.css', 'utf8').match(/:root \{([\s\S]*?)\n  \}/)[1];
  const colorTokens = [...root.matchAll(/(--[\w-]+):\s*#/g)].map(m => m[1]);
  const themable = new Set(THEME_TOKENS.map(t => t[1]));
  assert.deepEqual(colorTokens.filter(t => !themable.has(t)), [], 'a color token no theme can set');
});

test('validation refuses what would break the UI, before it applies', () => {
  const { themeValidate } = engine();
  const ok = { schema: 'amux-theme', version: 1, name: 'T', base: 'dark', colors: { bg: '#101010', card: '#181818', border: '#333333', text: '#eeeeee', dim: '#999999', accent: '#55aaff' } };
  assert.ok(themeValidate(ok).ok);
  const bad = (patch, msg) => { const v = themeValidate({ ...ok, ...patch }); assert.ok(!v.ok && v.errors.some(e => msg.test(e)), JSON.stringify(v.errors)); };
  bad({ schema: 'vscode' }, /schema/);
  bad({ version: 2 }, /version/);
  bad({ base: 'sepia' }, /base/);
  bad({ name: '' }, /name/);
  bad({ colors: { ...ok.colors, accent: 'blue' } }, /hex color/);
  bad({ colors: { ...ok.colors, accent: undefined } }, /accent/);
  bad({ colors: { ...ok.colors, text: '#151515' } }, /contrast/);
  assert.ok(themeValidate({ ...ok, colors: { ...ok.colors, mystery: '#000000' } }).warnings.some(w => /mystery/.test(w)));
});

test('export, then import of that file, is the same theme', () => {
  const { themeExport, themeValidate, themeSaveCustom, themeById } = engine();
  const text = themeExport('dracula');
  const doc = JSON.parse(text);
  assert.equal(doc.schema, 'amux-theme');
  assert.equal(doc.version, 1);
  assert.equal(doc.builtin, undefined, 'export carries no app-internal flags');
  const v = themeValidate({ ...doc, id: 'dracula-copy', name: 'Dracula copy' });
  assert.ok(v.ok);
  const saved = themeSaveCustom({ ...doc, id: 'dracula-copy', name: 'Dracula copy' });
  assert.ok(saved.ok);
  assert.deepEqual(themeById('dracula-copy').colors, themeById('dracula').colors);
});
