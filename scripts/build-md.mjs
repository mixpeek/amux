// Build crates/amux-dashboard/static/vendor/md.js: the markdown stack every
// markdown surface uses (chat, file viewer, board), served by amux itself so it
// works offline (the dashboard is an offline-first PWA; these came from a CDN).
//
//   marked   15.x  GFM parser (the renderer API in app.js is marked 15's)
//   dompurify 3.x  sanitizer: all of this is model or file text
//   remend    1.x  Streamdown's healing step: closes an unterminated fence,
//                  link or emphasis in text that is still streaming
//
// Installs into a private temp dir (never the shared node_modules), bundles
// with esbuild into one IIFE exposing window.marked / DOMPurify / remend.
// Usage: node scripts/build-md.mjs
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { build } from 'esbuild';

const PKGS = { marked: '15.0.12', dompurify: '3.4.16', remend: '1.3.1' };
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-md-'));
fs.writeFileSync(path.join(tmp, 'package.json'), JSON.stringify({ private: true, dependencies: PKGS }));
execFileSync('npm', ['install', '--silent', '--no-audit', '--no-fund'], { cwd: tmp, stdio: 'inherit' });
const entry = path.join(tmp, 'entry.mjs');
fs.writeFileSync(entry, `
import { marked } from 'marked';
import DOMPurify from 'dompurify';
import * as R from 'remend';
window.marked = marked;
window.DOMPurify = DOMPurify;
window.remend = R.default || R.remend || R.parseIncompleteMarkdown || R;
`);
const out = new URL('../crates/amux-dashboard/static/vendor/md.js', import.meta.url).pathname;
fs.mkdirSync(path.dirname(out), { recursive: true });
await build({ entryPoints: [entry], bundle: true, format: 'iife', minify: true, target: 'es2020',
  outfile: out, nodePaths: [path.join(tmp, 'node_modules')], absWorkingDir: tmp,
  banner: { js: `/* amux vendor bundle: ${Object.entries(PKGS).map(([k, v]) => k + '@' + v).join(' ')} (scripts/build-md.mjs) */` } });
console.log('wrote', out, fs.statSync(out).size, 'bytes');
