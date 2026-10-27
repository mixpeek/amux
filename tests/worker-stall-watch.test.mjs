// scripts/worker-stall-watch.sh against a fake curl and a fake amux.
//
// Pins the 2026-10-26 13:47Z false page: the wake POST timed out (http 000)
// while the wake itself landed in the worker's history, and a worker that had
// replied to a peer 23 s earlier read as idle for 21 min, because an isolated
// worker's last_progress_at lags its turns.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync, chmodSync, utimesSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const SCRIPT = join(dirname(fileURLToPath(import.meta.url)), '..', 'scripts', 'worker-stall-watch.sh');
const NOW = Math.floor(Date.now() / 1000);

// The fake curl serves /send from a queue of codes (one per call),
// /api/history?session= from a queue of bodies, and the ladder's routes from
// their own queues: /keys and /stop codes, /start [code, body] pairs, /peek pane
// texts, and GET /api/sessions/w1 `running` values. Every call is logged with
// its POST body.
const FAKE_CURL = `#!/usr/bin/env python3
import json, os, sys
d = os.environ['FAKE_DIR']
args = sys.argv[1:]
url = [a for a in args if a.startswith('http')][-1]
out = args[args.index('-o') + 1] if '-o' in args else None
data = args[args.index('-d') + 1] if '-d' in args else ''
st = json.load(open(d + '/state.json'))
open(d + '/calls.log', 'a').write(url + (' ' + data if data else '') + '\\n')
def pop(k, dflt):
    q = st.get(k) or []
    v = q.pop(0) if q else dflt
    st[k] = q
    json.dump(st, open(d + '/state.json', 'w'))
    return v
def reply(code, body):
    if out:
        open(out, 'w').write(body)
        sys.stdout.write(code)
    else:
        sys.stdout.write(body)
    sys.exit(28 if code == '000' else 0)
path = url.split('fake.invalid', 1)[-1]
if path.endswith('/send'):
    code = pop('send', '000')
    reply(code, '{"ok":true}' if code.startswith('2') else '')
if path.endswith('/keys'):
    code = pop('keys', '200')
    reply(code, '{"ok":true,"accepted":true,"effect":"unverified"}' if code.startswith('2') else '')
if path.endswith('/stop'):
    code = pop('stop', '202')
    reply(code, '{"ok":true,"message":"stopping"}' if code.startswith('2') else '')
if path.endswith('/start'):
    code, body = pop('start', ['202', '{"ok":true,"message":"starting","resumed":true}'])
    reply(code, body)
if '/peek' in path:
    reply('200', json.dumps({'output': pop('peek', '')}))
if path == '/api/sessions/w1':
    reply('200', json.dumps({'name': 'w1', 'running': pop('running', False)}))
reply('200', pop('history', '[]'))
`;
const FAKE_AMUX = `#!/bin/sh
[ "$1" = url ] && { echo https://fake.invalid; exit 0; }
echo "$@" >> "$FAKE_DIR/alerts.log"
`;

// A fixture dir that survives several runs (ticks) of the script, as the
// every-5m schedule does.
function setup(extraEnv = {}) {
  const d = mkdtempSync(join(tmpdir(), 'stallwatch-'));
  writeFileSync(join(d, 'curl'), FAKE_CURL); chmodSync(join(d, 'curl'), 0o755);
  writeFileSync(join(d, 'amux'), FAKE_AMUX); chmodSync(join(d, 'amux'), 0o755);
  const read = (f) => (existsSync(join(d, f)) ? readFileSync(join(d, f), 'utf8') : '');
  const tick = ({ session, inbound = [], outbound, send = [], history = [], keys, stop, start, peek, running,
    confirmSleep = '0', now = NOW, env = {}, args = ['w1'] }) => {
    const st = { send, history: history.map((h) => JSON.stringify(h)) };
    for (const [k, v] of Object.entries({ keys, stop, start, peek, running })) if (v !== undefined) st[k] = v;
    writeFileSync(join(d, 'state.json'), JSON.stringify(st));
    writeFileSync(join(d, 'session.json'), JSON.stringify(session));
    writeFileSync(join(d, 'history.json'), JSON.stringify(inbound));
    const e = {
      ...process.env, PATH: `${d}:${process.env.PATH}`, FAKE_DIR: d, AMUX_HOME: join(d, 'home'),
      STALL_FIXTURE_SESSION: join(d, 'session.json'), STALL_FIXTURE_HISTORY: join(d, 'history.json'),
      STALL_LOG: join(d, 'watch.log'), STALL_STATE_DIR: join(d, 'state'), STALL_CONFIRM_SLEEP: confirmSleep,
      STALL_WAKE_ORIGIN: 'peer', STALL_NOW: String(now), STALL_STOP_WAIT_S: '0', STALL_POLL_S: '0',
      ...extraEnv, ...env,
    };
    if (outbound) {
      writeFileSync(join(d, 'outbound.json'), JSON.stringify(outbound));
      e.STALL_FIXTURE_OUTBOUND = join(d, 'outbound.json');
    } else delete e.STALL_FIXTURE_OUTBOUND;
    const r = spawnSync('bash', [SCRIPT, ...args], { env: e, encoding: 'utf8', timeout: 20000 });
    return { rc: r.status, stdout: r.stdout, stderr: r.stderr, log: read('watch.log'), alerts: read('alerts.log'), calls: read('calls.log') };
  };
  return { d, tick, read };
}

function run(opts) {
  return setup().tick(opts);
}

// Idle 21 min by last_progress_at, with 4 inbound messages newer than that.
const stale = { name: 'w1', last_progress_at: NOW - 21 * 60, turn_state: 'completed', steering_queue: [] };
const inbound = [1, 2, 3, 4].map((i) => ({ ts: (NOW - 60 * i) * 1000 }));
const wakeRow = (at) => [{ ts: at * 1000, text: 'Stall watchdog: no progress for 21 min ...' }];

test('a message the worker sent to a peer counts as progress', () => {
  const r = run({ session: stale, inbound: [{ ts: (NOW - 60) * 1000 }], outbound: [{ ts: (NOW - 30) * 1000, sender: 'w1', session: 'peer' }] });
  assert.match(r.log, /verdict=ok/);
  assert.equal(r.calls, '', 'no wake is sent');
});

test('a send by the worker to itself (a relay into it) is not progress', () => {
  const r = run({ session: stale, inbound, outbound: [{ ts: (NOW - 30) * 1000, sender: 'w1', session: 'w1' }], send: ['200'] });
  assert.match(r.log, /verdict=STALLED/);
});

test('http 000 but the wake is in the history: confirmed late, no page', () => {
  // A 60 s re-check sleep: only the immediate first read can confirm in time.
  const r = run({ session: stale, inbound, send: ['000'], history: [wakeRow(NOW + 1)], confirmSleep: '60' });
  assert.match(r.log, /verdict=wake_unconfirmed http=000/);
  assert.match(r.log, /action=wake_sent verdict=wake_confirmed_late/);
  assert.equal(r.alerts, '', 'the owner is not paged');
  // Confirmed by the FIRST history read: one send, one read, no sleep-and-retry.
  assert.equal((r.calls.match(/\/send( |$)/gm) || []).length, 1);
  assert.equal((r.calls.match(/history\?session=/g) || []).length, 1);
});

test('http 000 and absent, the retry lands: sent on retry, no page', () => {
  const r = run({ session: stale, inbound, send: ['000', '200'], history: [[], []] });
  assert.match(r.log, /verdict=wake_sent_on_retry/);
  assert.equal(r.alerts, '');
});

test('http 000, absent after the retry: wake_failed pages the owner once', () => {
  const r = run({ session: stale, inbound, send: ['000', '000'], history: [[], [], []] });
  assert.match(r.log, /action=wake_failed verdict=wake_failed/);
  assert.match(r.alerts, /could not deliver a wake after a retry/);
  assert.equal(r.rc, 2);
});

test('a wake recorded before this send does not count as delivered', () => {
  const r = run({ session: stale, inbound, send: ['000', '000'], history: [wakeRow(NOW - 600), wakeRow(NOW - 600), wakeRow(NOW - 600)] });
  assert.match(r.log, /verdict=wake_failed/);
});

// ---- recovery ladder (MSG-98201, MSG-98259) -----------------------------------
// A worker blocked INSIDE a turn: 25 min without progress, 4 messages waiting.
const blocked = (extra = {}) => ({ name: 'w1', last_progress_at: NOW - 25 * 60, turn_state: 'running', steering_queue: [], isolated: false, ...extra });
const recovered = (at) => ({ name: 'w1', last_progress_at: at, turn_state: 'completed', steering_queue: [], isolated: false });
const RATE_MENU = 'Claude usage limit reached.\nWhat do you want to do?\n❯ 1. Stop and wait for limit to reset\n  2. Switch to usage credits\n  3. Upgrade your plan';
const RESUME_MENU = 'How do you want to resume?\n❯ 1. Resume from summary\n  2. Resume full session\n  3. Don’t ask me again';
const keyPosts = (calls) => [...calls.matchAll(/\/keys (\{.*\})/g)].map((m) => JSON.parse(m[1]).keys);
const verdicts = (log) => [...log.matchAll(/verdict=(ladder_\w+)(?: rung=(\w+))?(?: result=(\w+))?/g)].map((m) => m.slice(1).filter(Boolean).join(':'));

test('fail-before: a stalled running turn is interrupted first, the owner is not paged', () => {
  // The old code paged the owner at once here (action=owner_alert on the first run).
  const r = run({ session: blocked(), inbound, peek: ['working...'] });
  assert.deepEqual(keyPosts(r.calls), ['Escape']);
  assert.match(r.log, /verdict=ladder_skip rung=answer reason=no_known_prompt/);
  assert.match(r.log, /verdict=ladder_interrupted result=fired key=Escape http=200/);
  assert.equal(r.alerts, '', 'no page while a rung is pending');
  assert.doesNotMatch(r.log, /owner_alert/);
});

test('the ladder stops when progress resumes: verified, no further rung', () => {
  const s = setup();
  s.tick({ session: blocked(), inbound, peek: ['working...'] });
  const r = s.tick({ session: recovered(NOW + 120), inbound: [], now: NOW + 300 });
  assert.match(r.log, /verdict=ladder_interrupted result=verified progress_after_s=120 tried=answer,interrupt/);
  assert.equal(existsSync(join(s.d, 'state', 'w1.ladder')), false, 'stall state cleared');
  assert.doesNotMatch(r.calls, /\/stop|\/start/);
  assert.equal(r.alerts, '');
});

test('each rung fires in order, waits for verification, and pages only after the rest fail, once', () => {
  const s = setup();
  s.tick({ session: blocked(), inbound, peek: ['working...'] });                       // interrupt
  let r = s.tick({ session: blocked(), inbound, now: NOW + 300 });                       // within verify window
  assert.match(r.log, /verdict=ladder_wait rung=interrupt waited_s=300/);
  assert.doesNotMatch(r.calls, /\/stop/);
  r = s.tick({ session: blocked(), inbound, now: NOW + 540, running: [false] });         // interrupt failed -> restart
  assert.match(r.log, /WARN worker=w1 verdict=ladder_failed rung=interrupt reason=no_progress_after_8min/);
  assert.match(r.log, /verdict=ladder_restarted result=fired resumed=true/);
  assert.ok(r.calls.indexOf('/stop') < r.calls.indexOf('/start'), 'stop before start');
  assert.equal(r.alerts, '');
  r = s.tick({ session: blocked(), inbound, now: NOW + 1080 });                          // restart failed -> page
  assert.match(r.log, /verdict=ladder_failed rung=restart reason=no_progress_after_8min/);
  assert.match(r.log, /verdict=ladder_paged action=owner_alert tried=answer,interrupt,restart,page/);
  assert.match(r.alerts, /recovery ladder did not clear it \(tried: answer,interrupt,restart,page\)/);
  r = s.tick({ session: blocked(), inbound, now: NOW + 1380 });
  assert.equal(r.alerts.split('\n').filter(Boolean).length, 1, 'paged once per stall');
  assert.equal((r.calls.match(/\/stop/g) || []).length, 1, 'one restart per stall');
  assert.deepEqual(verdicts(r.log).filter((v) => !v.startsWith('ladder_skip') && !v.startsWith('ladder_wait')),
    ['ladder_interrupted:fired', 'ladder_failed:interrupt', 'ladder_restarted:fired', 'ladder_failed:restart', 'ladder_paged']);
});

test('answer rung: a rate-limit menu gets the server policy key (Enter), verified by re-reading the pane', () => {
  const r = run({ session: blocked(), inbound, peek: [RATE_MENU, 'working...'] });
  assert.deepEqual(keyPosts(r.calls), ['Enter']);
  assert.match(r.log, /verdict=ladder_answered result=fired prompt=rate_limit key=Enter/);
  assert.doesNotMatch(r.log, /ladder_interrupted/);
});

test('answer rung: the resume-mode selector gets the digit 1, never Enter', () => {
  const r = run({ session: blocked(), inbound, peek: [RESUME_MENU, 'working...'] });
  assert.deepEqual(keyPosts(r.calls), ['1']);
  assert.match(r.log, /verdict=ladder_answered result=fired prompt=resume_mode key=1/);
});

test('answer rung: policy off is respected, and the ladder moves to interrupt', () => {
  const r = run({ session: blocked(), inbound, peek: [RATE_MENU], env: { AMUX_RATE_LIMIT_ACTION: 'off' } });
  assert.match(r.log, /verdict=ladder_skip rung=answer reason=policy_off prompt=rate_limit/);
  assert.deepEqual(keyPosts(r.calls), ['Escape']);
});

test('answer rung: a prompt still on screen after the key is ladder_failed, then interrupt in the same run', () => {
  const r = run({ session: blocked(), inbound, peek: [RATE_MENU, RATE_MENU] });
  assert.match(r.log, /WARN worker=w1 verdict=ladder_failed rung=answer reason=prompt_still_shown/);
  assert.deepEqual(keyPosts(r.calls), ['Enter', 'Escape']);
  assert.equal(r.rc, 2, 'a failed rung is a non-zero run for the schedule');
});

test('restart rung: cooldown caps restarts per worker across stalls', () => {
  const s = setup();
  s.tick({ session: blocked(), inbound, peek: ['working...'] });
  s.tick({ session: blocked(), inbound, now: NOW + 540, running: [false] });             // restart #1
  s.tick({ session: recovered(NOW + 600), inbound: [], now: NOW + 660 });                // stall over
  const t0 = NOW + 3600;                                                                 // a new stall 1 h later
  const later = { ...blocked(), last_progress_at: t0 - 25 * 60 };
  s.tick({ session: later, inbound: inbound.map((m) => ({ ts: m.ts + 3600 * 1000 })), now: t0, peek: ['working...'] });
  const r = s.tick({ session: later, inbound: inbound.map((m) => ({ ts: m.ts + 3600 * 1000 })), now: t0 + 540 });
  assert.match(r.log, /verdict=ladder_skip rung=restart reason=cooldown last_restart_min_ago=\d+ cooldown_h=6/);
  assert.equal((r.calls.match(/\/stop/g) || []).length, 1, 'still one restart');
  assert.match(r.log, /verdict=ladder_paged action=owner_alert/);
});

test('restart rung: a stop that never confirms is ladder_failed and the page follows in the same run', () => {
  const s = setup();
  s.tick({ session: blocked(), inbound, peek: ['working...'] });
  const r = s.tick({ session: blocked(), inbound, now: NOW + 540, running: [true] });
  assert.match(r.log, /WARN worker=w1 verdict=ladder_failed rung=restart reason=stop_unconfirmed running=true/);
  assert.doesNotMatch(r.calls, /\/start/);
  assert.match(r.log, /verdict=ladder_paged/);
});

test('a rung disabled by config is skipped and logged', () => {
  const r = run({ session: blocked(), inbound, peek: ['working...'], running: [false], env: { STALL_RUNG_INTERRUPT: '0' } });
  assert.match(r.log, /verdict=ladder_skip rung=interrupt reason=disabled/);
  assert.match(r.log, /verdict=ladder_restarted result=fired/);
  assert.deepEqual(keyPosts(r.calls), []);
});

test('isolated worker: no keys and no restart unless the schedule opts in; the owner is paged', () => {
  let r = run({ session: blocked({ isolated: true }), inbound, peek: [RATE_MENU] });
  assert.match(r.log, /verdict=ladder_skip rung=answer reason=isolated/);
  assert.match(r.log, /verdict=ladder_skip rung=restart reason=isolated/);
  assert.doesNotMatch(r.calls, /\/keys|\/stop|\/start|\/peek/);
  assert.match(r.log, /verdict=ladder_paged/);
  r = run({ session: blocked({ isolated: true }), inbound, peek: ['working...'], env: { STALL_LADDER_ISOLATED: '1' } });
  assert.deepEqual(keyPosts(r.calls), ['Escape']);
  assert.equal(r.alerts, '');
});

test('an unmeasured run keeps the ladder state (it saw nothing)', () => {
  const s = setup();
  s.tick({ session: blocked(), inbound, peek: ['working...'] });
  s.tick({ session: { error: 'down' }, inbound, now: NOW + 300 });
  assert.match(readFileSync(join(s.d, 'state', 'w1.ladder'), 'utf8'), /rung=interrupt/);
});

test('--status prints the ladder state read-only', () => {
  const s = setup();
  s.tick({ session: blocked(), inbound, peek: ['working...'] });
  const r = s.tick({ session: blocked(), inbound, args: ['--status', 'w1'] });
  assert.match(r.stdout, /worker=w1 ladder=\[.*rung=interrupt.*\] restarts=0 last_restart= paged=no/);
});

// ---- subagent age -------------------------------------------------------------
function transcriptFixture(d, agents) {
  const proj = join(d, 'proj'); const subs = join(proj, 'conv', 'subagents');
  mkdirSync(subs, { recursive: true });
  writeFileSync(join(proj, 'conv.jsonl'), '{}\n');
  for (const a of agents) {
    const f = join(subs, `agent-${a.id}.jsonl`);
    const started = new Date((NOW - a.ageS) * 1000).toISOString().replace(/\.\d+Z$/, '.000Z');
    writeFileSync(f, JSON.stringify({ type: 'user', timestamp: started, message: { role: 'user', content: 'go' } }) + '\n');
    writeFileSync(join(subs, `agent-${a.id}.meta.json`), JSON.stringify({ description: a.desc, ...(a.finished ? { finished: true } : {}) }));
    utimesSync(f, NOW - a.idleS, NOW - a.idleS);
  }
  return join(proj, 'conv.jsonl');
}
const healthy = { name: 'w1', last_progress_at: NOW - 60, turn_state: 'running', steering_queue: [] };

test('subagent age: silent > 60 min, or running > 4 h and silent > 15 min, is subagent_stale; a long-running agent still writing is not', () => {
  const s = setup();
  const tp = transcriptFixture(s.d, [
    { id: 'old', desc: 'running five hours', ageS: 5 * 3600, idleS: 30 },
    { id: 'oldquiet', desc: 'running five hours, silent twenty minutes', ageS: 5 * 3600, idleS: 20 * 60 },
    { id: 'quiet', desc: 'silent ninety minutes', ageS: 2 * 3600, idleS: 90 * 60 },
    { id: 'fresh', desc: 'healthy', ageS: 600, idleS: 20 },
    { id: 'done', desc: 'finished', ageS: 5 * 3600, idleS: 90 * 60, finished: true },
  ]);
  let r = s.tick({ session: healthy, send: ['200'], env: { STALL_FIXTURE_TRANSCRIPT: tp } });
  assert.match(r.log, /WARN worker=w1 verdict=subagent_stale measured=true n_considered=5 running=4 stale=2 idle_min=60 max_h=4 ids=oldquiet,quiet/);
  assert.match(r.log, /verdict=subagent_stale action=wake_sent http=200 ids=oldquiet,quiet/);
  const wake = r.calls.split('\n').find((l) => l.includes('/send'));
  assert.match(wake, /oldquiet \(running five hours, silent twenty minutes\) running 5h00m/);
  assert.doesNotMatch(r.log, /ids=[^ ]*\bold\b/);
  assert.match(wake, /TaskStop/);
  r = s.tick({ session: healthy, send: ['200'], env: { STALL_FIXTURE_TRANSCRIPT: tp } });
  assert.equal((r.calls.match(/\/send/g) || []).length, 1, 'the same stale set is not re-woken');
});

test('subagent age: only fresh subagents is subagent_ok with its count; no transcript is unmeasured', () => {
  const s = setup();
  const tp = transcriptFixture(s.d, [{ id: 'fresh', desc: 'healthy', ageS: 600, idleS: 20 }]);
  let r = s.tick({ session: healthy, env: { STALL_FIXTURE_TRANSCRIPT: tp } });
  assert.match(r.log, /verdict=subagent_ok measured=true n_considered=1 running=1 stale=0/);
  assert.equal(r.calls, '');
  r = run({ session: healthy });
  assert.match(r.log, /verdict=subagent_unmeasured measured=false n_considered=0 why_unmeasured="no session meta"/);
});

test('the prompt phrases and policy defaults this script mirrors are still in the server', () => {
  const rs = readFileSync(join(dirname(SCRIPT), '..', 'crates', 'amux-server', 'src', 'api', 'session_verbs.rs'), 'utf8');
  for (const needle of ['"what do you want to do?"', '"stop and wait for limit to reset"', '"switch to usage credits"',
    '1\\.\\s+resume from summary', '2\\.\\s+resume full session', '"don\'t ask me again"',
    '"AMUX_RATE_LIMIT_ACTION"', '"AMUX_RESUME_MODE_ACTION"', '.unwrap_or_else(|| "wait".into())', '.unwrap_or_else(|| "summary".into())',
    'send_keys_op(name, "Enter")', 'send_keys_op(name, "1")']) {
    assert.ok(rs.includes(needle), `session_verbs.rs no longer contains ${needle}`);
  }
  const guard = readFileSync(join(dirname(SCRIPT), 'hooks', 'boundary-guard.py'), 'utf8');
  assert.match(guard, /def count_running_subagents\(transcript_path: str, cfg: dict, now: float \| None = None,/);
});
