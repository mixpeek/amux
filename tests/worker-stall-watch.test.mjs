// scripts/worker-stall-watch.sh against a fake curl and a fake amux.
//
// Pins the 2026-10-26 13:47Z false page: the wake POST timed out (http 000)
// while the wake itself landed in the worker's history, and a worker that had
// replied to a peer 23 s earlier read as idle for 21 min, because an isolated
// worker's last_progress_at lags its turns.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, existsSync, chmodSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const SCRIPT = join(dirname(fileURLToPath(import.meta.url)), '..', 'scripts', 'worker-stall-watch.sh');
const NOW = Math.floor(Date.now() / 1000);

// The fake curl serves /send from a queue of codes (one per call) and
// /api/history?session= from a queue of bodies; each call is logged.
const FAKE_CURL = `#!/usr/bin/env python3
import json, os, sys
d = os.environ['FAKE_DIR']
args = sys.argv[1:]
url = [a for a in args if a.startswith('http')][-1]
out = args[args.index('-o') + 1] if '-o' in args else None
st = json.load(open(d + '/state.json'))
open(d + '/calls.log', 'a').write(url + '\\n')
if url.endswith('/send'):
    code = st['send'].pop(0) if st['send'] else '000'
    if out:
        open(out, 'w').write('{"ok":true}' if code.startswith('2') else '')
    json.dump(st, open(d + '/state.json', 'w'))
    sys.stdout.write(code)
    sys.exit(28 if code == '000' else 0)
body = st['history'].pop(0) if st['history'] else '[]'
json.dump(st, open(d + '/state.json', 'w'))
sys.stdout.write(body)
`;
const FAKE_AMUX = `#!/bin/sh
[ "$1" = url ] && { echo https://fake.invalid; exit 0; }
echo "$@" >> "$FAKE_DIR/alerts.log"
`;

function run({ session, inbound, outbound, send = [], history = [], confirmSleep = '0' }) {
  const d = mkdtempSync(join(tmpdir(), 'stallwatch-'));
  writeFileSync(join(d, 'curl'), FAKE_CURL); chmodSync(join(d, 'curl'), 0o755);
  writeFileSync(join(d, 'amux'), FAKE_AMUX); chmodSync(join(d, 'amux'), 0o755);
  writeFileSync(join(d, 'state.json'), JSON.stringify({ send, history: history.map((h) => JSON.stringify(h)) }));
  writeFileSync(join(d, 'session.json'), JSON.stringify(session));
  writeFileSync(join(d, 'history.json'), JSON.stringify(inbound));
  const env = {
    ...process.env, PATH: `${d}:${process.env.PATH}`, FAKE_DIR: d,
    STALL_FIXTURE_SESSION: join(d, 'session.json'), STALL_FIXTURE_HISTORY: join(d, 'history.json'),
    STALL_LOG: join(d, 'watch.log'), STALL_STATE_DIR: join(d, 'state'), STALL_CONFIRM_SLEEP: confirmSleep,
    STALL_WAKE_ORIGIN: 'peer',
  };
  if (outbound) {
    writeFileSync(join(d, 'outbound.json'), JSON.stringify(outbound));
    env.STALL_FIXTURE_OUTBOUND = join(d, 'outbound.json');
  }
  const r = spawnSync('bash', [SCRIPT, 'w1'], { env, encoding: 'utf8', timeout: 20000 });
  const read = (f) => (existsSync(join(d, f)) ? readFileSync(join(d, f), 'utf8') : '');
  return { rc: r.status, log: read('watch.log'), alerts: read('alerts.log'), calls: read('calls.log') };
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
  assert.equal((r.calls.match(/\/send$/gm) || []).length, 1);
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
