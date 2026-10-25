// A route start returns once the requested page has loaded, so the lane's
// first `state` reads that page, not about:blank. The page takes 2 s to
// answer; the state read right after start must already see it.
//
// Run against an isolated candidate server, like browser-routing-e2e.mjs:
//   AMUX_ROUTING_E2E_BASE=https://127.0.0.1:<port> AMUX_ROUTING_E2E_HOME=<home> node tests/browser-start-settle-e2e.mjs
// With AMUX_BROWSER_START_SETTLE_MS=0 on that server it fails, and the server
// logs verdict=browser_start_settle_timeout.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { run } from '../scripts/browser-route-driver.mjs';

const base = process.env.AMUX_ROUTING_E2E_BASE, home = process.env.AMUX_ROUTING_E2E_HOME;
assert(base && home, 'point at an isolated candidate server and its AMUX_HOME');
const site = createServer((req, res) => setTimeout(() => { res.setHeader('Content-Type', 'text/html'); res.end('<title>Slow proof</title><h1>Loaded after 2s</h1>'); }, 2000));
await new Promise(r => site.listen(0, '127.0.0.1', r));
const url = `http://127.0.0.1:${site.address().port}/slow`;
const profile = 'settle-proof';
mkdirSync(join(home, 'playwright-auth', 'profiles', profile), { recursive: true });
const ctx = { base, home, session: 'settle-e2e', config: { native_profile: profile, chrome_profile: '', cua_profile: '', allow_cua: false } };
try {
  const started = await run({ context: ctx, verb: 'start', body: { url, profile } });
  assert.equal(started.route?.backend, 'amux', 'started on the amux backend');
  const st = await run({ context: ctx, verb: 'state', body: {} });
  assert.ok(String(st.url).includes('/slow'), `first state reads the requested page, got ${st.url}`);
  assert.match(String(st.title), /Slow proof/, 'and its content has loaded');
  console.log('PASS route start waits for the requested page; first state reads it');
} finally {
  await run({ context: ctx, verb: 'stop', body: {} }).catch(() => {});
  site.close();
}
