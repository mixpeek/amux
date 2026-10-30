#!/usr/bin/env node
// AF-1032: the fleet snapshot survives a real SIGKILL and restart, so the first
// /api/sessions after a deploy is served at once instead of waiting 16-19 s for
// a cold fleet build. A worker deleted before the restart changes the registry,
// so the kept copy is NOT adopted and the deleted worker never reappears.
import fs from 'node:fs';
import path from 'node:path';
import { startAmux, waitFor } from './harness.mjs';
const checks = [];
const check = (name, ok, detail) => { checks.push({ name, ok: !!ok, detail }); if (!ok) throw new Error(name + ': ' + JSON.stringify(detail)); };
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY, env: { AMUX_ISOLATED: '0' } });
const kept = () => path.join(amux.home, 'cache', 'sessions-snapshot.json');
const restored = () => [...fs.readFileSync(amux.serverLog, 'utf8').matchAll(/verdict="sessions_snapshot_restored" outcome="([a-z_]+)"/g)].map(m => m[1]);
try {
  for (const name of ['snap-a', 'snap-b']) check('worker created ' + name, (await amux.req('POST', '/api/sessions', { name, dir: amux.root, start: false })).status === 201);
  await waitFor('snapshot kept on disk', async () => { await amux.req('GET', '/api/sessions'); return fs.existsSync(kept()); }, 30000);
  await amux.down(); await amux.up();
  const t0 = Date.now(); const r = await amux.req('GET', '/api/sessions'); const ms = Date.now() - t0;
  const names = (r.body || []).map(s => s.name);
  check('first read after SIGKILL restart lists the fleet', r.status === 200 && names.includes('snap-a') && names.includes('snap-b'), { status: r.status, names, ms });
  check('the kept snapshot was adopted', restored().at(-1) === 'adopted', restored());
  // A deleted worker: its env file goes, the registry changes, the copy is refused.
  await waitFor('snapshot re-kept', async () => { await amux.req('GET', '/api/sessions'); return fs.existsSync(kept()); }, 30000);
  await amux.down();
  fs.rmSync(path.join(amux.home, 'sessions', 'snap-b.env'));
  await amux.up();
  const r2 = await amux.req('GET', '/api/sessions');
  check('a worker deleted before the restart does not come back', !(r2.body || []).some(s => s.name === 'snap-b'), (r2.body || []).map(s => s.name));
  check('the kept copy was refused on the changed registry', restored().at(-1) === 'registry_changed', restored());
} catch (e) { checks.push({ name: 'scenario completed', ok: false, detail: String(e.stack || e) }); console.error(fs.readFileSync(amux.serverLog, 'utf8').slice(-6000)); }
finally { await amux.stop(); }
const receipt = { measured: checks.length > 0, n_considered: checks.length, failed: checks.filter(c => !c.ok).length, artifacts: amux.root, checks };
console.log(JSON.stringify(receipt, null, 2)); process.exit(receipt.failed ? 1 : 0);
