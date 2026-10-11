// docs/peek-rendering.md must describe exactly the peek steps the code runs.
// The lists are read from source, not restated here, so adding a step without
// documenting it, or using a step that was never registered, turns this red.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const app = readFileSync('crates/amux-dashboard/static/app.js', 'utf8');
const rust = readFileSync('crates/amux-server/src/api/session_verbs.rs', 'utf8');
const doc = readFileSync('docs/peek-rendering.md', 'utf8');

function block(src, start, end) {
  const i = src.indexOf(start);
  assert.ok(i >= 0, `missing ${start}`);
  const j = src.indexOf(end, i);
  assert.ok(j > i, `unterminated ${start}`);
  return src.slice(i, j);
}
const keysOf = (body) => [...body.matchAll(/^\s{2}'([a-z-]+)':/gm)].map((m) => m[1]);
const documented = (id) => doc.includes('`' + id + '`');

const clientSteps = keysOf(block(app, 'const PEEK_STEPS = {', '\n};'));
const frameSteps = keysOf(block(app, 'const PEEK_FRAME_STEPS = {', '\n};'));
const serverSteps = [...block(rust, 'const PEEK_SERVER_STEPS', '\n];').matchAll(/\(\s*"([a-z-]+)",/g)].map((m) => m[1]);
const pipelineBody = block(app, 'const _PEEK_FULL =', 'function _peekProviderNow');
const providers = [...block(app, 'const PEEK_PIPELINES = {', '\n};').matchAll(/^\s{2}([a-z]+): /gm)].map((m) => m[1]);

test('the lists were found, so an empty match cannot pass', () => {
  assert.ok(clientSteps.length >= 8, `client steps ${clientSteps}`);
  assert.ok(frameSteps.length >= 8, `frame steps ${frameSteps}`);
  assert.ok(serverSteps.length >= 10, `server steps ${serverSteps}`);
  assert.ok(providers.includes('claude') && providers.includes('codex') && providers.includes('default'), `providers ${providers}`);
});

test('every registered step and provider pipeline is documented', () => {
  const missing = [...clientSteps, ...frameSteps, ...serverSteps, ...providers].filter((id) => !documented(id));
  assert.deepEqual(missing, [], 'add these to docs/peek-rendering.md');
});

test('pipelines name only registered dashboard steps', () => {
  const named = [...pipelineBody.matchAll(/'([a-z]+(?:-[a-z]+)+)'/g)].map((m) => m[1]);
  const unknown = named.filter((id) => !clientSteps.includes(id) && id !== 'thin-composer');
  assert.deepEqual(unknown, []);
});

test('every server path and step the peek emits is registered and documented', () => {
  const body = block(rust, 'async fn peek_response_at(', '\n// ----');
  const emitted = new Set();
  // Step ids reach `render` only through these shapes: a `steps` vec, a push or
  // extend onto it, or the literal slice passed to peek_render.
  const shapes = [
    /let mut steps = vec!\[([^\]]*)\]/g,
    /\(\s*"[a-z-]+",\s*vec!\[([^\]]*)\]/g,
    /steps\.push\(([^)]*)\)/g,
    /steps\.extend\(\[([^\]]*)\]/g,
    /peek_render\([\s\S]*?"[a-z-]+",\s*&\[([^\]]*)\]/g,
  ];
  for (const re of shapes) for (const m of body.matchAll(re)) {
    for (const lit of m[1].matchAll(/"([a-z]+(?:-[a-z]+)+)"/g)) emitted.add(lit[1]);
  }
  const paths = [...body.matchAll(/\(\s*"([a-z]+(?:-[a-z]+)+)",\s*vec!/g)].map((m) => m[1])
    .concat([...body.matchAll(/^\s*"([a-z]+(?:-[a-z]+)+)",\n\s*&\[/gm)].map((m) => m[1]));
  const unregistered = [...emitted].filter((id) => !serverSteps.includes(id) && !paths.includes(id));
  assert.deepEqual(unregistered, [], 'register these in PEEK_SERVER_STEPS');
  const undocumentedPaths = [...new Set(paths.concat(['worker-adapter']))].filter((p) => !documented(p));
  assert.deepEqual(undocumentedPaths, [], 'document these server paths');
  assert.ok(emitted.size >= 8, `found only ${[...emitted]}`);
});
