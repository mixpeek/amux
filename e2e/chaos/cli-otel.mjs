#!/usr/bin/env node
// END-TO-END (governance PR 9): amux collects the agent CLI's own
// OpenTelemetry instead of rebuilding tool-call tracking with hooks.
//
// A private amux (own home, port and tmux socket, fake provider CLIs on PATH).
// The OTLP body posted here is a real Claude Code 2.1.319 export captured on
// 2026-11-10 (e2e/chaos/fixtures/claude-code-otel-logs.json, account and text
// scrubbed), sent the way the CLI sends it. Checks:
//   a. a Claude lane's launch environment carries the OTel configuration:
//      telemetry on, the endpoint at this amux, and exporter headers holding
//      the lane's own worker token (an isolated lane too);
//   b. a post without the lane's token, or with another lane's, is refused
//      403 and counted; a protobuf body is refused 415;
//   c. the lane's own post is accepted: the tool result and the API request
//      land in _amux_tool_events under the proven worker, with no prompt or
//      response text and no account identity, and the accepted tool decision
//      is not stored;
//   d. the turn takes the trace of the delivery that started it: the rows
//      carry the delivery's trace and span, source `delivered`, and
//      GET /api/trace/{id} shows them as children of the delivery;
//   e. the counters say what was received, per worker (telemetry_received_at),
//      measured; /api/audit lists the tool call;
//   f. a lane whose own environment sets OTel is left alone;
//   g. AMUX_CLI_TELEMETRY=off: posts refused 503 and a new launch is not wired.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/cli-otel.mjs
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor} from './harness.mjs';

const here = path.dirname(new URL(import.meta.url).pathname);
const KEYS = ['AMUX_WORKER_TOKEN', 'CLAUDE_CODE_ENABLE_TELEMETRY', 'OTEL_EXPORTER_OTLP_ENDPOINT',
  'OTEL_EXPORTER_OTLP_HEADERS', 'OTEL_EXPORTER_OTLP_PROTOCOL', 'OTEL_LOGS_EXPORTER'];
const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_ISOLATED: '0', AMUX_RS_SCHEDULER: '0', AMUX_BOARD_DRIVE_SECS: '0',
  AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0', AMUX_MODEL_CATALOG_REFRESH_SECS: '0',
  FAKE_CLAUDE_ENV_KEYS: KEYS.join(','),
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
  console.log('ok -', name);
};
const sha = v => crypto.createHash('sha256').update(v).digest('hex');
const db = (sql, args = []) => JSON.parse(execFileSync('python3', ['-I', '-c',
  'import json,sqlite3,sys; c=sqlite3.connect(sys.argv[1],timeout=30); c.row_factory=sqlite3.Row; r=c.execute(sys.argv[2],json.loads(sys.argv[3])); print(json.dumps([dict(x) for x in r.fetchall()])); c.commit()',
  path.join(amux.home, 'amux.db'), sql, JSON.stringify(args)], {encoding: 'utf8'}));
const token = lane => amux.tmux('show-environment', '-t', 'amux-' + lane, 'AMUX_WORKER_TOKEN').trim().split('=').slice(1).join('=');
const counters = async () => (await amux.req('GET', '/api/contract/counters')).body.cli_telemetry || {};
const port = new URL(amux.base).port;

// The captured export, stamped "now" so the turn falls just after the delivery.
const fixture = () => {
  const body = JSON.parse(fs.readFileSync(path.join(here, 'fixtures/claude-code-otel-logs.json'), 'utf8'));
  const now = Date.now();
  for (const rl of body.resourceLogs) for (const sl of rl.scopeLogs) for (const r of sl.logRecords) {
    r.timeUnixNano = String(BigInt(now) * 1000000n);
    for (const a of r.attributes || []) if (a.key === 'event.timestamp') a.value = {stringValue: new Date(now).toISOString()};
  }
  return body;
};
const post = (lane, tok, body = fixture(), ct = 'application/json') => amux.req('POST', '/api/otel/v1/logs', body, 15000,
  {'Content-Type': ct, ...(lane ? {'x-amux-session': lane} : {}), ...(tok ? {'x-amux-worker-token': tok} : {})});

async function lane(name, extraEnv = '') {
  const dir = path.join(amux.root, name); fs.mkdirSync(dir);
  const r = await amux.req('POST', '/api/sessions', {name, dir, start: false, provider: 'claude'});
  check('worker created ' + name, r.status === 201, r);
  fs.appendFileSync(path.join(amux.home, 'sessions', name + '.env'),
    '\nCC_TAGS=customers\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\n' + extraEnv);
  const s = await amux.req('POST', `/api/sessions/${name}/start`);
  check('worker started ' + name, s.status < 300, s);
  return await waitFor('provider launch ' + name, () => amux.fakeLog().find(x => x.event === 'launch'
    && ((x.argv || []).includes(name) || (x.cwd || '').endsWith('/' + name))), 30000);
}

let ok = false;
try {
  // a. Launch wiring.
  const launch = await lane('otel-a');
  const iso = await lane('otel-iso', 'CC_ISOLATED=1\n');
  const tokA = token('otel-a');
  check('the lane has a worker token', /^[0-9a-f]{64}$/.test(tokA), tokA.length);
  const h = launch.env_sha256 || {};
  check('a. telemetry is switched on for the lane', h.CLAUDE_CODE_ENABLE_TELEMETRY === sha('1'), h);
  check('a. the endpoint is this amux', h.OTEL_EXPORTER_OTLP_ENDPOINT === sha(`https://localhost:${port}/api/otel`), h);
  check('a. OTLP over HTTP as JSON, logs exporter on', h.OTEL_EXPORTER_OTLP_PROTOCOL === sha('http/json') && h.OTEL_LOGS_EXPORTER === sha('otlp'), h);
  check("a. the exporter headers carry the lane's own token",
    h.OTEL_EXPORTER_OTLP_HEADERS === sha(`x-amux-session=otel-a,x-amux-worker-token=${tokA}`), h);
  check('a. the headers (a secret) are not on the command line', !(launch.argv || []).join(' ').includes('OTEL_EXPORTER_OTLP_HEADERS')
    && !(launch.argv || []).join(' ').includes(tokA), launch.argv);
  const hi = iso.env_sha256 || {};
  check('a. an isolated lane is wired the same way (configuration, not a prompt)',
    hi.CLAUDE_CODE_ENABLE_TELEMETRY === sha('1') && hi.OTEL_EXPORTER_OTLP_HEADERS === sha(`x-amux-session=otel-iso,x-amux-worker-token=${token('otel-iso')}`), hi);

  // b. Only a proven worker may post.
  let before = await counters();
  const anon = await post(null, null);
  check('b. an anonymous post is refused 403', anon.status === 403 && anon.body.verdict === 'cli_telemetry_unproven', anon);
  const claimed = await post('otel-a', null);
  check('b. a claim without the token is refused 403', claimed.status === 403, claimed);
  const wrong = await post('otel-a', token('otel-iso'));
  check("b. another lane's token is refused 403", wrong.status === 403, wrong);
  const proto = await amux.req('POST', '/api/otel/v1/logs', undefined, 15000,
    {'Content-Type': 'application/x-protobuf', 'x-amux-session': 'otel-a', 'x-amux-worker-token': tokA});
  check('b. a protobuf body is refused 415 with what to send instead', proto.status === 415, proto);
  let after = await counters();
  check('b. the refusals are counted', after.refused_unproven - (before.refused_unproven || 0) === 3 && after.refused_protobuf >= 1, after);

  // d (setup). A delivery starts the turn: the owner sends to the lane.
  const send = await amux.req('POST', '/api/workers/otel-a/send', {text: 'otel e2e: run echo hi'}, 30000);
  check('the owner sends to the lane', send.status < 300, send);
  await waitFor('the lane receives the message', () => amux.fakeLog().some(x => x.text?.includes('otel e2e: run echo hi')), 30000);
  const file = fs.readFileSync(path.join(amux.home, 'trace', 'otel-a.current'), 'utf8');
  const [, T, S] = /^00-([0-9a-f]{32})-([0-9a-f]{16})-01/.exec(file) || [];
  check("the delivery wrote the lane's trace file", !!T, file);

  // c. The lane's own post.
  const okPost = await post('otel-a', tokA);
  check('c. the lane posting with its token is accepted (200, empty OTLP response)', okPost.status === 200, okPost);
  const rows = db("select * from _amux_tool_events where worker_id='otel-a' and source='otel' order by id");
  check('c. the tool result and the API request are stored (the accepted decision is not)',
    rows.map(r => r.event_name).sort().join(',') === 'api_request,tool_result', rows);
  check("c. the fixture's own amux.worker claim is kept beside the proven worker, never instead of it",
    rows.every(r => r.worker_id === 'otel-a' && JSON.parse(r.detail).resource_worker_claimed === 'probe'), rows);
  const bash = rows.find(r => r.event_name === 'tool_result');
  const d = JSON.parse(bash.detail);
  check('c. the tool call keeps its facts', bash.tool === 'Bash' && d.ok === true && d.duration_ms === 30 && d.input.includes('echo hi')
    && d.tool_use_id === 'toolu_01FoZn33WD2ZfNdHJqFtvhSM', d);
  const api = JSON.parse(rows.find(r => r.event_name === 'api_request').detail);
  check('c. the API request keeps model, tokens and cost', api.model === 'claude-haiku-5-5' && api.output_tokens === 74 && api.cost_usd > 0, api);
  const all = JSON.stringify(db("select detail from _amux_tool_events where worker_id='otel-a'"));
  check('c. no prompt or response text and no account identity stored',
    !/FIXTURE PROMPT|FIXTURE RESPONSE|owner@example\.com|user_FIXTURE/.test(all), all.slice(0, 400));
  const again = await post('otel-a', tokA);
  check('c. the same export posted twice adds no second tool row', again.status === 200
    && db("select count(*) n from _amux_tool_events where worker_id='otel-a' and event_name='tool_result'")[0].n === 1);

  // d. Trace.
  check('d. the turn carries the delivery trace and span, source delivered',
    rows.every(r => r.trace_id === T && r.parent_span_id === S && r.trace_source === 'delivered'), rows);
  const tree = (await amux.req('GET', `/api/trace/${T}`)).body;
  const toolNodes = tree.nodes.filter(n => n.source === 'tool_events');
  check('d. GET /api/trace shows the tool call and API request', toolNodes.length === 2 && tree.measured === true, tree);
  check('d. tool_events is a consulted source', tree.sources.some(s => s.name === 'tool_events' && s.consulted && s.rows === 2), tree.sources);
  const flat = JSON.stringify(tree.tree);
  const deliveryNode = tree.nodes.find(n => n.source === 'cmd_history' && n.span_id === S);
  check('d. the tool call nests under the delivery', !!deliveryNode && toolNodes.every(n => n.parent_span_id === S)
    && !tree.tree.some(r => r.source === 'tool_events'), flat.slice(0, 600));

  // e. Counters and audit.
  after = await counters();
  check('e. counters: received, stored, attributed, per worker, measured', after.measured === true && after.events_stored >= 2
    && after.events_attributed_to_a_trace >= 2 && !!after.telemetry_received_at['otel-a'] && after.n_considered >= 2, after);
  const audit = (await amux.req('GET', '/api/audit?worker=otel-a&kind=tool')).body;
  check('e. /api/audit lists the tool call', (audit.events || audit.rows || audit.timeline || []).some(e => e.type === 'Bash'), audit);

  // f. An owner's own OTel is left alone.
  const own = await lane('otel-own', 'OTEL_EXPORTER_OTLP_ENDPOINT=http://collector.example:4318\n');
  const ho = own.env_sha256 || {};
  check("f. a lane with its own OTel keeps it; amux sets none", ho.OTEL_EXPORTER_OTLP_ENDPOINT === sha('http://collector.example:4318')
    && ho.CLAUDE_CODE_ENABLE_TELEMETRY === null && ho.OTEL_EXPORTER_OTLP_HEADERS === null, ho);
  check('f. and says so', (await counters()).launches_user_otel_kept >= 1);

  // g. Off switch.
  await amux.down();
  amux.env.AMUX_CLI_TELEMETRY = 'off';
  await amux.up();
  const off = await post('otel-a', tokA);
  check('g. with AMUX_CLI_TELEMETRY=off a post is refused 503', off.status === 503 && off.body.verdict === 'cli_telemetry_off', off);
  const plain = await lane('otel-off');
  check('g. and a new launch is not wired', (plain.env_sha256 || {}).CLAUDE_CODE_ENABLE_TELEMETRY === null
    && /^[0-9a-f]{64}$/.test(token('otel-off')), plain.env_sha256);
  ok = true;
} finally {
  const receipt = {scenario: 'cli-otel', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(c => c.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
