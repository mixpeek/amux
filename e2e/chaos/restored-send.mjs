#!/usr/bin/env node
// END-TO-END: the agent CLEARS the box on Enter, records nothing, and puts the
// text back 2 s later (FAKE_CLAUDE_RESTORE_S), which is what Claude Code did to
// trailing-@-mention messages on 2026-09-24 (amux-helper 16:26). amux's frame
// reads saw an empty box and said "sent"; the text then sat unsubmitted. The
// message must reach the agent exactly once with no manual Enter.
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/restored-send.mjs
import fs from 'node:fs';
import path from 'node:path';
import { startAmux, waitFor } from './harness.mjs';

const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY, env: { FAKE_CLAUDE_RESTORE_S: '2' } });
try {
  const name = 'restored';
  const img = path.join(amux.root, 'shot.png'); fs.writeFileSync(img, 'x');
  await amux.req('POST', '/api/sessions', { name, dir: amux.root });
  await waitFor('past boot', async () => {
    const w = ((await amux.req('GET', '/api/sessions')).body || []).find(x => x.name === name) || {};
    return w.running && w.status && !/start|boot/.test(w.status);
  }, 60000, 500);
  await new Promise(r => setTimeout(r, 3000));
  const text = `figure out why i cant see the full log @${img}`;
  const r = await amux.req('POST', `/api/workers/${name}/send`, { text, record_history: true, msg_id: 'restore-1' }, 60000);
  check('send accepted', r.status === 200 && r.body.ok === true, r.body);
  const want = 'figure out why i cant see the full log';
  await waitFor('message at agent', () => amux.fakeLog().some(e => (e.text || '').includes(want)), 60000, 300).catch(() => null);
  await new Promise(r => setTimeout(r, 5000));
  const n = amux.fakeLog().filter(e => (e.text || '').includes(want)).length;
  check('the message reached the agent with no manual Enter', n >= 1, { n });
  check('exactly once', n === 1, { n });
  const pane = amux.tmux('capture-pane', '-p', '-t', `amux-${name}`);
  const box = pane.split('\n').find(l => l.startsWith('❯')) || '';
  check('the input box is empty afterwards (no unsubmitted text)', box.replace('❯', '').trim() === '', { box });
  const log = fs.readFileSync(amux.serverLog, 'utf8');
  check('the server logged the re-submit', log.includes('direct_draft_submitted_at_idle'), log.match(/verdict="?direct_draft_[a-z_]+/g));
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally { await amux.stop(); }
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
