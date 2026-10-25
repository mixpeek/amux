#!/usr/bin/env node
// Which CI work does this pull request affect? (owner decision, 2026-10-15)
//
// On a PULL REQUEST, rust.yml runs only what the change can affect; on a PUSH
// to main every job runs the full suite exactly as before, so whatever a PR
// selection misses is still caught at the next main run. This script is the
// one place that decides, and its verdict names every reason, so a reader of a
// green PR can see what did NOT run.
//
// usage: ci-affected.mjs --event <pull_request|push> [--base <ref>] [--head <ref>]
//   Writes key=value lines to $GITHUB_OUTPUT (when set) and prints one JSON
//   verdict on stdout. A git failure throws: the job goes red, and every
//   downstream job falls back to its full default (see rust.yml).
//
// Outputs:
//   rust          full | targeted | none
//   rust_filter   nextest filterset for `targeted` ('' otherwise)
//   rust_shards   JSON matrix for test-shards ([1..8] full, [1] targeted)
//   e2e           true when any Playwright work runs
//   full_e2e      true when desktop+mobile run every spec
//   e2e_specs     JSON list of spec paths (relative to e2e/) for desktop+mobile, '' = all
//   ios_specs     JSON list for ios-safari, '' = all
//   e2e_shards    JSON matrix for e2e-shards; e2e_shard_total its length
//   spa           true when spa-gates must run
//   none          true when nothing code-relevant changed
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { pathToFileURL, fileURLToPath } from 'node:url';

export const FULL_SHARDS = 16;
export const RUST_SHARDS = 8;
// More selected spec files than this and a PR runs the whole desktop+mobile
// suite: past this point the selection saves little and its reasons stop
// being readable.
export const MAX_SELECTED_SPECS = 40;
// Planned seconds per e2e shard (durations.json is per-test wall time with one
// worker; shards run --workers=2, so ~240s planned is ~2 min of execution).
export const SECONDS_PER_SHARD = 240;

// Changing any of these changes how EVERY spec runs, or how the run is planned
// and judged, so a selection cannot be trusted to cover them.
export const E2E_INFRA = [
  /^e2e\/playwright\.config\.ts$/, /^e2e\/fixtures[^/]*\.ts$/, /^e2e\/serve-head\.sh$/,
  /^e2e\/teardown\.ts$/, /^e2e\/ci-evidence-reporter\.mjs$/, /^e2e\/lifecycle\/evidence[^/]*$/,
  /^e2e\/test-env\.ts$/, /^e2e\/tmp-reap\.ts$/, /^package(-lock)?\.json$/,
  /^\.github\/workflows\/rust\.yml$/, /^scripts\/e2e-plan\.mjs$/, /^scripts\/check-e2e-evidence\.mjs$/,
  /^scripts\/ci-affected\.mjs$/, /^scripts\/ci-rust-key\.mjs$/,
];

// Rust compile inputs: any of these runs the whole Rust suite plus clippy.
// scripts/ci-rust-key.mjs is here too: it decides whether a PR may reuse
// main's compiled server and tests (rust.yml), so a change to it compiles
// everything rather than trusting the key it is changing.
export const RUST_FULL = [
  /^crates\/.*\.rs$/, /^crates\/[^/]+\/Cargo\.toml$/, /^Cargo\.(toml|lock)$/,
  /^crates\/[^/]+\/build\.rs$/, /^crates\/amux-server\/migrations\//,
  /^rust-toolchain(\.toml)?$/, /^\.cargo\//, /^\.github\/workflows\/rust\.yml$/,
  /^scripts\/ci-rust-key\.mjs$/,
];

// Integration tests that WALK the tree and read non-Rust files, keyed by the
// test binary. A change under `prefixes` selects the test when `lines` (if
// given) matches an added or removed line. Tests that only walk crates/*/src
// for .rs files are listed in RUST_ONLY_WALKERS: any .rs change runs the full
// suite, which covers them. tests/ci-affected.test.mjs fails if a test that
// calls read_dir is in neither list, so a new walker cannot be silently missed.
export const TREE_GUARDS = {
  legacy_port_guard: { prefixes: ['crates/', 'scripts/', 'e2e/', 'tests/'], lines: /8822/ },
  inv_tags: { prefixes: ['crates/', 'scripts/', 'e2e/', 'docs/rust-rebuild-plan.md'], lines: /INV-/ },
  e2e_route_stub_guard: { prefixes: ['e2e/'], lines: /\.route\(|@playwright\/test|fixtures/ },
  workflow_pipefail_guard: { prefixes: ['.github/workflows/'] },
  no_gcp_cloud_provisioning: { prefixes: ['cloud/', '.github/workflows/', 'Makefile', 'install.sh'] },
  nudge_commands_exist: { prefixes: ['amux'] },
  dockerfile_build_inputs: { prefixes: ['cloud/docker/'] },
  // Hashes every file under the dashboard's static/ (the derived APP_VER, #318).
  dashboard_asset_version: { prefixes: ['crates/amux-dashboard/'] },
  // Runs the bash CLI at the repo root (`amux exec --help`, 2849df12); its
  // read_dir is over a temp home.
  cli_worker_name_guard: { prefixes: ['amux'] },
  // Reads every script and git hook for a GNU-safe mktemp template
  // (e4fee90bf). Its CLI checks are already selected by the "amux" path literal.
  cli_syntax_guard: { prefixes: ['amux', 'scripts/', '.githooks/'], lines: /mktemp/ },
};
export const RUST_ONLY_WALKERS = [
  'dead_pub_api', 'kill_on_drop_guard', 'no_ollama_cli', 'schema_fixtures', 'runtime_tick_bracketing',
  'tmux_target_audit',
  // read_dir over a temp dir the test made, not the repository:
  'chat_delegate', 'restart_persistence', 'worker_integration_e2e',
];

// The SMOKE set runs on every PR that runs e2e at all. Fast (~72s planned over
// desktop+mobile+ios together), each one a different surface a broad break
// would hit first:
//   worker-toolbar-boot      cold boot of the SPA with a cached worker deep link, page errors fail it
//   worker-card-counts       the worker list, seeded through real POSTs to the board (the write)
//   peek-default-tab         opening a peek and its tab state
//   settings-layout          the settings surface and its tabs
//   lifecycle/board-header   the board tab rendering on a phone viewport
export const SMOKE = [
  'worker-toolbar-boot.spec.ts', 'worker-card-counts.spec.ts', 'peek-default-tab.spec.ts',
  'settings-layout.spec.ts', 'lifecycle/board-header.spec.ts',
];

// A spec whose name or text mentions any of these is iOS-relevant and keeps
// its ios-safari run on a PR. Everything else runs ios-safari on main only.
export const IOS_RELEVANT = /\bios\b|ios-safari|safari|webkit|touch|\btap\(|mobile|phone|\bsheet\b|bottom-sheet|safe-area|viewport/i;

// Class/id tokens too generic to discriminate between specs: a changed line
// carrying `class="active"` would otherwise select every spec asserting on any
// active tab. The function and id tokens on the same lines still select.
export const GENERIC_TOKENS = new Set([
  'active', 'hidden', 'open', 'show', 'shown', 'visible', 'disabled', 'selected', 'checked', 'collapsed',
  'expanded', 'btn', 'button', 'input', 'label', 'icon', 'card', 'row', 'col', 'title', 'text', 'small',
  'muted', 'primary', 'secondary', 'danger', 'warning', 'error', 'success', 'loading', 'empty', 'item',
  'list', 'header', 'footer', 'body', 'content', 'wrap', 'inner', 'container', 'flex', 'grid', 'block',
  'inline', 'none', 'auto', 'left', 'right', 'center', 'top', 'bottom', 'full', 'mobile', 'desktop', 'dark',
  'light', 'theme', 'focus', 'hover', 'menu', 'modal', 'overlay', 'badge', 'chip', 'pill', 'tab', 'tabs',
  'name', 'value', 'data', 'type', 'true', 'false', 'null', 'this', 'that', 'with', 'from', 'span', 'div',
]);

const rx = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const wordIn = (token, text) => new RegExp('(^|[^\\w-])' + rx(token) + '($|[^\\w-])').test(text);
const isDoc = (p) => /\.md$/i.test(p) || p.startsWith('docs/') || /^LICENSE/.test(p);

// ---------- diff parsing ----------
// `git diff -U0 --no-renames` text -> [{ path, deleted, hunks: [{ newStart, newCount, added, removed }] }]
export function parseDiff(text) {
  const files = [];
  let cur = null, hunk = null;
  for (const line of text.split('\n')) {
    if (line.startsWith('diff --git ')) {
      const m = /^diff --git a\/(.*) b\/(.*)$/.exec(line);
      cur = { path: m ? m[2] : line.slice(11), deleted: false, hunks: [] }; files.push(cur); hunk = null;
    } else if (!cur) continue;
    else if (line.startsWith('deleted file mode')) cur.deleted = true;
    else if (line.startsWith('+++ ') || line.startsWith('--- ')) {
      if (line === '--- /dev/null' || line === '+++ /dev/null') continue;
      const p = line.slice(4).replace(/^[ab]\//, ''); if (line.startsWith('+++ ')) cur.path = p;
      else if (!cur.path) cur.path = p;
    } else if (line.startsWith('@@')) {
      const m = /^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@/.exec(line);
      hunk = { newStart: Number(m[1]), newCount: m[2] === undefined ? 1 : Number(m[2]), added: [], removed: [] };
      cur.hunks.push(hunk);
    } else if (hunk && line.startsWith('+')) hunk.added.push(line.slice(1));
    else if (hunk && line.startsWith('-')) hunk.removed.push(line.slice(1));
  }
  return files;
}

// ---------- dashboard token extraction ----------
// Names of top-level functions (and function-valued bindings) whose source
// span intersects a changed hunk. Deleted-only hunks (newCount 0) are pinned
// to the line they sat next to.
export function changedFunctions(source, hunks, espree) {
  let ast = null;
  for (const sourceType of ['script', 'module']) {
    try { ast = espree.parse(source, { ecmaVersion: 'latest', sourceType, loc: true }); break; } catch { /* try next */ }
  }
  if (!ast) return { names: [], parsed: false };
  const spans = [];
  const fnLike = (n) => n && /Function|Class/.test(n.type);
  for (const n of ast.body) {
    const s = n.loc.start.line, e = n.loc.end.line;
    if (n.type === 'FunctionDeclaration' || n.type === 'ClassDeclaration') spans.push([n.id?.name, s, e]);
    else if (n.type === 'VariableDeclaration') {
      for (const d of n.declarations) if (d.id?.type === 'Identifier' && fnLike(d.init)) spans.push([d.id.name, d.loc.start.line, d.loc.end.line]);
    } else if (n.type === 'ExpressionStatement' && n.expression.type === 'AssignmentExpression' && fnLike(n.expression.right)) {
      const l = n.expression.left;
      const name = l.type === 'Identifier' ? l.name : (l.type === 'MemberExpression' && !l.computed ? l.property.name : null);
      spans.push([name, s, e]);
    }
  }
  const names = new Set();
  for (const h of hunks) {
    const lo = h.newStart, hi = h.newCount ? h.newStart + h.newCount - 1 : h.newStart;
    for (const [name, s, e] of spans) if (name && s <= hi && e >= lo) names.add(name);
  }
  return { names: [...names].sort(), parsed: true };
}

// Element ids, data-action values and class names named on a changed line.
export function lineTokens(line, kind) {
  const out = new Set();
  const add = (t) => { if (t && t.length >= 4 && !GENERIC_TOKENS.has(t.toLowerCase()) && /[a-z]/i.test(t)) out.add(t); };
  for (const m of line.matchAll(/\bid\s*[=:]\s*\\?["'`]([\w-]+)/g)) add(m[1]);
  for (const m of line.matchAll(/getElementById\(\s*["'`]([\w-]+)/g)) add(m[1]);
  for (const m of line.matchAll(/data-action\s*=\s*\\?["'`]?([\w-]+)/g)) add(m[1]);
  for (const m of line.matchAll(/dataset\.action\s*===?\s*["'`]([\w-]+)/g)) add(m[1]);
  for (const m of line.matchAll(/\bclass(?:Name)?\s*[=:]\s*\\?["'`]([^"'`\\$]+)/g)) for (const c of m[1].split(/\s+/)) add(c);
  for (const m of line.matchAll(/classList\.(?:add|remove|toggle|contains)\(([^)]*)\)/g))
    for (const s of m[1].matchAll(/["'`]([\w-]+)["'`]/g)) add(s[1]);
  for (const m of line.matchAll(/querySelector(?:All)?\(\s*["'`]([^"'`]+)/g))
    for (const s of m[1].matchAll(/[#.]([a-zA-Z][\w-]+)/g)) add(s[1]);
  if (kind === 'css') {
    // Selectors only: drop declarations so `#fff` and `0.5s` never become tokens.
    const sel = line.includes('{') ? line.slice(0, line.indexOf('{')) : (line.includes(':') && !line.trim().endsWith(',') ? '' : line);
    for (const m of sel.matchAll(/[#.]([a-zA-Z][\w-]+)/g)) {
      if (/^[0-9a-f]{3,8}$/i.test(m[1])) continue;
      add(m[1]);
    }
  }
  return out;
}

// ---------- Rust route prefixes for an api module ----------
function balanced(text, open) {
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === '(') depth++;
    else if (text[i] === ')' && --depth === 0) return text.slice(open + 1, i);
  }
  return text.slice(open + 1);
}
export function routePrefixes(moduleName, moduleSources, modRs) {
  const nests = [];
  for (const m of modRs.matchAll(/\.nest\(/g)) {
    const args = balanced(modRs, m.index + 5);
    const lit = /^\s*"([^"]+)"/.exec(args);
    if (lit && new RegExp('\\b' + rx(moduleName) + '::').test(args)) nests.push(lit[1]);
  }
  const out = new Set();
  const norm = (p) => { const cut = p.split(/[{*]/)[0].replace(/\/+$/, ''); return cut.startsWith('/api/') && cut.length > 5 ? cut : null; };
  for (const src of moduleSources) for (const m of src.matchAll(/\.(?:route|nest)\(\s*"([^"]*)"/g)) {
    const p = m[1];
    if (p.startsWith('/api')) { const n = norm(p); if (n) out.add(n); }
    else for (const pre of nests) { const n = norm(pre + (p === '/' ? '' : p)); if (n) out.add(n); }
  }
  for (const pre of nests) { const n = norm(pre); if (n) out.add(n); }
  return [...out].sort();
}

// ---------- Rust: which tests read a changed non-Rust file ----------
// path of a .rs file -> nextest filter clause for the tests defined in it.
export function rustClause(rsPath, pkgOf) {
  const m = /^crates\/([^/]+)\/(src|tests)\/(.*)\.rs$/.exec(rsPath);
  if (!m) return null;
  const pkg = pkgOf(m[1]);
  if (m[2] === 'tests') {
    const bin = m[3].includes('/') ? m[3].split('/')[0] : m[3];
    return `(package(=${pkg}) & binary(=${bin}))`;
  }
  const parts = m[3].split('/');
  const last = parts[parts.length - 1];
  if (last === 'mod') parts.pop();
  if (parts.length === 1 && ['lib', 'main'].includes(parts[0])) return `(package(=${pkg}))`;
  if (parts[0] === 'bin') return `(package(=${pkg}) & kind(bin))`;
  return `(package(=${pkg}) & test(/^${parts.join('::')}::/))`;
}

// Literal suffixes of a repo path that Rust source could name it by: every
// suffix of 2+ components, the parent directory's 2+ component suffixes, and a
// single dotted filename at the repo root (frustrations.md, install.sh).
export function pathNeedles(p) {
  const parts = p.split('/');
  const files = new Set(), dirs = new Set();
  for (let i = 0; i < parts.length - 1; i++) files.add(parts.slice(i).join('/'));
  if (parts.length === 1 && parts[0].includes('.')) files.add(parts[0]);
  const dir = parts.slice(0, -1);
  for (let i = 0; i < dir.length - 1; i++) dirs.add(dir.slice(i).join('/'));
  return { files: [...files], dirs: [...dirs] };
}

// ---------- the classifier ----------
// repo: { read(path) -> string|null, specs() -> [paths rel to e2e/], rustFiles() -> [repo paths],
//         durations: {}, pkgOf(crateDir) -> name, espree }
export function classify({ event, files, repo }) {
  const reasons = { rust: [], e2e: [], spa: [] };
  const allPaths = files.map(f => f.path);
  const verdictBase = { verdict: 'ci_affected', measured: true, event, n_considered: allPaths.length };
  if (event !== 'pull_request') {
    return full({ ...verdictBase, reason: `${event}: the full suite runs on every push to main` });
  }

  // ---- Rust ----
  let rust = 'none';
  const clauses = new Map(); // nextest clause -> { files, via } (first reader that named it)
  const note = (clause, file, via) => {
    if (!clauses.has(clause)) clauses.set(clause, { files: new Set(), via });
    clauses.get(clause).files.add(file);
  };
  for (const p of allPaths) if (RUST_FULL.some(r => r.test(p))) { rust = 'full'; reasons.rust.push(`${p}: Rust compile input`); }
  for (const f of files) if (f.path.startsWith('crates/') && !RUST_FULL.some(r => r.test(f.path)) &&
      !f.path.startsWith('crates/amux-dashboard/static/') && !isDoc(f.path)) {
    rust = 'full'; reasons.rust.push(`${f.path}: non-Rust file inside a crate (fixture or data); full suite to be safe`);
  }
  const rustSrc = new Map();
  if (rust !== 'full') {
    for (const rs of repo.rustFiles()) rustSrc.set(rs, repo.read(rs) || '');
    const nonRust = files.filter(f => !RUST_FULL.some(r => r.test(f.path)));
    for (const f of nonRust) {
      const lines = f.hunks.flatMap(h => [...h.added, ...h.removed]);
      for (const [bin, g] of Object.entries(TREE_GUARDS)) {
        if (!g.prefixes.some(pre => pre.endsWith('/') ? f.path.startsWith(pre) : f.path === pre)) continue;
        if (g.lines && !lines.some(l => g.lines.test(l)) && f.path !== 'docs/rust-rebuild-plan.md') continue;
        note(`(package(=amux-server) & binary(=${bin}))`, f.path, `tree guard ${bin}`);
      }
      const { files: fNeedles, dirs } = pathNeedles(f.path);
      for (const [rs, src] of rustSrc) {
        let hit = null;
        for (const m of src.matchAll(/include_(?:str|bytes)!\(\s*"([^"]+)"/g)) {
          if (path.posix.normalize(path.posix.join(path.posix.dirname(rs), m[1])) === f.path) { hit = `include_str!`; break; }
        }
        if (!hit) for (const n of fNeedles) if (new RegExp('["/]' + rx(n) + '"').test(src)) { hit = `"${n}"`; break; }
        if (!hit) for (const d of dirs) if (new RegExp('["/]' + rx(d) + '["/]').test(src)) { hit = `"${d}"`; break; }
        if (!hit) continue;
        const clause = rustClause(rs, repo.pkgOf);
        if (clause) note(clause, f.path, `${rs} (${hit})`);
      }
    }
    if (clauses.size) {
      rust = 'targeted';
      for (const [clause, { files: fs_, via }] of clauses) reasons.rust.push(`${clause} <- ${[...fs_].join(', ')} via ${via}`);
    }
  }

  // ---- e2e ----
  const specs = repo.specs();
  const specSet = new Set(specs);
  const specText = new Map(specs.map(s => [s, repo.read('e2e/' + s) || '']));
  const selected = new Map();
  const pick = (spec, why) => { if (specSet.has(spec) && !selected.has(spec)) selected.set(spec, why); };
  let fullE2e = false, e2eRelevant = false, spa = false;

  for (const p of allPaths) if (E2E_INFRA.some(r => r.test(p))) { fullE2e = true; reasons.e2e.push(`${p}: e2e infrastructure`); }
  for (const f of files) {
    const p = f.path;
    if (/^(crates\/amux-dashboard\/|e2e\/|tests\/[^/]+\.mjs$|eslint\.config\.mjs$|scripts\/spa-lint\.sh$|package)/.test(p)) {
      spa = true;
    }
    // (a) a changed spec runs itself
    if (p.startsWith('e2e/') && p.endsWith('.spec.ts')) {
      e2eRelevant = true;
      if (f.deleted) reasons.e2e.push(`${p}: deleted; nothing to run`);
      else pick(p.slice(4), 'changed spec');
      continue;
    }
    // (b) dashboard: functions on changed hunks, ids/actions/classes on changed lines
    if (p.startsWith('crates/amux-dashboard/static/')) {
      e2eRelevant = true;
      const tokens = new Map();
      const kind = p.endsWith('.css') ? 'css' : 'markup';
      for (const h of f.hunks) for (const l of [...h.added, ...h.removed]) for (const t of lineTokens(l, kind)) tokens.set(t, 'line token');
      if (/\.m?js$/.test(p) && !f.deleted) {
        const src = repo.read(p);
        const fn = src ? changedFunctions(src, f.hunks, repo.espree) : { names: [], parsed: false };
        if (!fn.parsed) reasons.e2e.push(`${p}: could not parse; function names unavailable`);
        for (const n of fn.names) if (n.length >= 3) tokens.set(n, 'function');
      }
      const base = path.posix.basename(p);
      if (base !== 'app.js' && base !== 'app.css' && base !== 'index.html') tokens.set(base, 'file name');
      for (const [t, how] of tokens) for (const [s, text] of specText) if (wordIn(t, text)) pick(s, `${p}: ${how} ${t}`);
      continue;
    }
    // other e2e support files: specs that import or name them
    if (p.startsWith('e2e/')) {
      e2eRelevant = true;
      if (/\.test\.mjs$/.test(p)) continue; // node:test suites run in spa-gates
      const stem = path.posix.basename(p).replace(/\.(ts|mjs|js)$/, '');
      const rel = p.slice(4);
      const imp = new RegExp(`["'\`][^"'\`]*/${rx(stem)}(\\.(ts|mjs|js))?["'\`]`);
      for (const [s, text] of specText) if (imp.test(text) || text.includes(rel)) pick(s, `${p}: imported or named`);
      continue;
    }
    // (c) server API modules: specs that call a route the module registers
    const api = /^crates\/amux-server\/src\/api\/([^/]+?)(?:\.rs|\/.*\.rs)$/.exec(p);
    if (api) {
      e2eRelevant = true;
      const mod = api[1];
      const srcs = [];
      if (repo.read(`crates/amux-server/src/api/${mod}.rs`)) srcs.push(repo.read(`crates/amux-server/src/api/${mod}.rs`));
      for (const rs of repo.rustFiles()) if (rs.startsWith(`crates/amux-server/src/api/${mod}/`)) srcs.push(repo.read(rs) || '');
      const prefixes = routePrefixes(mod, srcs, repo.read('crates/amux-server/src/api/mod.rs') || '');
      if (!prefixes.length) reasons.e2e.push(`${p}: registers no /api route; smoke only`);
      for (const pre of prefixes) {
        const re = new RegExp(rx(pre) + '($|[^\\w-])');
        for (const [s, text] of specText) if (re.test(text)) pick(s, `${p}: route ${pre}`);
      }
      continue;
    }
    // other server sources and assets the server binary carries
    if (/^crates\/(amux-server|amux-core|amux-dashboard)\//.test(p) || /^Cargo\.(toml|lock)$/.test(p)) {
      if (!/^crates\/[^/]+\/tests\//.test(p) && !isDoc(p)) { e2eRelevant = true; reasons.e2e.push(`${p}: server source; smoke only`); }
      continue;
    }
    // anything a spec names by path (scripts, fixtures)
    if (!isDoc(p)) {
      let named = false;
      // Only a path distinctive enough to be a reference: `amux` (the CLI) is in every spec's text.
      if (p.includes('/') || p.includes('.')) for (const [s, text] of specText) if (text.includes(p)) { pick(s, `${p}: named in spec`); named = true; }
      if (named) e2eRelevant = true;
      else if (rustSrc.size && [...rustSrc.entries()].some(([rs, src]) => rs.includes('/src/') && src.includes(path.posix.basename(p)) && path.posix.basename(p).length >= 6)) {
        e2eRelevant = true; reasons.e2e.push(`${p}: referenced by server source; smoke only`);
      }
    }
  }
  if (fullE2e) e2eRelevant = true;

  const none = rust === 'none' && !e2eRelevant && !spa;
  let e2eSpecs = null, iosSpecs = null;
  if (e2eRelevant) {
    if (!fullE2e) for (const s of SMOKE) pick(s, 'smoke');
    if (!fullE2e && selected.size > MAX_SELECTED_SPECS) {
      fullE2e = true; reasons.e2e.push(`${selected.size} spec files selected (> ${MAX_SELECTED_SPECS}); running the whole desktop+mobile suite`);
    }
    const picked = [...selected.keys()].sort();
    if (!fullE2e) e2eSpecs = picked;
    // ios-safari: the iOS-relevant part of what the change selected, even when
    // desktop+mobile went full on volume. Only an infrastructure change runs
    // every spec on ios-safari, because it changes how every spec runs there.
    const infra = allPaths.some(p => E2E_INFRA.some(r => r.test(p)));
    if (!infra) iosSpecs = picked.filter(s => IOS_RELEVANT.test(s) || IOS_RELEVANT.test(specText.get(s) || ''));
  }
  const planned = plannedSeconds(specs, e2eSpecs, iosSpecs, repo.durations);
  const shardTotal = !e2eRelevant ? 1 : (fullE2e && iosSpecs === null) ? FULL_SHARDS
    : Math.max(1, Math.min(FULL_SHARDS, planned.units, Math.ceil(planned.seconds / SECONDS_PER_SHARD)));
  const filter = rust === 'targeted' ? [...clauses.keys()].sort().join(' | ') : '';
  return {
    ...verdictBase,
    none,
    rust: { scope: rust, filter, shards: rust === 'full' ? RUST_SHARDS : rust === 'targeted' ? 1 : 0, reasons: reasons.rust },
    e2e: {
      run: e2eRelevant, full: fullE2e, specs: e2eSpecs, ios_specs: iosSpecs,
      selected: Object.fromEntries(selected),
      planned_seconds: Math.round(planned.seconds), planned_units: planned.units, shards: e2eRelevant ? shardTotal : 0,
      not_run: { 'desktop+mobile spec files': e2eSpecs ? specs.length - e2eSpecs.length : 0,
        'ios-safari spec files': iosSpecs ? specs.length - iosSpecs.length : 0 },
      reasons: reasons.e2e,
    },
    spa: spa || e2eRelevant,
  };
}

function full(base) {
  return { ...base, none: false,
    rust: { scope: 'full', filter: '', shards: RUST_SHARDS, reasons: [base.reason] },
    e2e: { run: true, full: true, specs: null, ios_specs: null, selected: {}, planned_seconds: null, planned_units: null,
      shards: FULL_SHARDS, not_run: { 'desktop+mobile spec files': 0, 'ios-safari spec files': 0 }, reasons: [base.reason] },
    spa: true };
}

export function plannedSeconds(allSpecs, specs, ios, durations) {
  const vals = Object.values(durations).sort((a, b) => a - b);
  const median = vals.length ? vals[Math.floor(vals.length / 2)] : 30;
  let seconds = 0, units = 0;
  const add = (project, list) => { for (const s of list) { seconds += durations[`${project}|${s}`] ?? median; units++; } };
  add('desktop', specs || allSpecs); add('mobile', specs || allSpecs); add('ios-safari', ios || allSpecs);
  return { seconds, units };
}

export function outputs(v) {
  const j = (x) => JSON.stringify(x);
  return {
    rust: v.rust.scope,
    rust_filter: v.rust.filter,
    rust_shards: j(Array.from({ length: Math.max(1, v.rust.shards) }, (_, i) => i + 1)),
    e2e: String(v.e2e.run),
    full_e2e: String(v.e2e.full),
    e2e_specs: v.e2e.specs ? j(v.e2e.specs) : '',
    ios_specs: v.e2e.ios_specs ? j(v.e2e.ios_specs) : '',
    e2e_shards: j(Array.from({ length: Math.max(1, v.e2e.shards) }, (_, i) => i + 1)),
    e2e_shard_total: String(Math.max(1, v.e2e.shards)),
    spa: String(v.spa),
    none: String(v.none),
  };
}

// Markdown for the job summary: what this run tests and, on a PR, what it does not.
export function summary(v, out) {
  const e = v.e2e;
  const e2eLine = !e.run ? '**skipped: not affected**'
    : e.full && !e.ios_specs ? '**full suite**, all three projects'
      : `desktop+mobile ${e.specs ? e.specs.length + ' spec files' : 'all spec files'}, ` +
        `ios-safari ${e.ios_specs ? e.ios_specs.length + ' spec files' : 'all spec files'}, ` +
        `${out.e2e_shard_total} shard(s), ~${e.planned_seconds}s planned`;
  return [
    `### What this run tests (${v.event})`,
    `- nothing code-relevant changed: ${v.none}`,
    `- Rust: **${v.rust.scope}**${v.rust.filter ? ' (`' + v.rust.filter + '`)' : ''}`,
    `- e2e: ${e2eLine}`,
    `- not run on this PR: ${JSON.stringify(e.not_run)}. The full suite runs on every push to main.`,
    '', 'Reasons:', ...[...v.rust.reasons, ...e.reasons].map(r => '- ' + r),
    ...(e.specs ? ['', 'Selected spec files:', ...e.specs.map(s => `- ${s}: ${e.selected[s] || ''}`)] : []),
  ].join('\n') + '\n';
}

// ---------- real repository adapter ----------
export function gitRepo(root, espree) {
  const read = (p) => { try { return fs.readFileSync(path.join(root, p), 'utf8'); } catch { return null; } };
  const walk = (dir, keep, out = []) => {
    let ents = []; try { ents = fs.readdirSync(path.join(root, dir), { withFileTypes: true }); } catch { return out; }
    for (const e of ents) {
      if (['node_modules', 'target', '.git', 'test-results'].includes(e.name)) continue;
      const rel = dir ? `${dir}/${e.name}` : e.name;
      if (e.isDirectory()) walk(rel, keep, out); else if (keep(rel)) out.push(rel);
    }
    return out;
  };
  let rustCache = null;
  const pkgs = {};
  return {
    read, espree,
    specs: () => walk('e2e', p => p.endsWith('.spec.ts') && !/\/lifecycle\/live-[^/]*\.spec\.ts$/.test(p)).map(p => p.slice(4)).sort(),
    rustFiles: () => (rustCache ||= walk('crates', p => p.endsWith('.rs'))),
    durations: JSON.parse(read('e2e/durations.json') || '{}'),
    pkgOf: (dir) => (pkgs[dir] ||= (/^\s*name\s*=\s*"([^"]+)"/m.exec(read(`crates/${dir}/Cargo.toml`) || '') || [, dir])[1]),
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  const args = Object.fromEntries(process.argv.slice(2).reduce((a, x, i, all) => (x.startsWith('--') ? [...a, [x.slice(2), all[i + 1]]] : a), []));
  const event = args.event || process.env.GITHUB_EVENT_NAME || 'pull_request';
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  let files = [];
  if (event === 'pull_request') {
    const base = args.base || 'origin/main', head = args.head || 'HEAD';
    const diff = execFileSync('git', ['-C', root, 'diff', '-U0', '--no-renames', '--no-color', `${base}...${head}`],
      { encoding: 'utf8', maxBuffer: 1 << 30 });
    files = parseDiff(diff);
  }
  const espree = await import('espree');
  const verdict = classify({ event, files, repo: gitRepo(root, espree) });
  const out = outputs(verdict);
  if (process.env.GITHUB_OUTPUT) fs.appendFileSync(process.env.GITHUB_OUTPUT, Object.entries(out).map(([k, v]) => `${k}=${v}`).join('\n') + '\n');
  if (process.env.GITHUB_STEP_SUMMARY) fs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, summary(verdict, out));
  console.log(JSON.stringify({ ...verdict, outputs: out }, null, 2));
}
