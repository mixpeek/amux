#!/usr/bin/env node
// END-TO-END (amux's side of provider/model switching, no model spend):
// one worker is switched claude(sonnet) -> claude(opus) -> codex -> gemini ->
// muse -> claude. After every switch: the worker restarts on the right binary
// with the right model flag, and its message history and board card are still
// there. The fake agent stands in for every CLI (symlinked as codex, gemini,
// muse) and records each launch's binary and argv.
//
// What this does NOT prove: that each REAL CLI accepts those flags and that a
// message round-trips through its own TUI. That is the live lifecycle suite
// (e2e/lifecycle/live-*.spec.ts, AMUX_LIFECYCLE_PROVIDER), which spends tokens.
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/provider-switch.mjs
import { startAmux, waitFor } from './harness.mjs';

const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY });
const name = 'switcher';
const launches = () => amux.fakeLog().filter(e => e.event === 'launch');
const worker = async () => ((await amux.req('GET', '/api/sessions')).body || []).find(s => s.name === name) || {};
const history = async () => {
  const b = (await amux.req('GET', '/api/history?limit=500')).body;
  return (Array.isArray(b) ? b : b.rows || []).filter(r => r.session === name).map(r => r.text || '');
};
try {
  await amux.req('POST', '/api/sessions', { name, dir: amux.root, model: 'sonnet' });
  await waitFor('first launch', () => launches().length === 1, 30000);
  await waitFor('past boot', async () => { const w = await worker(); return w.running && !/start|boot/.test(w.status || ''); }, 60000, 500);
  await new Promise(r => setTimeout(r, 3000));
  const sent = await amux.req('POST', `/api/workers/${name}/send`, { text: 'remember the blue kite', record_history: true, msg_id: 'kite-1' }, 60000);
  check('message delivered before any switch', sent.body.submission === 'confirmed', sent.body);
  check('the message is in history before any switch', (await history()).some(t => t.includes('remember the blue kite')), await history());
  const card = await amux.req('POST', '/api/board', { title: 'provider switch keeps this card', session: name, status: 'todo' });
  const cardId = card.body.id;
  check('a board card exists for the worker', !!cardId, card.body);

  const steps = [
    { body: { model: 'opus' }, bin: 'claude', flag: /--model\s+opus/ },
    { body: { provider: 'codex' }, bin: 'codex', flag: /--model\s+\S+/ },
    { body: { provider: 'gemini' }, bin: 'gemini', flag: /--model\s+auto/ },
    { body: { provider: 'muse' }, bin: 'muse', flag: null },
    { body: { provider: 'claude' }, bin: 'claude', flag: null },
  ];
  for (const st of steps) {
    const before = launches().length;
    const r = await amux.req('PATCH', `/api/sessions/${name}/config`, st.body, 90000);
    const label = JSON.stringify(st.body);
    const l = await waitFor('relaunch ' + label, () => launches().length > before ? launches().at(-1) : null, 60000, 300).catch(() => null);
    check(`${label}: accepted and restarted`, r.status === 200 && !!l, { status: r.status, body: r.body });
    if (!l) continue;
    check(`${label}: launched ${st.bin}`, l.bin === st.bin, { bin: l.bin, argv: l.argv });
    if (st.flag) check(`${label}: model flag`, st.flag.test(l.argv.join(' ')), l.argv);
    check(`${label}: same working directory`, l.cwd.endsWith(amux.root.split('/').pop()) || l.cwd.includes('amux-chaos'), l.cwd);
    const h = await history();
    check(`${label}: message history kept`, h.some(t => t.includes('remember the blue kite')), h.slice(0, 5));
    const c = (await amux.req('GET', `/api/board/${cardId}`)).body;
    check(`${label}: board card kept`, c && c.id === cardId && c.session === name, { id: c && c.id, session: c && c.session });
  }
  const w = await worker();
  check('ends on claude and running', (w.provider || 'claude') === 'claude' && w.running === true, { provider: w.provider, running: w.running });
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally { await amux.stop(); }
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
