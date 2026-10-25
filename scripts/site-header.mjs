#!/usr/bin/env node
// One header for every amux.io page. scripts/site-header.html is the source;
// /nav.css styles it; /site.js runs the theme picker and must load in <head>
// so the theme is set before first paint.
//
//   node scripts/site-header.mjs          report pages that drift (exit 1 if any)
//   node scripts/site-header.mjs --fix    rewrite them from the source
//
// The homepage is the one allowed variant: its Get started link is the
// in-page #get, and it renders the star count inside .nav-stars.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const SITE = path.join(ROOT, 'site');
export const CANONICAL = fs.readFileSync(path.join(ROOT, 'scripts/site-header.html'), 'utf8').trim();
const HEADER_RE = /<header class="site-header">[\s\S]*?<\/header>|<header class="site">[\s\S]*?<\/header>/;
const FONT = '<link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;600;700&display=swap" rel="stylesheet">';

export function expectedHeader(rel) {
  if (rel !== 'index.html') return CANONICAL;
  return CANONICAL
    .replace('href="/#get"', 'href="#get"')
    .replace('<span class="gh-stars" id="gh-stars">★</span>', '<span class="nav-stars">★ <span class="gh-stars">539</span></span>');
}

export function pages(dir = SITE) {
  const out = [];
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) out.push(...pages(p));
    else if (e.name === 'index.html') out.push(path.relative(SITE, p));
  }
  return out.sort();
}

const norm = s => s.replace(/\s+/g, ' ').trim();
const starsOf = s => (s.match(/<span class="gh-stars">(\d+)<\/span>/) || [])[1];

export function problems(rel, html) {
  const out = [];
  const m = html.match(HEADER_RE);
  const want = expectedHeader(rel);
  if (!m) out.push('no site header');
  else {
    // the homepage's static star count is a fallback the page script updates
    const got = rel === 'index.html' && starsOf(m[0]) ? m[0].replace(starsOf(m[0]), '539') : m[0];
    if (norm(got) !== norm(want)) out.push('header differs from scripts/site-header.html');
  }
  const head = html.split('</head>')[0];
  if (!head.includes('href="/nav.css"')) out.push('missing /nav.css in <head>');
  if (!head.includes('src="/site.js"')) out.push('/site.js not loaded in <head>');
  if (/<html[^>]*\sdata-theme=/.test(html)) out.push('<html> hardcodes data-theme (site.js decides)');
  return out;
}

export function fix(rel, html) {
  const m = html.match(HEADER_RE);
  if (m) html = html.replace(m[0], expectedHeader(rel));
  else html = html.replace(/<body([^>]*)>/, `<body$1>\n  ${expectedHeader(rel)}`);
  html = html.replace(/(<html[^>]*?)\sdata-theme="[^"]*"/, '$1');
  html = html.replace(/[ \t]*<script src="\/site\.js"><\/script>\n?/g, '');
  const add = [];
  if (!html.split('</head>')[0].includes('JetBrains+Mono')) add.push(FONT);
  add.push('<script src="/site.js"></script>');
  if (!html.split('</head>')[0].includes('href="/nav.css"')) add.push('<link rel="stylesheet" href="/nav.css">');
  return html.replace('</head>', add.map(l => '  ' + l).join('\n') + '\n</head>');
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const doFix = process.argv.includes('--fix');
  let bad = 0;
  for (const rel of pages()) {
    const file = path.join(SITE, rel);
    let html = fs.readFileSync(file, 'utf8');
    const p = problems(rel, html);
    if (!p.length) continue;
    bad++;
    if (doFix) {
      html = fix(rel, html);
      fs.writeFileSync(file, html);
      const left = problems(rel, html);
      console.log(`${left.length ? 'STILL BAD' : 'fixed'}  ${rel}  (${p.join('; ')})${left.length ? ' -> ' + left.join('; ') : ''}`);
    } else console.log(`${rel}: ${p.join('; ')}`);
  }
  const total = pages().length;
  console.log(`${bad} of ${total} pages ${doFix ? 'rewritten' : 'drift from the standard header'}`);
  if (!doFix && bad) { console.log('fix: node scripts/site-header.mjs --fix'); process.exit(1); }
}
