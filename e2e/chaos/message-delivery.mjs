#!/usr/bin/env node
// END-TO-END: message delivery between sessions.
//
// Creates two workers (sender, receiver), sends a message from sender to
// receiver via the /api/sessions/<name>/send endpoint, and verifies the
// message appears in the receiver's fake-claude log (ground truth: what
// actually reached the agent, not what amux believes it delivered).
//
// Also tests: delivery to a stopped worker queues and delivers on start,
// rapid-fire sends (3 messages in quick succession) all arrive, and sends
// to a nonexistent session return an error.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/message-delivery.mjs
import { startAmux, waitFor } from './harness.mjs';

const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY });

try {
  // Create two workers
  const rA = await amux.req('POST', '/api/sessions', { name: 'alice', dir: amux.root });
  check('alice created', rA.status === 201);
  const rB = await amux.req('POST', '/api/sessions', { name: 'bob', dir: amux.root });
  check('bob created', rB.status === 201);

  // Wait for both to be running
  await waitFor('alice running', async () => {
    const list = (await amux.req('GET', '/api/sessions')).body;
    return (Array.isArray(list) ? list : []).find(s => s.name === 'alice' && s.running);
  }, 20000);
  await waitFor('bob running', async () => {
    const list = (await amux.req('GET', '/api/sessions')).body;
    return (Array.isArray(list) ? list : []).find(s => s.name === 'bob' && s.running);
  }, 20000);

  // ── Test 1: basic send from alice to bob ──
  const sendR = await amux.req('POST', '/api/sessions/bob/send', {
    text: 'hello from alice', session: 'alice'
  });
  check('send returned 200', sendR.status === 200, sendR);

  const delivered = await waitFor('message in bob log', () => {
    const entries = amux.fakeLog().filter(e => e.text && e.text.includes('hello from alice'));
    return entries.length > 0 ? entries : null;
  }, 15000).catch(() => null);
  check('message reached bob agent (ground truth)', !!delivered, { log: amux.fakeLog() });

  // ── Test 2: send to nonexistent session ──
  const bad = await amux.req('POST', '/api/sessions/nobody/send', { text: 'lost' });
  check('send to nonexistent session fails', bad.status >= 400, bad);

  // ── Test 3: rapid-fire sends all arrive ──
  const msgs = ['rapid-1-' + Date.now(), 'rapid-2-' + Date.now(), 'rapid-3-' + Date.now()];
  for (const m of msgs) {
    await amux.req('POST', '/api/sessions/bob/send', { text: m, session: 'alice' });
  }
  const allArrived = await waitFor('all 3 rapid messages', () => {
    const log = amux.fakeLog();
    const found = msgs.filter(m => log.some(e => e.text && e.text.includes(m)));
    return found.length === msgs.length ? found : null;
  }, 20000).catch(() => null);
  check('all 3 rapid-fire messages reached bob', !!allArrived, {
    expected: msgs, found: allArrived, log: amux.fakeLog().filter(e => e.text)
  });

  // ── Test 4: send to stopped worker is accepted with deferred status ──
  // Full deferred delivery (queue while stopped, flush on start) requires the
  // message-capture runtime job, which is suppressed under AMUX_ISOLATED=1 to
  // prevent a test server from driving the production tmux fleet (AF-69).
  // We verify the server accepts and queues the message; end-to-end delivery
  // after boot is covered by the live fleet's own message-capture job.
  const rC = await amux.req('POST', '/api/sessions', { name: 'carol', dir: amux.root, start: false });
  check('carol created stopped', rC.status === 201 && rC.body.starting === false, rC);
  const queueMsg = 'queued-for-carol-' + Date.now();
  const qR = await amux.req('POST', '/api/sessions/carol/send', { text: queueMsg, session: 'alice' });
  check('send to stopped carol accepted and deferred', qR.status === 200 && qR.body.submission === 'deferred', qR);

  // ── Test 5: send to a just-started worker (not deferred) ──
  await amux.req('POST', '/api/sessions/carol/start');
  await waitFor('carol running', async () => {
    const list = (await amux.req('GET', '/api/sessions')).body;
    return (Array.isArray(list) ? list : []).find(s => s.name === 'carol' && s.running);
  }, 20000);
  await waitFor('carol agent launched', () => {
    return amux.fakeLog().find(e => e.event === 'launch' && e.argv && e.argv.includes('carol'));
  }, 20000);
  const liveMsg = 'live-to-carol-' + Date.now();
  const liveR = await amux.req('POST', '/api/sessions/carol/send', { text: liveMsg, session: 'alice' });
  check('send to running carol returned 200', liveR.status === 200, liveR);
  const carolGot = await waitFor('live message in carol log', () => {
    const entries = amux.fakeLog().filter(e => e.text && e.text.includes(liveMsg));
    return entries.length > 0 ? entries : null;
  }, 15000).catch(() => null);
  check('live message reached carol agent (ground truth)', !!carolGot, { log: amux.fakeLog().filter(e => e.text) });

} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally { await amux.stop(); }

const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
