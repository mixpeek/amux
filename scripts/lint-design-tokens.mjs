#!/usr/bin/env node
// The design-token lint (Ethan, 2026-10-26: "no hardcoded visual styles remain
// outside the token system"). Colors, font sizes, radii and font stacks are
// written once, in the token blocks, and everything else reads var(--token).
//
//   node scripts/lint-design-tokens.mjs          # report, exit 1 on any finding
//
// Where a literal is allowed:
// - app.css between "── Design tokens ──" and "── end design tokens ──";
// - app.js between "── design tokens: theme palettes ──" and its end marker
//   (the built-in themes are the token values themselves);
// - a line carrying "design-tokens: allow (<reason>)", for a value that must be
//   literal: a hex-only consumer (Excel, the drawing editor, <input
//   type=color>, the browser's theme-color) or stored data (the brand color).
// Comments, HTML entities (&#128190;) and the separately built business/
// bundle are not styles.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const STATIC = path.join(ROOT, 'crates/amux-dashboard/static');
export const FILES = ['app.css', 'app.js', 'index.html', 'state/feedback.css'];

const COLOR = /(?<![&\w])#(?:[0-9a-fA-F]{8}|[0-9a-fA-F]{6}|[0-9a-fA-F]{3,4})(?![\w-])|\b(?:rgba?|hsla?)\((?=\s*(?:[\d.]|\$\{|var\())/g;
const FONT_SIZE = /font-size\s*:\s*([^;"'`}]+)/g;
const RADIUS = /border(?:-(?:top|bottom)-(?:left|right))?-radius\s*:\s*([^;"'`}]+)/g;
const FAMILY = /font-family\s*:\s*([^;"'`}]+)/g;
const SPACING = /(?<![\w-])(?:padding|margin)(?:-(?:top|bottom|left|right|inline|block)(?:-(?:start|end))?)?\s*:\s*([^;"'`}]+)|(?<![\w-])(?:row-|column-)?gap\s*:\s*([^;"'`}]+)/g;
const VAR_USE = /var\(\s*(--[\w-]+)\s*(,)?/g;
const ALLOW = /design-tokens: allow \(/;

function blank(text, from, to) {
  return text.slice(0, from) + text.slice(from, to).replace(/[^\n]/g, ' ') + text.slice(to);
}
// Comments become spaces, so offsets and line numbers survive.
function stripComments(text, kind) {
  let out = text;
  for (const m of text.matchAll(/\/\*[\s\S]*?\*\//g)) out = blank(out, m.index, m.index + m[0].length);
  if (kind === 'html') for (const m of text.matchAll(/<!--[\s\S]*?-->/g)) out = blank(out, m.index, m.index + m[0].length);
  if (kind !== 'css') {
    // Whole-line and trailing // comments, outside obvious strings.
    out = out.replace(/(^|[^:'"`\\])\/\/[^\n]*/g, (m, pre) => pre + ' '.repeat(m.length - pre.length));
  }
  return out;
}
function regions(text, begin, end) {
  const out = [];
  let i = 0;
  while ((i = text.indexOf(begin, i)) >= 0) {
    const j = text.indexOf(end, i);
    if (j < 0) break;
    out.push([i, j + end.length]);
    i = j + end.length;
  }
  return out;
}
function lineOf(text, i) { return text.slice(0, i).split('\n').length; }
function lineText(text, i) { return text.slice(text.lastIndexOf('\n', i) + 1, (text.indexOf('\n', i) + 1 || text.length + 1) - 1); }

export function lint(read = f => fs.readFileSync(path.join(STATIC, f), 'utf8')) {
  const findings = [];
  const sources = Object.fromEntries(FILES.map(f => [f, read(f)]));
  // Every custom property anything defines (a token, or a local one a rule sets).
  const defined = new Set();
  for (const text of Object.values(sources)) {
    for (const m of text.matchAll(/(--[\w-]+)\s*:/g)) defined.add(m[1]);
    for (const m of text.matchAll(/setProperty\(\s*['"`](--[\w-]+)['"`]/g)) defined.add(m[1]);
    for (const m of text.matchAll(/\[\s*'\w+'\s*,\s*'(--[\w-]+)'/g)) defined.add(m[1]);
  }
  for (const [file, raw] of Object.entries(sources)) {
    const kind = file.endsWith('.css') ? 'css' : file.endsWith('.html') ? 'html' : 'js';
    const text = stripComments(raw, kind);
    const exempt = [
      ...regions(raw, '── Design tokens ──', '── end design tokens ──'),
      ...regions(raw, '── design tokens: theme palettes ──', '── end theme palettes ──'),
    ];
    const allowed = i => exempt.some(([a, b]) => i >= a && i < b) || ALLOW.test(lineText(raw, i));
    const report = (rule, i, value) => findings.push({ rule, file, line: lineOf(raw, i), value: value.trim().slice(0, 60), text: lineText(raw, i).trim().slice(0, 140) });
    for (const m of text.matchAll(COLOR)) if (!allowed(m.index)) report('color-literal', m.index, m[0]);
    for (const m of text.matchAll(FONT_SIZE)) {
      const v = m[1].trim();
      if (/^(var\(--(fs-[\w-]+|control-font)\)|0|inherit|initial|unset|[\d.]+(em|%)|calc\([^)]*var\(--fs)/.test(v) || /\$\{/.test(v) || v === '') continue;
      if (!allowed(m.index)) report('font-size', m.index, v);
    }
    for (const m of text.matchAll(RADIUS)) {
      const v = m[1].replace('!important', '').trim();
      if (v.split(/\s+/).every(p => /^(0|0px|1px|var\(--radius-[\w-]+\)|inherit|\$\{.*|[\d.]+%)$/.test(p) && p !== '50%' || p === 'var(--radius-round)')) continue;
      if (!allowed(m.index)) report('radius', m.index, v);
    }
    for (const m of text.matchAll(SPACING)) {
      const v = (m[1] || m[2] || '').replace('!important', '');
      // 0, a 1px hairline and very large layout offsets are not spacing steps.
      const bad = [...v.matchAll(/(?<![\w.#-])-?(\d+(?:\.\d+)?)px\b/g)].filter(x => +x[1] > 1 && +x[1] <= 96);
      if (bad.length && !allowed(m.index)) report('spacing', m.index, v);
    }
    for (const m of text.matchAll(FAMILY)) {
      const v = m[1].replace('!important', '').trim();
      if (/@font-face\s*\{[^}]*$/.test(text.slice(Math.max(0, m.index - 200), m.index))) continue;   // naming a font file
      if (/^(var\(--font-(ui|mono|sys)\)|inherit)$/.test(v) || /\$\{/.test(v)) continue;
      if (!allowed(m.index)) report('font-family', m.index, v);
    }
    // A layout value published at runtime carries its own fallback (var(--x, 0px));
    // a token read with no fallback must be defined somewhere.
    for (const m of text.matchAll(VAR_USE)) if (!m[2] && !defined.has(m[1]) && !allowed(m.index)) report('undefined-token', m.index, m[1]);
  }
  return findings;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const found = lint();
  const by = {};
  for (const f of found) (by[f.rule] ||= []).push(f);
  for (const [rule, list] of Object.entries(by)) {
    console.log(`${rule}: ${list.length}`);
    for (const f of list.slice(0, Number(process.env.SHOW || 25))) console.log(`  ${f.file}:${f.line}  ${f.value}   | ${f.text}`);
  }
  console.log(found.length ? `design tokens: ${found.length} finding(s) in ${FILES.length} files` : `design tokens: clean (${FILES.join(', ')})`);
  process.exit(found.length ? 1 : 0);
}
