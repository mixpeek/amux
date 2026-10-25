// "Delivered while the worker was busy" lists only what THIS process took.
// Ethan, 2026-10-21 11:08 ("this is buggy too"): after the 10:54 fleet kill,
// amux-helper was relaunched at 10:55 and the panel listed five messages from
// 10:26-10:35 that the dead process had taken. The pane's scrollback still
// held the old output, so an old prompt served as the anchor.
import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const source = fs.readFileSync('crates/amux-dashboard/static/app.js', 'utf8');
const code = source.slice(source.indexOf('const _PEEK_UNECHOED_GRACE_MS'), source.indexOf('async function _peekLoadKindHints'));

const T0 = Date.parse('2026-10-21T10:00:00-04:00');
const at = (h, m) => T0 + ((h - 10) * 60 + m) * 60000;

function render({created, history, now}) {
  const ctx = vm.createContext({
    Date: class extends Date { static now() { return now; } }, Number, String, Set, Array, Math,
    peekSession: 'amux-helper',
    sessions: [{name: 'amux-helper', session_created: created / 1000}],
    _peekKindHints: [],
    _cmdHistory: history.map(([text, ts]) => ({session: 'amux-helper', type: 'user', text, ts})),
    _stripAnsi: s => s, esc: s => String(s), window: {},
  });
  vm.runInContext(code + '\n;globalThis.__f = _peekUnechoedHtml;', ctx);
  // The frame still shows the dead process's 09:50 prompt and the new 11:04 one.
  return ctx.__f('❯ check the shard sizes\n❯ send it\n', '');
}

const history = [
  ['check the shard sizes', at(9, 50)],          // shown in the old scrollback
  ['why cant this just be a day of migration thing', at(10, 26)],
  ['ok give me a message to send', at(10, 26)],
  ['cool ok so you have everything you need for now?', at(10, 35)],
  ['send it', at(11, 4)],                         // shown, new process
  ['and also check the pgvector counts', at(11, 6)], // taken mid-turn by the new process
];

test('messages sent before the relaunch are not listed as taken by the new process', () => {
  const html = render({created: at(10, 55), history, now: at(11, 8)});
  assert.ok(!html.includes('day of migration'), html);
  assert.ok(!html.includes('everything you need'), html);
  assert.ok(html.includes('pgvector counts'), 'a message the CURRENT process took mid-turn is still listed: ' + html);
});

// Ethan, 2026-10-22 16:41: rows taken during an EARLIER turn piled up under
// later prompts, out of order. The anchor is the newest printed prompt, so only
// the turn it started is listed, with or without a relaunch.
test('without a relaunch only the current turn is listed: an earlier turn\'s unprinted rows are not', () => {
  const html = render({created: at(9, 0), history, now: at(11, 8)});
  assert.ok(!html.includes('day of migration'), html);
  assert.ok(html.includes('pgvector counts'), html);
});

test('a session with no session_created still lists the current turn after the newest printed prompt', () => {
  const html = render({created: 0, history, now: at(11, 8)});
  assert.ok(!html.includes('day of migration'), html);
  assert.ok(html.includes('pgvector counts'), html);
});
