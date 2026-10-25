// Every amux.io page carries the one standard header (scripts/site-header.html),
// loads /nav.css, and runs /site.js in <head> so the theme is set before paint.
// A page rebuilt from an old copy of the header fails here, with the fix command.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { pages, problems, fix, expectedHeader, CANONICAL } from '../scripts/site-header.mjs';

const SITE = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', 'site');

test('every page has the standard header, nav.css and site.js in <head>', () => {
  const all = pages();
  assert.ok(all.length > 300, `expected the whole site, found ${all.length} pages`);
  const bad = all.map(rel => [rel, problems(rel, fs.readFileSync(path.join(SITE, rel), 'utf8'))]).filter(([, p]) => p.length);
  assert.deepEqual(bad.map(([rel, p]) => `${rel}: ${p.join('; ')}`), [], 'run: node scripts/site-header.mjs --fix');
});

test('the header has the theme picker and the two CTAs', () => {
  for (const s of ['class="theme-btn"', 'data-theme-set="system"', 'data-theme-set="light"', 'data-theme-set="dark"', 'nav-cta-primary', 'github.com/mixpeek/amux'])
    assert.ok(CANONICAL.includes(s), s);
});

test('the checker catches an old header and --fix repairs it', () => {
  const old = `<!DOCTYPE html><html lang="en" data-theme="dark"><head><link rel="stylesheet" href="/style.css"></head><body>
<header class="site-header"><a class="logo" href="/">amux</a><nav><a href="/docs/">docs</a><a href="https://apps.apple.com/x" class="nav-cta">iOS app</a></nav></header><main></main><script src="/site.js"></script></body></html>`;
  const p = problems('walkthrough/index.html', old);
  assert.ok(p.some(x => x.includes('header differs')), p.join());
  assert.ok(p.some(x => x.includes('nav.css')), p.join());
  assert.ok(p.some(x => x.includes('site.js')), p.join());
  assert.ok(p.some(x => x.includes('data-theme')), p.join());
  const fixed = fix('walkthrough/index.html', old);
  assert.deepEqual(problems('walkthrough/index.html', fixed), []);
  assert.ok(fixed.includes(expectedHeader('walkthrough/index.html')));
});
