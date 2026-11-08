#!/usr/bin/env node
// One header and one footer for every amux.io page. scripts/site-header.html
// and scripts/site-footer.html are the sources; /nav.css styles both; /site.js
// runs the theme picker and must load in <head> so the theme is set before
// first paint. (The footer joined 2026-11-08: "Make sure the header and footer
// is the same for every page"; 426 pages carried 89 different footers.)
//
//   node scripts/site-header.mjs          report pages that drift (exit 1 if any)
//   node scripts/site-header.mjs --fix    rewrite them from the source
//
// /nav.css and /site.js are referenced with ?v=<scripts/site-asset-version.txt>.
// The CDN in front of amux.io keeps those two files for hours, so a header that
// needs new CSS or JS would otherwise reach visitors before its styles do. Bump
// the version whenever nav.css or site.js changes, then run --fix.
//
// The homepage is the one allowed variant: it renders the star count inside
// .nav-stars. Pages in NO_GET_STARTED drop the Get started link and keep the
// rest of the header.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const SITE = path.join(ROOT, 'site');
export const CANONICAL = fs.readFileSync(path.join(ROOT, 'scripts/site-header.html'), 'utf8').trim();
export const FOOTER = fs.readFileSync(path.join(ROOT, 'scripts/site-footer.html'), 'utf8').trim();
// Each page has exactly one <footer>, the site's own.
const FOOTER_RE = /<footer[\s>][\s\S]*?<\/footer>/;
const HEADER_RE = /<header class="site-header">[\s\S]*?<\/header>|<header class="site">[\s\S]*?<\/header>/;
export const ASSET_V = fs.readFileSync(path.join(ROOT, 'scripts/site-asset-version.txt'), 'utf8').trim();
// A page that alone depends on a newer nav.css or site.js moves first, so the
// rest of the site is not rewritten for it. The higher version wins.
export const PAGE_ASSET_V = new Map([['concierge/index.html', '20261108']]);
const assetV = rel => { const v = PAGE_ASSET_V.get(rel); return v && v > ASSET_V ? v : ASSET_V; };
const navCss = rel => `<link rel="stylesheet" href="/nav.css?v=${assetV(rel)}">`;
const siteJs = rel => `<script src="/site.js?v=${assetV(rel)}"></script>`;
const FONT = '<link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;600;700&display=swap" rel="stylesheet">';

// Pages whose header has no Get started link, each for a reason of its own.
export const NO_GET_STARTED = new Map([
  ['index.html', "the hero's own buttons cover it (homepage redesign, 2026-11-03)"],
  ['start/index.html', 'no exits above the signup form (EXP-157)'],
]);
const GET_STARTED = /\n\s*<a href="\/#get" class="nav-cta nav-cta-primary">Get started<\/a>/;

// The homepage header's star count as baked into the page, before site.js
// replaces it with GitHub's live count. Keep equal to site.js STARS_FALLBACK.
export const STARS = '555';

export function expectedHeader(rel) {
  const base = NO_GET_STARTED.has(rel) ? CANONICAL.replace(GET_STARTED, '') : CANONICAL;
  if (rel !== 'index.html') return base;
  return base
    .replace('<span class="gh-stars" id="gh-stars">★</span>', `<span class="nav-stars">★ <span class="gh-stars">${STARS}</span></span>`);
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
    const got = rel === 'index.html' && starsOf(m[0]) ? m[0].replace(starsOf(m[0]), STARS) : m[0];
    if (norm(got) !== norm(want)) out.push('header differs from scripts/site-header.html');
  }
  const foot = html.match(FOOTER_RE);
  if (!foot) out.push('no site footer');
  else if (norm(foot[0]) !== norm(FOOTER)) out.push('footer differs from scripts/site-footer.html');
  const head = html.split('</head>')[0];
  if (!head.includes('href="/nav.css')) out.push('missing /nav.css in <head>');
  else if (!head.includes(navCss(rel))) out.push(`/nav.css not at ?v=${assetV(rel)}`);
  if (!head.includes('src="/site.js')) out.push('/site.js not loaded in <head>');
  else if (!head.includes(siteJs(rel))) out.push(`/site.js not at ?v=${assetV(rel)}`);
  if (/<html[^>]*\sdata-theme=/.test(html)) out.push('<html> hardcodes data-theme (site.js decides)');
  return out;
}

export function fix(rel, html) {
  const m = html.match(HEADER_RE);
  if (m) html = html.replace(m[0], expectedHeader(rel));
  else html = html.replace(/<body([^>]*)>/, `<body$1>\n  ${expectedHeader(rel)}`);
  const foot = html.match(FOOTER_RE);
  if (foot) html = html.replace(foot[0], FOOTER);
  else html = html.replace('</body>', `${FOOTER}\n</body>`);
  html = html.replace(/(<html[^>]*?)\sdata-theme="[^"]*"/, '$1');
  html = html.replace(/[ \t]*<script src="\/site\.js(\?v=[^"]*)?"><\/script>\n?/g, '');
  html = html.replace(/<link rel="stylesheet" href="\/nav\.css(\?v=[^"]*)?">/g, navCss(rel));
  const add = [];
  if (!html.split('</head>')[0].includes('JetBrains+Mono')) add.push(FONT);
  add.push(siteJs(rel));
  if (!html.split('</head>')[0].includes('href="/nav.css')) add.push(navCss(rel));
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
  console.log(`${bad} of ${total} pages ${doFix ? 'rewritten' : 'drift from the standard header or footer'}`);
  if (!doFix && bad) { console.log('fix: node scripts/site-header.mjs --fix'); process.exit(1); }
}
