#!/usr/bin/env node
// END-TO-END: creating a worker starts it, with no /start call from the client.
// The fake agent's own launch record is the proof that something is running.
// Control: "start": false leaves the worker stopped and launches nothing.
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/create-autostart.mjs
import { startAmux, waitFor } from './harness.mjs';

const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY });
try {
  const created = await amux.req('POST', '/api/sessions', { name: 'auto-a', dir: amux.root });
  check('create answered 201 and says it is starting', created.status === 201 && created.body.starting === true, created);
  const launch = await waitFor('agent launch', () => amux.fakeLog().find(e => e.event === 'launch'), 30000).catch(() => null);
  check('the agent launched without any /start call', !!launch, { log: amux.fakeLog() });
  const running = await waitFor('running', async () => {
    const list = (await amux.req('GET', '/api/sessions')).body;
    return (Array.isArray(list) ? list : []).find(s => s.name === 'auto-a' && s.running === true);
  }, 20000).catch(() => null);
  check('API reports it running', !!running);

  const declined = await amux.req('POST', '/api/sessions', { name: 'auto-b', dir: amux.root, start: false });
  check('start:false says it is not starting', declined.status === 201 && declined.body.starting === false, declined);
  await new Promise(r => setTimeout(r, 3000));
  const launches = amux.fakeLog().filter(e => e.event === 'launch').length;
  check('CONTROL: start:false launched nothing (exactly one agent total)', launches === 1, { launches });
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally { await amux.stop(); }
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
