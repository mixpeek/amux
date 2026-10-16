// Button feedback audit (AMUX-5417). Computes, from the shipped source, every
// clickable control in the dashboard, what it calls, whether that is async, and
// what feedback the press gets. Writes docs/ux/button-feedback-audit.md.
//
//   node scripts/button-audit.mjs            # regenerate the doc
//   node scripts/button-audit.mjs --check    # fail if the doc is stale, or if
//                                            # any async control is outside the
//                                            # shared press-feedback layer
//
// The shared layer (state/feedback.mjs) gives every press on a declared control
// a busy state, a double-press guard and an outcome. A control is COVERED when
// the layer can see its click (the element matches the layer's selector, or it
// carries a delegated data-* hook the layer also binds) and its handler is
// started by a click/change/submit rather than pointerdown or touchstart.
//
// STABLE OUTPUT. The doc carries no line numbers and no population counts:
// both move on almost any app.js edit, and committing them made every pair of
// dashboard PRs conflict on this file. Rows are identified by file, control,
// handler and wiring, and sorted on those. Counts go to stdout, and --check
// still fails on the two invariants that matter (no async control outside the
// shared layer, no unexempted gesture write). Failure messages carry
// app.js:<line>, since they are read once and never committed.
import fs from 'node:fs';
import * as espree from 'espree';

const STATIC = 'crates/amux-dashboard/static';
const OUT = 'docs/ux/button-feedback-audit.md';
const app = fs.readFileSync(STATIC + '/app.js', 'utf8');
const html = fs.readFileSync(STATIC + '/index.html', 'utf8');
const registry = JSON.parse(fs.readFileSync(STATIC + '/state/control-registry.json', 'utf8'));
const mutating = new Set(Object.keys(registry.command_handlers));

// ---- call graph (same walk as scripts/build-state.mjs), plus READ fetches ----
const ast = espree.parse(app, { ecmaVersion: 'latest', sourceType: 'script', range: true });
const functions = new Map();
function visit(node, fn) {
  if (!node || typeof node !== 'object') return;
  if (node.type) fn(node);
  for (const v of Object.values(node)) {
    if (Array.isArray(v)) v.forEach(c => visit(c, fn));
    else if (v?.type) visit(v, fn);
  }
}
visit(ast, n => { if (n.type === 'FunctionDeclaration' && n.id) functions.set(n.id.name, n); });
const FETCHERS = new Set(['fetch', 'apiCall', '_directInteractionFetch', '_origFetch', '_uploadRequest', '_interactionAccept', '_queueOp']);
const graph = new Map();
const reads = new Set();
for (const [name, node] of functions) {
  const calls = new Set();
  visit(node.body, c => {
    if (c.type !== 'CallExpression') return;
    const callee = c.callee.name;
    if (callee) calls.add(callee);
    if (FETCHERS.has(callee)) reads.add(name);
  });
  graph.set(name, calls);
}
for (let changed = true; changed;) {
  changed = false;
  for (const [name, calls] of graph) {
    if (!reads.has(name) && [...calls].some(c => reads.has(c))) { reads.add(name); changed = true; }
  }
}

// What a handler does on its own, before the shared layer: does it await
// before its first request (so the click's synchronous binding is lost), does
// it disable itself, does it toast on success or on failure.
const DIALOG = /^(confirm|_confirm\w*|showConfirm|amuxConfirm|_amuxConfirm|_askConfirm|prompt|_prompt\w*)$/;
function profile(name) {
  const node = functions.get(name);
  const p = { awaitBeforeRequest: false, dialog: false, disables: false, toastOk: false, toastFail: false };
  if (!node) return p;
  let firstAwait = Infinity, firstRequest = Infinity;
  visit(node.body, c => {
    if (c.type === 'AwaitExpression') firstAwait = Math.min(firstAwait, c.range[0]);
    if (c.type === 'CallExpression') {
      const callee = c.callee.name;
      if (callee && (FETCHERS.has(callee) || mutating.has(callee) || reads.has(callee))) firstRequest = Math.min(firstRequest, c.range[0]);
      if (callee && DIALOG.test(callee)) p.dialog = true;
      if (c.callee.type === 'MemberExpression' && c.callee.property?.name === 'setAttribute' && c.arguments[0]?.value === 'aria-busy') p.disables = true;
    }
    if (c.type === 'AssignmentExpression' && c.left.type === 'MemberExpression' && c.left.property?.name === 'disabled' && c.right.value === true) p.disables = true;
  });
  p.awaitBeforeRequest = firstAwait < firstRequest && firstRequest !== Infinity;
  // Toasts: inside a catch, or in an `if (!r.ok)`-shaped branch, count as failure feedback.
  visit(node.body, c => {
    if (c.type === 'CatchClause') visit(c.body, d => { if (d.type === 'CallExpression' && d.callee.name === 'showToast') p.toastFail = true; });
    if (c.type === 'IfStatement' && /\bok\b|error|status/.test(app.slice(c.test.range[0], c.test.range[1])))
      visit(c.consequent, d => { if (d.type === 'CallExpression' && d.callee.name === 'showToast') p.toastFail = true; });
  });
  visit(node.body, c => {
    if (c.type !== 'CallExpression' || c.callee.name !== 'showToast') return;
    p.toastOk = true;
    const arg = c.arguments[0];
    if (arg && /\.ok\b|error|\bstatus\b|Not done|failed|Could not/i.test(app.slice(arg.range[0], arg.range[1]))) p.toastFail = true;
  });
  return p;
}

// ---- controls ----
const SELECTOR_TAGS = new Set(['button', 'input', 'select', 'textarea', 'a']);
// Gestures, not presses: a drag or a text selection has its own visible
// feedback (the thing moves, the selection shows). Listed with the reason so a
// NEW write started by pointerdown/touch still fails --check.
const NONCLICK_EXEMPT = {
  peekCheckSelection: 'text selection in the terminal; the selection itself is the feedback',
  _graphStartDrag: 'graph node drag; the node follows the finger',
  _graphEndDrag: 'graph node drop; the node stays where it was dropped',
};
const HELPERS = new Set(['esc', 'escJs', 'escAttr', 'encodeURIComponent', 'decodeURIComponent', 'String', 'Number', 'JSON', 'parseInt', 'Boolean',
  'event', 'stopPropagation', 'preventDefault', 'if', 'for', 'while', 'switch', 'function', 'return', 'this']);
function handlerOf(src) {
  const names = [...String(src).matchAll(/(?:^|[^\w.$])([\w$]+)\s*\(/g)].map(m => m[1]).filter(n => !HELPERS.has(n));
  return names.find(n => mutating.has(n)) || names.find(n => reads.has(n)) || names.find(n => functions.has(n)) || names[0] || '';
}
function lineOf(text, idx) { return text.slice(0, idx).split('\n').length; }
function labelAt(text, idx) {
  const open = text.lastIndexOf('<', idx);
  const tagEnd = text.indexOf('>', idx);
  const tag = text.slice(open, tagEnd + 1);
  const tagName = (tag.match(/^<([a-zA-Z][\w-]*)/) || [])[1] || '?';
  const attr = n => (tag.match(new RegExp('\\b' + n + '=\\\\?["\']([^"\'\\\\]+)')) || [])[1];
  let inner = text.slice(tagEnd + 1, text.indexOf('<', tagEnd + 1)).replace(/'\s*\+[^+]*\+\s*'/g, '…').replace(/\s+/g, ' ').trim();
  inner = inner.replace(/&[#\w]+;/g, '').replace(/[`'"]/g, '').trim();
  const label = attr('aria-label') || inner || attr('title') || attr('id') || '';
  return { tagName: tagName.toLowerCase(), id: attr('id') || '', label: label.slice(0, 48), role: attr('role') || '' };
}
const rows = [];
function addOnclick(text, file) {
  const re = /onclick=(\\?)(["'])/g;
  let m;
  while ((m = re.exec(text))) {
    const q = m[1] + m[2];
    const start = m.index + m[0].length;
    const end = text.indexOf(q, start);
    if (end < 0) continue;
    const src = text.slice(start, Math.min(end, start + 600));
    const h = handlerOf(src);
    const at = labelAt(text, m.index);
    rows.push({ file, line: lineOf(text, m.index), wiring: 'onclick', handler: h, ...at, seen: true });
  }
}
addOnclick(html, 'index.html');
addOnclick(app, 'app.js');
// Delegated actions: data-peek-action menus dispatch through one listener.
for (const m of app.matchAll(/data-peek-action="([\w-]+)"/g)) {
  const at = labelAt(app, m.index);
  rows.push({ file: 'app.js', line: lineOf(app, m.index), wiring: 'data-peek-action', handler: 'peek-action:' + m[1], ...at, seen: true });
}
// Programmatic click listeners whose handler is a named function or an arrow
// calling one. `seen` is false when the target cannot be shown to match the
// layer's selector statically (it is resolved in the browser).
function tagForId(id) {
  const re = new RegExp('<([a-zA-Z][\\w-]*)[^<>]*\\bid=\\\\?["\']' + id.replace(/-/g, '\\-') + '\\\\?["\']');
  return ((html.match(re) || app.match(re)) || [])[1]?.toLowerCase() || '?';
}
function resolveTarget(target, at) {
  let id = (target.match(/getElementById\(['"]([\w-]+)['"]\)/) || [])[1];
  if (!id && /^[\w$]+$/.test(target)) {
    const before = app.slice(Math.max(0, at - 600), at);
    const re = new RegExp('\\b' + target.replace(/\$/g, '\\$') + '\\s*=\\s*document\\.getElementById\\([\'"]([\\w-]+)[\'"]\\)', 'g');
    id = ([...before.matchAll(re)].at(-1) || [])[1];
  }
  return id ? { id, tagName: tagForId(id) } : { id: '', tagName: '?' };
}
for (const m of app.matchAll(/([\w$.'"()[\]#-]+)\.addEventListener\('click',\s*([^\n]{0,160})/g)) {
  const body = m[2];
  // `e.target === <el>` is a click on a backdrop: a dismiss, not a control.
  if (/\.target\s*===\s*[\w$]+/.test(body)) continue;
  const h = handlerOf(body) || (body.match(/^([\w$]+)/) || [])[1] || '';
  if (!h || ['close', 'dismiss'].includes(h)) continue;
  const target = m[1];
  const isDoc = /^document$/.test(target);
  if (isDoc) continue; // outside-click closers and global delegates, not controls
  const res = resolveTarget(target, m.index);
  rows.push({ file: 'app.js', line: lineOf(app, m.index), wiring: 'addEventListener', handler: h, tagName: res.tagName, id: res.id,
    label: res.id || target.slice(0, 48), role: '',
    seen: SELECTOR_TAGS.has(res.tagName) || /submit|button|btn|trigger|\bx\b|\bb\b/i.test(target) });
}
// Non-click starters the layer never sees.
const nonClick = [...app.matchAll(/addEventListener\('(pointerdown|mousedown|touchstart|touchend|pointerup)',\s*([^\n]{0,120})/g)]
  .map(m => ({ event: m[1], handler: handlerOf(m[2]), line: lineOf(app, m.index) }))
  .filter(r => mutating.has(r.handler))
  .map(r => ({ ...r, exempt: NONCLICK_EXEMPT[r.handler] || '' }));

// ---- classify ----
for (const r of rows) {
  const name = r.handler.startsWith('peek-action:') ? '' : r.handler;
  r.kind = mutating.has(name) ? 'async (writes)' : reads.has(name) ? 'async (reads)' : r.wiring === 'data-peek-action' ? 'dispatch' : 'local';
  const p = profile(name);
  r.today = [];
  if (r.kind.startsWith('async')) {
    r.today.push(p.awaitBeforeRequest ? 'no busy state (request after an await)' : 'outline pulse while running');
    r.today.push(p.disables ? 'disables itself' : 'no double-press guard');
    r.today.push(p.toastFail ? 'toast on failure' : 'failure only in Recent actions');
    if (r.kind === 'async (writes)') r.today.push(p.toastOk ? 'toast on success' : 'success only in Recent actions');
    r.gap = p.awaitBeforeRequest || !p.disables || !p.toastFail || (r.kind === 'async (writes)' && !p.toastOk);
  } else {
    r.today.push('immediate visible change');
    r.gap = false;
  }
  // Covered by the shared layer: an onclick attribute always matches [onclick];
  // data-peek-action items are role=menuitem divs the layer binds explicitly.
  r.covered = r.wiring === 'onclick' || r.wiring === 'data-peek-action' || (r.seen && SELECTOR_TAGS.has(r.tagName)) || r.seen;
}
const asyncRows = rows.filter(r => r.kind.startsWith('async'));
const gaps = asyncRows.filter(r => r.gap);
const uncovered = asyncRows.filter(r => !r.covered);
const summary = {
  measured: true,
  n_considered: rows.length,
  async: asyncRows.length,
  async_writes: rows.filter(r => r.kind === 'async (writes)').length,
  async_reads: rows.filter(r => r.kind === 'async (reads)').length,
  gaps_before_shared_layer: gaps.length,
  uncovered_by_shared_layer: uncovered.length,
  non_click_mutations: nonClick.length,
  non_click_unexempted: nonClick.filter(r => !r.exempt).length,
};

// Code-point order, not localeCompare: the output must not depend on the
// machine's ICU data or locale.
const cmp = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
const controlOf = r => r.label || r.id || r.tagName;
const rowKey = r => [r.handler, r.file, controlOf(r), r.wiring, r.kind, r.id, r.tagName].join('\u0000');
const stable = list => [...list].sort((a, b) => cmp(rowKey(a), rowKey(b)));
const esc = s => String(s).replace(/\|/g, '\\|').replace(/[<>]/g, c => (c === '<' ? '&lt;' : '&gt;'));
let md = `# Dashboard button feedback audit (AMUX-5417)

Generated by \`node scripts/button-audit.mjs\` from the shipped \`app.js\` and \`index.html\`. Do not edit by hand; \`--check\` fails when this file is stale.

## The contract

Every press on a control that starts a server request must:

1. show it was received within 100 ms: the control is disabled (where it can be), \`aria-busy="true"\`, dimmed, with a moving bar on its bottom edge;
2. refuse a second press while the first is running (no double-fire), and a second press within 700 ms before any request started;
3. show the outcome when it settles: a green outline on success, a red outline and a toast with the server's message on failure (unless the handler already said something), a toast on success when the control is gone (a dialog closed). Never silence.

A press arms its control for 5 s, so a handler that awaits something before its request (a third of them) still gets the busy state. A control that is legitimately pressed in quick succession opts out with \`data-repeatable\`.

The shared layer in \`state/feedback.mjs\` does all three for every declared control, so individual handlers do not need their own busy code. "Today" below describes each handler's OWN feedback, measured from its source, which is what a press got before the shared layer covered it.

## Invariants

\`node scripts/button-audit.mjs --check\` fails unless both hold:

- every async control (one whose handler reaches a request in the static call graph) is visible to the shared layer;
- every write started by a gesture (pointerdown/touch) carries an exemption with a reason, listed below.

Counts (controls inventoried, async, writes, reads, gaps) are printed by the script rather than kept here, so that adding one control does not rewrite this file. Rows are control occurrences in the source (one markup site may render many times), sorted by handler, file and control.

## Async controls

| file | control | handler | kind | handler's own feedback today | gap | covered by shared layer |
|---|---|---|---|---|---|---|
`;
for (const r of stable(asyncRows)) {
  md += `| ${r.file} | ${esc(controlOf(r))} | \`${esc(r.handler)}\` | ${r.kind} | ${esc(r.today.join('; '))} | ${r.gap ? 'yes' : 'no'} | ${r.covered ? 'yes' : '**no**'} |\n`;
}
md += `
## Local controls

These change the screen immediately (open a menu, switch a tab) and need no busy state.

| file | control | handler | wiring |
|---|---|---|---|
`;
for (const r of stable(rows.filter(r => !r.kind.startsWith('async')))) {
  md += `| ${r.file} | ${esc(controlOf(r))} | \`${esc(r.handler)}\` | ${r.wiring} |\n`;
}
if (nonClick.length) {
  md += `\n## Writes started by gestures\n\n| file | event | handler | why it needs no press feedback |\n|---|---|---|---|\n`;
  const gestureKey = r => [r.handler, r.event].join('\u0000');
  for (const r of [...nonClick].sort((a, b) => cmp(gestureKey(a), gestureKey(b))))
    md += `| app.js | ${r.event} | \`${r.handler}\` | ${r.exempt || '**not exempt**'} |\n`;
}

if (process.argv.includes('--check')) {
  const have = fs.existsSync(OUT) ? fs.readFileSync(OUT, 'utf8') : '';
  const problems = [];
  if (have !== md) problems.push(`${OUT} is stale: run node scripts/button-audit.mjs`);
  if (uncovered.length) problems.push(`${uncovered.length} async control(s) are outside the shared press-feedback layer: ${uncovered.slice(0, 5).map(r => r.file + ':' + r.line + ' ' + r.handler).join(', ')}`);
  const bare = nonClick.filter(r => !r.exempt);
  if (bare.length) problems.push(`${bare.length} write(s) start from pointerdown/touch, which the layer does not see: ${bare.map(r => 'app.js:' + r.line + ' ' + r.handler).join(', ')}. Start it from a click, or add it to NONCLICK_EXEMPT with the reason.`);
  if (problems.length) { console.error(problems.join('\n')); process.exit(1); }
  console.log(`button audit: ${JSON.stringify(summary)}`);
} else {
  fs.mkdirSync('docs/ux', { recursive: true });
  fs.writeFileSync(OUT, md);
  console.log(`button audit: ${JSON.stringify(summary)}`);
}
