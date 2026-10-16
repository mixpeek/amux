#!/usr/bin/env node
// The Rust-input key: one hash that moves whenever anything a Rust compile of
// this workspace consumes moves, and stays put when only the dashboard's
// static/ (or e2e/, docs, ...) changes.
//
// rust.yml keys two caches on it: `server-bin-<key>` (the debug amux-server
// the e2e shards boot) and `nextest-archive-<key>` (every compiled test
// binary). Main saves them; a pull request whose Rust inputs equal main's
// restores them instead of compiling. The PR's static/ still reaches the
// server and the tests at RUN time through AMUX_DASHBOARD_DIR
// (crates/amux-dashboard/src/lib.rs), which is why static/ alone may be left
// out of the key. Everything else that could change compiled bytes is in it:
//
//   - every tracked file under crates/ except crates/amux-dashboard/static/
//   - Cargo.toml, Cargo.lock, rust-toolchain*, .cargo/**
//   - .github/workflows/rust.yml (the build flags: profile env, dependency
//     opt-level, features live there)
//   - every file an include_str!/include_bytes! in crates/ resolves to,
//     wherever it lives (the bash CLI, scripts/hooks/*.py, repo-root docs)
//   - `rustc -V`, as resolved in the repo root (rust-toolchain.toml applies)
//
// File identity is the git blob id from `git ls-files -s`, so the key is a
// function of the checked-out index, not of mtimes.
//
// FAIL CLOSED. An include the parser cannot resolve to a path (a macro
// argument, a const, an env var other than CARGO_MANIFEST_DIR) means a compile
// input the key cannot see. The CLI then prints no key and exits 2, and
// rust.yml treats that as "no cache": PRs compile, main saves nothing.
//
// usage: ci-rust-key.mjs [--rustc "<rustc -V output>"] [--json]
//   Prints the key (64 hex chars) on stdout, or the full input listing with --json.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { pathToFileURL, fileURLToPath } from 'node:url';

export const EXCLUDED_PREFIX = 'crates/amux-dashboard/static/';

export function isRustInput(p) {
  if (p.startsWith('crates/')) return !p.startsWith(EXCLUDED_PREFIX);
  return p === 'Cargo.toml' || p === 'Cargo.lock' || /^rust-toolchain(\.toml)?$/.test(p) ||
    p.startsWith('.cargo/') || p === '.github/workflows/rust.yml';
}

// `git ls-files -s` -> Map(path -> blob id). Stage numbers other than 0 only
// appear mid-merge; all stages are kept so a conflict cannot alias a clean tree.
export function parseLsFiles(text) {
  const out = new Map();
  for (const line of text.split('\n')) {
    const m = /^(\d+) ([0-9a-f]+) (\d)\t(.*)$/.exec(line);
    if (!m) continue;
    const key = m[3] === '0' ? m[4] : `${m[4]}@stage${m[3]}`;
    out.set(key, `${m[1]}:${m[2]}`);
  }
  return out;
}

// A minimal Rust lexer: identifiers, string literals (normal, raw, byte) and
// single punctuation characters. Comments are dropped, so an include_str!
// written in prose is never mistaken for one in code, and an include_str!
// inside a string literal stays a string.
export function lexRust(src) {
  const toks = [];
  let i = 0;
  const n = src.length;
  while (i < n) {
    const c = src[i];
    if (c === '/' && src[i + 1] === '/') { while (i < n && src[i] !== '\n') i++; continue; }
    if (c === '/' && src[i + 1] === '*') {
      let depth = 1; i += 2;
      while (i < n && depth) {
        if (src[i] === '/' && src[i + 1] === '*') { depth++; i += 2; } else if (src[i] === '*' && src[i + 1] === '/') { depth--; i += 2; } else i++;
      }
      continue;
    }
    if (/\s/.test(c)) { i++; continue; }
    // raw / byte string prefixes
    const raw = /^(br|rb|r|b)(#*)"/.exec(src.slice(i, i + 260));
    if (raw && (raw[1].includes('r') || raw[2] === '')) {
      if (raw[1].includes('r')) {
        const hashes = raw[2];
        const start = i + raw[0].length;
        const end = src.indexOf('"' + hashes, start);
        if (end < 0) break;
        toks.push({ t: 'str', v: src.slice(start, end) });
        i = end + 1 + hashes.length;
        continue;
      }
      i += raw[1].length; // b"..." falls through to the normal string lexer
    }
    if (src[i] === '"') {
      let v = ''; i++;
      while (i < n && src[i] !== '"') {
        if (src[i] === '\\') {
          const e = src[i + 1];
          if (e === '\n') { i += 2; while (i < n && /\s/.test(src[i])) i++; continue; }
          v += ({ n: '\n', t: '\t', r: '\r', '0': '\0', '\\': '\\', '"': '"', "'": "'" })[e] ?? e; i += 2;
        } else v += src[i++];
      }
      i++;
      toks.push({ t: 'str', v });
      continue;
    }
    if (c === "'") {
      // char literal or lifetime
      if (src[i + 1] === '\\') { const end = src.indexOf("'", i + 3); i = end < 0 ? n : end + 1; continue; }
      if (src[i + 2] === "'") { i += 3; continue; }
      i++; continue;
    }
    if (/[A-Za-z_]/.test(c)) {
      let j = i + 1; while (j < n && /\w/.test(src[j])) j++;
      toks.push({ t: 'id', v: src.slice(i, j) }); i = j; continue;
    }
    toks.push({ t: 'p', v: c }); i++;
  }
  return toks;
}

// Every include_str!/include_bytes! in `src`:
//   { kind: 'path', rel, base: 'file'|'manifest' } | { kind: 'generated' } | { kind: 'unresolved', text }
export function findIncludes(src) {
  const toks = lexRust(src);
  const out = [];
  for (let k = 0; k + 2 < toks.length; k++) {
    const a = toks[k];
    if (a.t !== 'id' || (a.v !== 'include_str' && a.v !== 'include_bytes')) continue;
    if (toks[k + 1].v !== '!' || toks[k + 2].v !== '(') continue;
    let depth = 0, j = k + 2;
    const inner = [];
    for (; j < toks.length; j++) {
      if (toks[j].t === 'p' && toks[j].v === '(') { depth++; if (depth === 1) continue; }
      if (toks[j].t === 'p' && toks[j].v === ')') { depth--; if (depth === 0) break; }
      inner.push(toks[j]);
    }
    const text = inner.map(t => (t.t === 'str' ? JSON.stringify(t.v) : t.v)).join('');
    const ids = inner.filter(t => t.t === 'id').map(t => t.v);
    const puncts = inner.filter(t => t.t === 'p').map(t => t.v).join('');
    if (inner.length === 1 && inner[0].t === 'str') {
      out.push({ kind: 'path', rel: inner[0].v, base: 'file' });
    } else if (ids.length && ids.every(id => id === 'concat' || id === 'env') && /^[!(),]*$/.test(puncts)) {
      // concat!(env!("VAR"), "lit", ...): the string right after `env ! (` is the variable.
      const vars = [], lits = [];
      for (let q = 0; q < inner.length; q++) {
        if (inner[q].v === 'env' && inner[q + 1]?.v === '!' && inner[q + 2]?.v === '(' && inner[q + 3]?.t === 'str') {
          vars.push(inner[q + 3].v); q += 3;
        } else if (inner[q].t === 'str') lits.push(inner[q].v);
      }
      if (vars.length === 1 && vars[0] === 'CARGO_MANIFEST_DIR' && ids.filter(i => i === 'env').length === 1) {
        out.push({ kind: 'path', rel: lits.join(''), base: 'manifest' });
      } else if (vars.length === 1 && vars[0] === 'OUT_DIR') {
        out.push({ kind: 'generated', text });
      } else {
        out.push({ kind: 'unresolved', text });
      }
    } else {
      out.push({ kind: 'unresolved', text });
    }
    k = j;
  }
  return out;
}

// The crate directory (holding Cargo.toml) that owns `file`, for CARGO_MANIFEST_DIR.
function crateDirOf(file, tracked) {
  let d = path.posix.dirname(file);
  while (d && d !== '.') {
    if (tracked.has(`${d}/Cargo.toml`)) return d;
    d = path.posix.dirname(d);
  }
  return '';
}

// repo: { lsFiles: string (git ls-files -s), read(path) -> string|null }
export function rustKeyInputs(repo) {
  const tracked = parseLsFiles(repo.lsFiles);
  const entries = [...tracked].filter(([p]) => isRustInput(p.replace(/@stage\d$/, ''))).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  const includes = [], generated = [], unresolved = [];
  for (const [p] of entries) {
    if (!p.endsWith('.rs') || p.includes('@stage')) continue;
    const src = repo.read(p);
    if (src == null) { unresolved.push({ from: p, text: '(source unreadable)' }); continue; }
    for (const inc of findIncludes(src)) {
      if (inc.kind === 'generated') { generated.push({ from: p, text: inc.text }); continue; }
      if (inc.kind === 'unresolved') { unresolved.push({ from: p, text: inc.text }); continue; }
      const base = inc.base === 'file' ? path.posix.dirname(p) : crateDirOf(p, tracked);
      if (inc.base === 'manifest' && !base) { unresolved.push({ from: p, text: `no Cargo.toml above ${p}` }); continue; }
      const target = path.posix.normalize(inc.base === 'file' ? path.posix.join(base, inc.rel) : base + (inc.rel.startsWith('/') ? '' : '/') + inc.rel);
      if (target.startsWith('../') || path.posix.isAbsolute(target)) { unresolved.push({ from: p, text: `${inc.rel} leaves the repository` }); continue; }
      includes.push({ from: p, path: target, blob: tracked.get(target) || 'untracked' });
    }
  }
  return { entries, includes, generated, unresolved };
}

export function rustKey(inputs, rustc) {
  if (inputs.unresolved.length) return null;
  const h = crypto.createHash('sha256');
  h.update(`rust-key v1\nrustc ${String(rustc).trim()}\n`);
  for (const [p, blob] of inputs.entries) h.update(`file ${blob} ${p}\n`);
  const seen = new Set();
  for (const inc of [...inputs.includes].sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0))) {
    if (seen.has(inc.path)) continue;
    seen.add(inc.path);
    h.update(`include ${inc.blob} ${inc.path}\n`);
  }
  return h.digest('hex');
}

export function gitRustRepo(root) {
  return {
    lsFiles: execFileSync('git', ['-C', root, 'ls-files', '-s'], { encoding: 'utf8', maxBuffer: 1 << 30 }),
    read: (p) => { try { return fs.readFileSync(path.join(root, p), 'utf8'); } catch { return null; } },
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  const argv = process.argv.slice(2);
  const arg = (name) => { const i = argv.indexOf(name); return i >= 0 ? argv[i + 1] : undefined; };
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  const rustc = arg('--rustc') ?? execFileSync('rustc', ['-V'], { cwd: root, encoding: 'utf8' }).trim();
  const inputs = rustKeyInputs(gitRustRepo(root));
  const key = rustKey(inputs, rustc);
  if (argv.includes('--json')) {
    console.log(JSON.stringify({ verdict: 'ci_rust_key', measured: true, key, rustc, n_considered: inputs.entries.length,
      includes: inputs.includes, generated: inputs.generated, unresolved: inputs.unresolved }, null, 2));
  } else if (key) {
    console.log(key);
  }
  if (!key) {
    console.error(`ci-rust-key: ${inputs.unresolved.length} include(s) the key cannot see, so there is no key (no cache):`);
    for (const u of inputs.unresolved) console.error(`  ${u.from}: ${u.text}`);
    process.exit(2);
  }
  console.error(`ci-rust-key: ${inputs.entries.length} tracked inputs, ${new Set(inputs.includes.map(i => i.path)).size} include_str!/include_bytes! targets, ${inputs.generated.length} OUT_DIR includes, rustc "${rustc}"`);
}
