#!/usr/bin/env node
// END-TO-END: vault secrets on EVERY worker execution path (AMUX-5375).
// Launch, restart, a tmux schedule, a shell schedule, a Chat turn and a
// delegate job, for authorized and unauthorized workers, an isolated worker,
// worker > group > global precedence, rotation and revocation.
//
// Private server, private tmux socket, fake `claude` on PATH that reports only
// the SHA-256 of each named variable, so no record this test reads holds a
// value. The last step sweeps every place a value could land.
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/vault-lifecycle.mjs
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { startAmux, waitFor } from './harness.mjs';

const V = {
  global: 'vl-global-3f1a77c2', ops: 'vl-ops-91d0e4b8', opsRotated: 'vl-ops-rotated-6c2e19aa',
  tokGlobal: 'vl-tokglobal-0e7d5b31', tokOps: 'vl-tokops-d4a8c612', tokAuth1: 'vl-tokauth1-58be03f9',
  solo: 'vl-solo-a17c4e2d',
};
const sha = s => crypto.createHash('sha256').update(s).digest('hex');
const KEYS = ['VL_GLOBAL', 'VL_OPS', 'VL_TOKEN', 'VL_SOLO'];
const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY,
  // AMUX_RS_SCHEDULER=1: Run now fires for real (the cron loop stays off under
  // the harness's fleet isolation, so nothing fires on its own).
  env: { AMUX_VAULT_KEYSTORE: 'file', FAKE_CLAUDE_ENV_KEYS: KEYS.join(','), AMUX_RS_SCHEDULER: '1' } });
const OWNER = { Authorization: 'Bearer ' + fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim() };
const call = async (m, p, b, h = OWNER) => { const r = await amux.req(m, p, b, 90000, h); return { ...r, raw: JSON.stringify(r.body ?? '') }; };
const real = d => fs.realpathSync(d);
const records = (pred) => amux.fakeLog().filter(pred);
const launches = dir => records(r => r.event === 'launch' && r.cwd && real(r.cwd) === real(dir));
async function launchEnv(name, dir, n) {
  await waitFor(`${name} launch #${n}`, () => launches(dir).length >= n, 180000);
  return launches(dir)[n - 1].env_sha256 || {};
}
// What each worker should hold: a hash, or null for "not set".
const want = (o) => Object.fromEntries(KEYS.map(k => [k, o[k] === undefined ? null : sha(o[k])]));
const same = (got, exp) => KEYS.every(k => (got || {})[k] === exp[k]);
const label = (got) => Object.fromEntries(KEYS.map(k => [k, !got || got[k] == null ? null : (Object.entries(V).find(([, v]) => sha(v) === got[k]) || ['?'])[0]]));

const W = { auth1: ['ops'], auth2: ['ops'], other: ['web'], solo: [] };
const dirs = {};
try {
  for (const [name, tags] of Object.entries(W)) {
    dirs[name] = path.join(amux.root, name); fs.mkdirSync(dirs[name]);
    const c = await call('POST', '/api/sessions', { name, dir: dirs[name], tags, start: false });
    check(`worker ${name} created`, c.status < 300, c.body);
  }
  // auth2 is an ISOLATED worker (owner decision: isolated workers receive
  // their provisioned secrets through the same scoping as any other worker).
  fs.appendFileSync(path.join(amux.home, 'sessions', 'auth2.env'), '\nCC_ISOLATED=1\n');

  const add = (key, scope, target, value) => call('POST', '/api/vault/secrets', { key, scope, target, value, description: 'e2e' });
  const ids = {};
  for (const [k, key, scope, target, value] of [
    ['global', 'VL_GLOBAL', 'global', '', V.global], ['ops', 'VL_OPS', 'group', 'ops', V.ops],
    ['tokGlobal', 'VL_TOKEN', 'global', '', V.tokGlobal], ['tokOps', 'VL_TOKEN', 'group', 'ops', V.tokOps],
    ['tokAuth1', 'VL_TOKEN', 'worker', 'auth1', V.tokAuth1], ['solo', 'VL_SOLO', 'worker', 'solo', V.solo]]) {
    const r = await add(key, scope, target, value);
    ids[k] = r.body.item?.id;
    check(`owner adds ${key} at ${scope}${target ? ' ' + target : ''}`, r.status === 201 && !r.raw.includes(value), r.body);
  }

  // ── LAUNCH ────────────────────────────────────────────────────────────
  const expect1 = {
    auth1: want({ VL_GLOBAL: V.global, VL_OPS: V.ops, VL_TOKEN: V.tokAuth1 }),
    auth2: want({ VL_GLOBAL: V.global, VL_OPS: V.ops, VL_TOKEN: V.tokOps }),
    other: want({ VL_GLOBAL: V.global, VL_TOKEN: V.tokGlobal }),
    solo: want({ VL_GLOBAL: V.global, VL_TOKEN: V.tokGlobal, VL_SOLO: V.solo }),
  };
  for (const n of Object.keys(W)) await call('POST', `/api/sessions/${n}/start`);
  for (const n of Object.keys(W)) {
    const got = await launchEnv(n, dirs[n], 1);
    check(`launch: ${n} sees exactly its authorized secrets (worker > group > global)`, same(got, expect1[n]), label(got));
  }
  check('launch: isolated auth2 gets global + its group, not auth1\'s worker value', expect1.auth2.VL_TOKEN === sha(V.tokOps));

  // Per-worker provisioned view: names, sources, current version, no values.
  await waitFor('ledger', async () => (await call('GET', '/api/vault/secrets?worker=auth1')).body.tracked, 20000);
  const p1 = await call('GET', '/api/vault/secrets?worker=auth1');
  const st = Object.fromEntries((p1.body.resolved || []).map(x => [x.key, `${x.scope}:${x.state}`]));
  check('provisioned view: auth1 running, every key current, source scopes right',
    p1.body.running === true && st.VL_TOKEN === 'worker:current' && st.VL_OPS === 'group:current' && st.VL_GLOBAL === 'global:current' && !st.VL_SOLO, st);
  const p2 = await call('GET', '/api/vault/secrets?worker=auth2');
  check('provisioned view marks auth2 isolated and lists its secrets', p2.body.isolated === true && (p2.body.resolved || []).length === 3, p2.body);
  check('no provisioned view carries a value', ![p1, p2].some(r => Object.values(V).some(v => r.raw.includes(v))));
  const acc = await call('GET', `/api/vault/secrets/${ids.tokGlobal}/access`);
  const accRows = Object.fromEntries((acc.body.workers || []).map(w => [w.worker, w.state]));
  check('effective access for global VL_TOKEN: overridden for auth1/auth2, current for other/solo',
    accRows.auth1 === 'overridden' && accRows.auth2 === 'overridden' && accRows.other === 'current' && accRows.solo === 'current', accRows);

  // ── ROTATE + REVOKE while running ─────────────────────────────────────
  const rot = await call('PUT', `/api/vault/secrets/${ids.ops}`, { value: V.opsRotated });
  check('rotate names the running workers that still hold the old value',
    rot.status === 200 && rot.body.item.version === 2 && JSON.stringify(rot.body.running_stale.sort()) === '["auth1","auth2"]', rot.body);
  const accOps = await call('GET', `/api/vault/secrets/${ids.ops}/access`);
  check('access view: rotated value is "stale" in running auth1/auth2 (restart to apply)',
    (accOps.body.workers || []).every(w => w.state === 'stale') && accOps.body.summary.running_needs_restart === 2, accOps.body.workers);
  const del = await call('DELETE', `/api/vault/secrets/${ids.solo}`);
  check('delete names the running worker that still holds it', JSON.stringify(del.body.still_held_by) === '["solo"]', del.body);
  const ps = await call('GET', '/api/vault/secrets?worker=solo');
  check('solo provisioned view: VL_SOLO revoked but held until restart', JSON.stringify(ps.body.revoked_held) === '["VL_SOLO"]', ps.body);

  // ── RESTART: rotation reaches it, revocation removes it ───────────────
  for (const n of ['auth1', 'auth2', 'solo']) {
    await call('POST', `/api/sessions/${n}/stop`);
  }
  await new Promise(r => setTimeout(r, 1500));
  for (const n of ['auth1', 'auth2', 'solo']) await call('POST', `/api/sessions/${n}/start`);
  const expect2 = {
    auth1: want({ VL_GLOBAL: V.global, VL_OPS: V.opsRotated, VL_TOKEN: V.tokAuth1 }),
    auth2: want({ VL_GLOBAL: V.global, VL_OPS: V.opsRotated, VL_TOKEN: V.tokOps }),
    solo: want({ VL_GLOBAL: V.global, VL_TOKEN: V.tokGlobal }),
  };
  for (const n of Object.keys(expect2)) {
    const got = await launchEnv(n, dirs[n], 2);
    check(`restart: ${n} gets the rotated value and loses revoked keys`, same(got, expect2[n]), label(got));
  }
  await waitFor('ledger after restart', async () => ((await call('GET', `/api/vault/secrets/${ids.ops}/access`)).body.workers || []).every(w => w.state === 'current'), 30000)
    .then(() => check('after restart the access view reads current', true))
    .catch(e => check('after restart the access view reads current', false, String(e)));

  // ── TMUX SCHEDULE: delivered to the running process that holds them ───
  const mk = body => call('POST', '/api/schedules', { schedule_expr: 'daily at 3am', enabled: 0, ...body });
  const runs = async id => { const r = await call('GET', `/api/schedules/runs?schedule_id=${id}&limit=20`); return Array.isArray(r.body) ? r.body : r.body.runs || r.body.rows || []; };
  const tm = await mk({ title: 'vault tmux probe', session: 'auth1', kind: 'tmux', command: 'vault-tmux-probe-1' });
  const tmId = tm.body.id || tm.body.schedule?.id;
  check('tmux schedule created', !!tmId, tm.body);
  await call('POST', `/api/schedules/${tmId}/run`, {});
  const sub = await waitFor('tmux schedule prompt reaches auth1', () => records(r => (r.text || '').includes('vault-tmux-probe-1'))[0], 60000).catch(() => null);
  check('tmux schedule: the process that receives the prompt holds auth1\'s current secrets', sub && same(sub.env_sha256, expect2.auth1), sub && label(sub.env_sha256));

  // ── SHELL SCHEDULES: owner's secrets in env, masked out of stored output ─
  const probe = 'python3 -c "import os,hashlib,json;print(\'HASHES=\'+json.dumps({k:(hashlib.sha256(os.environ[k].encode()).hexdigest() if k in os.environ else None) for k in [' + KEYS.map(k => `'${k}'`).join(',') + ']}))"; echo "echoed=$VL_TOKEN"';
  const shellFor = async (session, title) => {
    const s = await mk({ title, kind: 'shell', command: probe, ...(session ? { session } : {}) });
    const id = s.body.id || s.body.schedule?.id;
    await call('POST', `/api/schedules/${id}/run`, {});
    const logDir = path.join(amux.home, 'shell-runs', id);
    const readLog = () => fs.existsSync(logDir) ? fs.readdirSync(logDir).map(f => fs.readFileSync(path.join(logDir, f), 'utf8')).join('\n') : '';
    // Finished means the log carries its exit line AND the run row is final.
    const run = await waitFor(`shell run of ${id} finished`, async () => readLog().includes('[exit ')
      && (await runs(id)).find(r => ['ok', 'error'].includes(r.status)), 60000).catch(e => ({ error: String(e) }));
    const all = JSON.stringify(await runs(id));
    const logText = readLog();
    const m = (logText + all).match(/HASHES=(\{[^\n]*?\})(?:\\n|\n|")/);
    let hashes = null; try { hashes = JSON.parse(m[1].replace(/\\"/g, '"')); } catch {}
    return { id, run, all, logText, hashes };
  };
  const sh1 = await shellFor('auth1', 'vault shell probe auth1');
  check('shell schedule owned by auth1 gets auth1\'s current secrets', same(sh1.hashes, expect2.auth1), label(sh1.hashes));
  check('shell schedule output: an echoed value is masked in the run log and run row',
    sh1.logText.includes('echoed=[vault:VL_TOKEN]') && !sh1.logText.includes(V.tokAuth1) && !sh1.all.includes(V.tokAuth1), sh1.logText.slice(0, 400));
  const sh2 = await shellFor('other', 'vault shell probe other');
  check('shell schedule owned by unauthorized "other" gets only global values', same(sh2.hashes, want({ VL_GLOBAL: V.global, VL_TOKEN: V.tokGlobal })), label(sh2.hashes));
  const sh3 = await shellFor('', 'vault shell probe unowned');
  check('an unowned shell schedule gets no vault secrets', same(sh3.hashes, want({})), label(sh3.hashes));

  // ── CHAT TURN (headless): the worker's secrets, resolved per turn ─────
  const chat = await call('POST', '/api/sessions/auth1@chat/send', { text: 'vault-chat-probe', msg_id: 'vault-chat-1' });
  check('chat turn accepted', chat.status < 300, chat.body);
  const head = await waitFor('chat turn runs', () => records(r => r.event === 'headless' && /auth1/.test(r.session || ''))[0], 60000).catch(() => null);
  check('chat turn: the provider process holds auth1\'s current secrets', head && same(head.env_sha256, expect2.auth1), head && label(head.env_sha256));

  // ── DELEGATE (read-only job asked of a worker) ────────────────────────
  const dg = await call('POST', '/api/chat-delegate', { worker: 'other', prompt: 'vault-delegate-probe', mode: 'fresh', wait_s: 60 });
  let dgText = JSON.stringify(dg.body);
  if (dg.body.job?.id && !/env_sha256=/.test(dgText)) {
    await waitFor('delegate done', async () => { const g = await call('GET', `/api/chat-delegate/${dg.body.job.id}`); dgText = JSON.stringify(g.body); return /env_sha256=|"status":"(done|failed|error)"/.test(dgText); }, 60000).catch(() => {});
  }
  const dm = dgText.match(/env_sha256=(\{.*?\})(?:\\n|")/);
  let dh = null; try { dh = JSON.parse(dm[1].replace(/\\"/g, '"')); } catch {}
  check('delegate job for "other" holds other\'s secrets and nothing scoped elsewhere', same(dh, want({ VL_GLOBAL: V.global, VL_TOKEN: V.tokGlobal })), dh ? label(dh) : dgText.slice(0, 600));

  // ── AUTHZ from a worker origin ────────────────────────────────────────
  const wr = await call('POST', `/api/vault/secrets/${ids.tokAuth1}/reveal`, undefined, { ...OWNER, 'X-Amux-Session': 'auth1' });
  const wl = await call('GET', '/api/vault/secrets', undefined, { 'X-Amux-Session': 'other' });
  check('a worker cannot reveal, even its own secret', wr.status === 403 && !wr.raw.includes(V.tokAuth1), wr.status);
  check('a worker lists only items that apply to it', (wl.body.items || []).every(i => i.scope === 'global' || (i.scope === 'group' && i.target === 'web') || (i.scope === 'worker' && i.target === 'other')), wl.body.items);

  // ── AUDIT names every path ────────────────────────────────────────────
  const aud = await call('GET', '/api/vault/secrets/audit?limit=1000');
  const paths = new Set((aud.body.entries || []).filter(e => e.event === 'secrets_delivered').map(e => e.path));
  check('audit records deliveries on launch, shell schedule, chat turn and delegate', ['launch', 'shell_schedule', 'chat_turn', 'delegate'].every(p => paths.has(p)), [...paths]);

  // ── LEAK SWEEP ────────────────────────────────────────────────────────
  const read = f => { try { return fs.readFileSync(f, 'utf8'); } catch { return ''; } };
  const walk = d => { try { return fs.readdirSync(d, { withFileTypes: true }).flatMap(e => e.isDirectory() ? walk(path.join(d, e.name)) : [path.join(d, e.name)]); } catch { return []; } };
  const leaks = [];
  const scan = (label, text) => { for (const [k, v] of Object.entries(V)) if (text.includes(v)) leaks.push(`${label}:${k}`); };
  scan('server.log', read(amux.serverLog));
  scan('fake-claude.log', read(path.join(amux.root, 'fake-claude.log')));
  for (const f of walk(amux.home)) if (!/\.(png|jpg)$/.test(f)) scan(path.relative(amux.home, f), fs.readFileSync(f).toString('latin1'));
  scan('ps argv', execFileSync('ps', ['-axww', '-o', 'command'], { encoding: 'utf8' }));
  for (const n of Object.keys(W)) { try { scan(`pane ${n}`, amux.tmux('capture-pane', '-p', '-S', '-3000', '-t', `=amux-${n}:`)); } catch {} }
  check('no value anywhere under the amux home, in the server log, argv or pane history', !leaks.length, leaks);
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally { await amux.stop(); }
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
