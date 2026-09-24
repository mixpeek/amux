#!/usr/bin/env node
// END-TO-END: a direct message sent while the worker is MID-TURN reaches the
// agent exactly once, after the turn ends, with nobody pressing Enter. The
// fake agent drops every Enter while busy (FAKE_CLAUDE_BUSY_S), which is the
// "unsubmitted text" incident: amux pasted the owner's message, Claude Code
// did not take the Enter, and the text sat in the input box.
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/midturn-send.mjs
import fs from 'node:fs';
import { startAmux, waitFor } from './harness.mjs';

const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY, env: { FAKE_CLAUDE_BUSY_S: '10' } });
try {
  const name = 'midturn';
  await amux.req('POST', '/api/sessions', { name, dir: amux.root });
  await waitFor('composer painted', () => {
    const pane = amux.tmux('capture-pane', '-p', '-t', `amux-${name}`);
    return pane.includes('❯') && pane.includes('bypass permissions');
  }, 30000);
  // A send right after boot is deferred until startup settles (correctly).
  await waitFor('worker past boot', async () => {
    const w = ((await amux.req('GET', '/api/sessions')).body || []).find(x => x.name === name) || {};
    return w.running && w.status && !/start|boot/.test(w.status) ? w.status : null;
  }, 60000, 500);
  await new Promise(r => setTimeout(r, 3000));
  const first = await amux.req('POST', `/api/workers/${name}/send`, { text: 'first message starts a turn' }, 60000);
  check('first message submitted while idle', first.body.submission === 'confirmed', first.body);
  await waitFor('agent is mid-turn', () => amux.tmux('capture-pane', '-p', '-t', `amux-${name}`).includes('esc to interrupt'), 5000, 100);
  const t0 = Date.now();
  const second = await amux.req('POST', `/api/workers/${name}/send`, { text: 'second message sent mid-turn' }, 60000);
  check('mid-turn send is accepted as held, not failed', second.status === 200 && second.body.ok === true && /held in the input box/.test(second.body.message || ''), second.body);
  const got = await waitFor('second message at the agent', () => amux.fakeLog().filter(e => (e.text || '').includes('second message sent mid-turn')).length ? true : null, 60000, 300).catch(() => false);
  await new Promise(r => setTimeout(r, 4000));
  const n = amux.fakeLog().filter(e => (e.text || '').includes('second message sent mid-turn')).length;
  check('the held message reached the agent after the turn, with no manual Enter', got, { waited_s: (Date.now() - t0) / 1000 });
  check('exactly once', n === 1, { n });
  const pane = amux.tmux('capture-pane', '-p', '-t', `amux-${name}`);
  check('the input box is empty afterwards (no unsubmitted text)', !/❯ \S/.test(pane.replace(/❯ ?\n/g, '')), pane.split('\n').filter(l => l.includes('❯')));
  const log = fs.readFileSync(amux.serverLog, 'utf8');
  check('the server logged the idle submit', log.includes('direct_draft_submitted_at_idle'));
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally { await amux.stop(); }
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
