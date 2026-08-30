// AEAB-22 — file paths in session output must link to the file browser.
//
// `linkifyOutput` existed for this surface and had ZERO callers, so every path in
// every session's output rendered as dead text. Its `.file-link`/`.md-link`
// classes are styled under `.overlay-body`, which is exactly `#peek-body`'s
// class — it was written for the peek pane and never wired in.
//
// It also could not have matched the reported case: its regex required a leading
// `/` or `./`, while real output says "Contacts are in
// customers/rothco/data/jewishlink-prospects.csv." — a BARE relative path, which
// is how a worker naturally writes a path inside its own cwd.
//
// These load the SHIPPED functions straight out of app.js and run them, rather
// than restating the regex — a copy of the pattern would pass while the pipeline
// stayed unwired, which is the exact failure being fixed.
//
// Run: node scripts/test-linkify-paths.mjs   (exit 0 = pass). Wired into checks.yml.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// Resolve from the SCRIPT, not the cwd, so CI and a shell in any directory agree.
const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const src = fs.readFileSync(path.join(REPO, 'crates/amux-dashboard/static/app.js'), 'utf8');
// Pull the two pure functions out and run them standalone.
// Fail LEGIBLY if the function is gone, rather than throwing a stack trace from
// deep inside `new Function`. Against the pre-fix app.js this is the assertion
// that fires, and "not found in app.js" is the useful sentence to read.
const grab = n => { const i = src.indexOf('function '+n+'(');
  if (i < 0) { console.error(`FAIL: function ${n}() not found in app.js — the path linkifier is missing or renamed`); process.exit(1); }
  let d=0,j=src.indexOf('{',i);
  for(let k=j;k<src.length;k++){ if(src[k]==='{')d++; else if(src[k]==='}'){d--; if(!d){return src.slice(i,k+1);} } } };
const esc = s => String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
const escJs = s => String(s).replace(/\\/g,'\\\\').replace(/'/g,"\\'");
let peekSessionDir = '/Users/d/Projects/amux-gtm', peekSession='amux-gtm';
const fn = new Function('esc','escJs','peekSessionDir','peekSession',
  grab('_resolveOutputPath') + '\n' + grab('_linkifyPaths') + '\n return {_linkifyPaths,_resolveOutputPath};')
  (esc, escJs, peekSessionDir, peekSession);

let pass=0, fail=0;
const t=(label,cond,extra='')=>{ if(cond){pass++;} else {fail++; console.log('FAIL:',label, extra);} };

// (a) THE REPORTED SPECIMEN — a bare relative path, which the old regex could not match.
let out = fn._linkifyPaths(esc('  Contacts are in customers/rothco/data/jewishlink-prospects.csv.'));
t('(a) bare relative path is linked', out.includes('class="file-link"'), out);
t('(a) full path captured', out.includes('customers/rothco/data/jewishlink-prospects.csv</span>'), out);
t('(a) trailing sentence period left OUTSIDE the link', out.endsWith('</span>.'), out);
t('(a) resolves against the session cwd for the title',
  out.includes('/Users/d/Projects/amux-gtm/customers/rothco/data/jewishlink-prospects.csv'), out);

// (b) absolute and ./ forms still work
t('(b) absolute', fn._linkifyPaths(esc('see /Users/d/x/y.py now')).includes('file-link'));
t('(b) dot-slash', fn._linkifyPaths(esc('see ./src/main.rs now')).includes('file-link'));
t('(b) .md gets md-link', fn._linkifyPaths(esc('see docs/plan.md now')).includes('md-link'));
t('(b) :linenum kept in the label', fn._linkifyPaths(esc('at src/a/b.rs:42 here')).includes('b.rs:42</span>'));

// (c) THE CONTROLS — must NOT link.
const url = '<a href="https://h.io/a/b.js" target="_blank">https://h.io/a/b.js</a>';
t('(c) existing anchor untouched', fn._linkifyPaths(url) === url, fn._linkifyPaths(url));
t('(c) no link inside tag attributes',
  !fn._linkifyPaths('<span title="a/b.css">hi</span>').includes('file-link'),
  fn._linkifyPaths('<span title="a/b.css">hi</span>'));
t('(c) prose with a slash but no extension is not a path',
  !fn._linkifyPaths(esc('use and/or here')).includes('file-link'));
t('(c) a bare filename with no slash is not linked',
  !fn._linkifyPaths(esc('open report.csv now')).includes('file-link'));
t('(c) a version number is not a path',
  !fn._linkifyPaths(esc('bumped to 0.9.665 today')).includes('file-link'));

// (d) escaping: a quote in the path must not break the onclick
// A path containing a quote would break out of the inline onclick, so it must
// FAIL SAFE (not linked) rather than be linked unsafely.
const tricky = fn._linkifyPaths(esc("it's in a/b'c.txt ok"));
t('(d) quoted path fails safe: not linked, no raw quote injected',
  !tricky.includes('file-link'), tricky);
// And prose apostrophes must not glue onto a neighbouring path.
const prose = fn._linkifyPaths(esc("it's in data/x.csv ok"));
t('(d) prose apostrophe does not break a real path', prose.includes('data/x.csv</span>'), prose);

// (e) resolution
t('(e) absolute stays absolute', fn._resolveOutputPath('/a/b.txt') === '/a/b.txt');
t('(e) relative joins cwd', fn._resolveOutputPath('a/b.txt') === '/Users/d/Projects/amux-gtm/a/b.txt');
t('(e) ./ stripped', fn._resolveOutputPath('./a/b.txt') === '/Users/d/Projects/amux-gtm/a/b.txt');
t('(e) linenum stripped for the API', fn._resolveOutputPath('a/b.rs:42') === '/Users/d/Projects/amux-gtm/a/b.rs');

// (g) No session cwd => a relative path is NOT linked. Rendering a link we
// cannot resolve would be text that looks clickable and does nothing, which is
// the failure this file's OSC-8 comment already records. An ABSOLUTE path needs
// no cwd and must still link.
const noCwd = new Function('esc','escJs','peekSessionDir','peekSession',
  grab('_resolveOutputPath') + '\n' + grab('_linkifyPaths') + '\n return {_linkifyPaths};')
  (esc, escJs, '', '');
t('(g) relative path not linked without a session cwd',
  !noCwd._linkifyPaths(esc('see customers/rothco/x.csv now')).includes('file-link'),
  noCwd._linkifyPaths(esc('see customers/rothco/x.csv now')));
t('(g) absolute path still linked without a session cwd',
  noCwd._linkifyPaths(esc('see /Users/d/x/y.py now')).includes('file-link'));

// THE WIRING, which is the half that was actually broken before: a perfect
// linkifier that no render path calls is what shipped for months. Assert the
// peek pipeline actually runs it.
t('(f) _peekHtml exists and calls _linkifyPaths',
  /function _peekHtml\([\s\S]{0,200}?_linkifyPaths\(/.test(src),
  'the peek pipeline does not call _linkifyPaths');
t('(f) no peek call site bypasses the shared pipeline',
  !/highlightPrompts\(ansiToHtml\(/.test(src),
  'a render site still spells the chain out and so skips path linkification');

console.log(`\n_linkifyPaths: ${pass} passed, ${fail} failed`);
process.exit(fail?1:0);
