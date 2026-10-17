// Converse tab, end to end (Ethan 2026-10-11: "chat with your worker ... hands
// free ... a log of the interactions ... replay any ... an expiration", then
// "more realtime and it shouldnt interrupt the worker", and OpenAI Realtime as
// the default engine).
//
// WHAT IS REAL: the dashboard, a registered worker on this server, the
// Converse storage API (save, audio upload, list, read, delete, retention
// sweep), the real realtime-session mint route (this server has no
// OPENAI_API_KEY, so it must refuse and the tab must fall back), MediaRecorder
// recording a real MediaStream, and the replay player.
//
// WHAT IS STUBBED, and why:
// - Speech recognition, speech output and the microphone: a headless browser
//   cannot hear or speak. The mic is a real oscillator MediaStream.
// - The worker's chat companion (`<worker>@chat`): the golden servers do not
//   run model turns. Its /send answers with the server's acceptance shape and
//   its /chat/stream serves the real event shapes (`user`, `delta`, `done`
//   with seq/epoch), released in two steps so the test can see speech start
//   before the reply has finished arriving.
// - OpenAI: no request leaves the machine and nothing is spent. The mint
//   route, the WebRTC SDP POST to api.openai.com and RTCPeerConnection itself
//   are stubbed; the test plays the Realtime server events into the data
//   channel.
// The worker's own terminal send route is watched and must never be called.
import { test, expect, allowUnusedRoute } from './fixtures';
import type { Page, Route } from '@playwright/test';
import { cleanup as safeCleanup } from './teardown';

const REPLY_A = 'The deploy finished ten minutes ago.';
const REPLY_B = ' All checks are green and nothing is waiting on you.';
// Streamed as two deltas that join to exactly REPLY_A + REPLY_B.
const CHAT_ANSWER = 'It is refactoring the board drain loop; tests are green.';

async function installSpeechStubs(page: Page) {
  await page.addInitScript(() => {
    const w = window as any;
    w.__spoken = [];
    w.__spokenAt = [];
    w.__cancelled = 0;
    class FakeRecognition {
      continuous = false; interimResults = false; lang = '';
      onresult: any = null; onend: any = null; onerror: any = null; started = false; results: any[] = [];
      start() { if (this.started) throw new Error('already started'); this.started = true; this.results = []; w.__rec = this; }
      stop() { this.abort(); }
      abort() { if (!this.started) return; this.started = false; setTimeout(() => this.onend && this.onend(), 0); }
    }
    w.SpeechRecognition = FakeRecognition;
    w.webkitSpeechRecognition = FakeRecognition;
    // Results accumulate across a recognition session, as in a real browser.
    w.__say = (text: string) => {
      const r = w.__rec;
      if (!r || !r.started) return false;
      const res: any = [{ transcript: text, confidence: 0.95 }];
      res.isFinal = true;
      r.results.push(res);
      r.onresult({ resultIndex: r.results.length - 1, results: r.results });
      return true;
    };
    let current: any = null;
    const queue: any[] = [];
    const next = () => {
      if (current || !queue.length) return;
      const u = current = queue.shift();
      w.__spoken.push(u.text); w.__spokenAt.push(Date.now());
      u.onstart && u.onstart({});
      u.__t = setTimeout(() => { if (current === u) current = null; u.onend && u.onend({}); next(); }, 600);
    };
    const synth = {
      speaking: false, pending: false, paused: false, onvoiceschanged: null,
      getVoices: () => [], addEventListener() {}, removeEventListener() {},
      cancel() { w.__cancelled++; queue.length = 0; const u = current; current = null; if (u) { clearTimeout(u.__t); u.onend && u.onend({}); } },
      speak(u: any) { queue.push(u); next(); },
    };
    Object.defineProperty(w, 'speechSynthesis', { value: synth, configurable: true });
    if (!w.SpeechSynthesisUtterance) w.SpeechSynthesisUtterance = class { text: string; constructor(t: string) { this.text = t; } };
    w.__tone = () => {
      const Ctx = w.AudioContext || w.webkitAudioContext;
      const ctx = new Ctx();
      const osc = ctx.createOscillator();
      osc.frequency.value = 220;
      const dest = ctx.createMediaStreamDestination();
      osc.connect(dest); osc.start();
      return dest.stream;
    };
    if (navigator.mediaDevices) {
      (navigator.mediaDevices as any).getUserMedia = async () => w.__tone();
      (navigator.mediaDevices as any).enumerateDevices = async () => [];
    }
  });
}

/** The worker's chat companion, with the real stream event shapes. */
function chatStub(page: Page, worker: string, opts: { reply: (text: string) => string[] }) {
  const state = { sends: [] as string[], terminal: 0, released: [] as any[], seq: 0, doneReleasedAt: 0, waiters: [] as (() => void)[] };
  const epoch = 'E2E' + Date.now();
  const push = (ev: any) => { state.seq++; state.released.push({ ...ev, seq: state.seq, epoch }); state.waiters.splice(0).forEach(f => f()); };
  const isChat = (u: URL, tail: string) => decodeURIComponent(u.pathname) === `/api/sessions/${worker}@chat/${tail}`;
  return {
    state,
    async install() {
      // The worker's own terminal: counted, and must stay at zero.
      const terminal = new RegExp(`/api/sessions/${worker}/send(\\?|$)`);
      await page.route(terminal, async (route: Route) => {
        state.terminal++;
        await route.fulfill({ status: 500, contentType: 'application/json', body: '{"error":"terminal send must not be used"}' });
      });
      // Zero hits is the assertion here, not a broken stub.
      allowUnusedRoute(page, terminal);
      await page.route(u => isChat(new URL(u), 'send'), async (route: Route) => {
        if (route.request().method() !== 'POST') {
          const id = new URL(route.request().url()).searchParams.get('msg_id') || '';
          await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ ok: true, accepted: true, msg_id: id, id: 'chat:' + id }) });
          return;
        }
        const body = JSON.parse(route.request().postData() || '{}');
        const fresh = !state.sends.includes(body.msg_id + ' ' + body.text);
        if (fresh) {
          state.sends.push(body.msg_id + ' ' + body.text);
          const turn = 't' + state.sends.length;
          const parts = opts.reply(String(body.text || ''));
          push({ type: 'user', turn_id: turn, text: body.text, msg_id: body.msg_id, origin: 'owner' });
          push({ type: 'delta', turn_id: turn, text: parts[0] });
          // The rest arrives later, so speech can be seen starting first.
          setTimeout(() => {
            for (const p of parts.slice(1)) push({ type: 'delta', turn_id: turn, text: p });
            push({ type: 'done', turn_id: turn, message: { role: 'assistant', text: parts.join(''), turn_id: turn } });
          }, 2500);
        }
        await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ ok: true, submitted: true, submission: 'confirmed', msg_id: body.msg_id }) });
      });
      await page.route(u => isChat(new URL(u), 'chat/stream'), async (route: Route) => {
        const after = new URL(route.request().url()).searchParams.get('after') || '';
        const from = after.startsWith(epoch + ':') ? Number(after.split(':')[1]) : 0;
        const pending = () => state.released.filter(e => e.seq > from);
        if (!pending().length) await new Promise<void>(res => { state.waiters.push(res); setTimeout(res, 1500); });
        const evs = pending();
        if (evs.some(e => e.type === 'done') && !state.doneReleasedAt) state.doneReleasedAt = Date.now();
        const body = [`data: ${JSON.stringify({ type: 'hello', cursor: `${epoch}:${state.seq}` })}\n\n`]
          .concat(evs.map(e => `id: ${epoch}:${e.seq}\ndata: ${JSON.stringify(e)}\n\n`)).join('');
        await route.fulfill({ status: 200, headers: { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache' }, body }).catch(() => {});
      });
    },
  };
}

async function boot(page: Page, request: any, testInfo: any, tag: string) {
  await installSpeechStubs(page);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any)._vxRender === 'function');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const uiToken = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN as string);
  expect(token, 'served bootstrap must provide the API token').toBeTruthy();
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const worker = `vx-${tag}-${testInfo.project.name}-${Date.now()}`;
  const created = await request.post('/api/sessions', { headers: auth, data: { name: worker, dir: '/tmp', start: false } });
  expect(created.status(), 'create the worker').toBe(201);
  // Through e2e/teardown.ts: a teardown failure must never replace the
  // test's own error (AMUX-4737).
  const cleanup = async () => {
    await safeCleanup('close peek', () => page.evaluate(() => (window as any).closePeek?.()));
    await safeCleanup('delete fixture worker', () => request.post(`/api/sessions/${encodeURIComponent(worker)}/delete`,
      { headers: { ...auth, 'X-Amux-UI-Token': uiToken }, data: {} }));
  };
  return { auth, worker, cleanup };
}

async function openConverse(page: Page, worker: string) {
  await page.evaluate(async () => { await (window as any).fetchSessions?.(); });
  await page.evaluate(name => (window as any).openPeek(name), worker);
  const tab = page.locator('#peek-tab-converse');
  await expect(tab).toBeVisible();
  await tab.click();
  const panel = page.locator('#peek-converse-panel');
  await expect(panel).toHaveClass(/active/);
  await expect(panel.locator('.vx-title')).toHaveText('Chat with your worker');
  return panel;
}

test('Converse (browser fallback): streamed chat replies are spoken as they arrive, the worker is never interrupted, and the log replays, deletes and expires', async ({ page, request }, testInfo) => {
  test.setTimeout(150_000);
  const { auth, worker, cleanup } = await boot(page, request, testInfo, 'br');
  const conv = `/api/converse/${worker}/conversations`;
  const chat = chatStub(page, worker, { reply: () => [REPLY_A, REPLY_B] });
  await chat.install();
  try {
    // ── no OpenAI key on this server: the mint refuses, leaks nothing ────
    const cfg = await (await request.get(`/api/converse/config?worker=${worker}`, { headers: auth })).json();
    expect(cfg.measured).toBe(true);
    expect(cfg.retention_days).toBe(30);
    const oa = cfg.engines.find((e: any) => e.id === 'openai');
    expect(oa.available, 'this server has no OPENAI_API_KEY').toBe(false);
    expect(cfg.engines.some((e: any) => e.id === 'personaplex'), 'PersonaPlex is gone').toBe(false);
    const mint = await request.post(`/api/converse/${worker}/realtime-session`, { headers: auth });
    expect(mint.status()).toBe(503);
    const mintText = await mint.text();
    expect(JSON.parse(mintText).fallback).toBe('browser');
    expect(mintText).not.toMatch(/sk-[A-Za-z0-9]/);

    const panel = await openConverse(page, worker);
    await expect(panel.locator('#vx-retention-line')).toContainText('stored for 30 days');
    expect(await panel.locator('#vx-engine option[value="openai"]').evaluate(o => (o as HTMLOptionElement).disabled)).toBe(true);
    await expect(panel.locator('#vx-engine')).toHaveValue('browser');
    await expect(panel.locator('#vx-engine-note')).toContainText('never interrupts the worker');

    // ── a hands-free conversation through the chat companion ─────────────
    await page.locator('#vx-call').click();
    await expect(page.locator('#vx-call-lbl')).toHaveText('End');
    await page.waitForFunction(() => (window as any).__rec?.started === true);
    const saidAt = Date.now();
    expect(await page.evaluate(() => (window as any).__say('Has the deploy finished?'))).toBe(true);
    await expect.poll(() => chat.state.sends.length, { message: 'the utterance must reach the chat companion', timeout: 10_000 }).toBe(1);
    expect(chat.state.sends[0]).toContain('Has the deploy finished?');
    const sendLatency = Date.now() - saidAt;
    // Speech starts on the first sentence, before the rest of the reply exists.
    await expect.poll(() => page.evaluate(() => (window as any).__spoken as string[]), { timeout: 10_000 }).toContain(REPLY_A);
    const firstSpokenAt: number = await page.evaluate(() => (window as any).__spokenAt[0]);
    await expect.poll(() => chat.state.doneReleasedAt, { timeout: 10_000 }).toBeGreaterThan(0);
    expect(firstSpokenAt, 'speech started before the full reply had arrived').toBeLessThan(chat.state.doneReleasedAt);
    await expect(page.locator('#vx-live')).toContainText(REPLY_B.trim(), { timeout: 10_000 });
    await expect.poll(() => page.evaluate(() => (window as any).__spoken as string[]), { timeout: 10_000 }).toContain(REPLY_B.trim());
    // Barge-in: talking over the voice stops it.
    const cancelledBefore = await page.evaluate(() => (window as any).__cancelled as number);
    await page.evaluate(() => (window as any).__say('And what about the tests?'));
    await expect.poll(() => chat.state.sends.length, { timeout: 10_000 }).toBe(2);
    await expect.poll(() => page.evaluate(() => (window as any).__spoken.length as number), { timeout: 10_000 }).toBeGreaterThan(2);
    await page.evaluate(() => (window as any).__say('Never mind.'));
    await expect.poll(() => page.evaluate(() => (window as any).__cancelled as number)).toBeGreaterThan(cancelledBefore);
    await page.waitForTimeout(3500);
    await page.locator('#vx-call').click();
    await expect(page.locator('#vx-call-lbl')).toHaveText('Start');
    expect(chat.state.terminal, 'the worker terminal send path was never called').toBe(0);

    // ── the log: duration, expiry, latency ───────────────────────────────
    const row = panel.locator('.vx-row').first();
    await expect(row).toContainText('Has the deploy finished?', { timeout: 15_000 });
    await expect(row.locator('.vx-row-meta')).toContainText(/\d+s/);
    await expect(row.locator('.vx-row-exp')).toContainText('Expires in 30 days');
    const listed = await (await request.get(conv, { headers: auth })).json();
    expect(listed.measured).toBe(true);
    expect(listed.n).toBe(1);
    const rec = listed.conversations[0];
    expect(rec.engine).toBe('browser');
    const full = await (await request.get(`${conv}/${rec.id}`, { headers: auth })).json();
    const workerTurns = full.turns.filter((t: any) => t.role === 'worker');
    expect(workerTurns[0].text).toBe(REPLY_A + REPLY_B);
    expect(workerTurns[0].first_audio_ms, 'first-audio latency is recorded per reply').toBeGreaterThanOrEqual(0);
    expect(rec.latency.n).toBeGreaterThanOrEqual(1);
    console.log(`[converse] browser engine: utterance to chat send ${sendLatency}ms; first audio ${workerTurns.map((t: any) => t.first_audio_ms).join(', ')}ms; median ${rec.latency.median_ms}ms`);
    testInfo.annotations.push({ type: 'first_audio_ms', description: String(rec.latency.median_ms) });
    const canRecord = testInfo.project.name !== 'ios-safari'
      && await page.evaluate(() => typeof (window as any).MediaRecorder === 'function');
    if (!rec.audio) expect(canRecord, 'audio missing on a browser that records').toBe(false);
    if (canRecord) {
      expect(rec.audio.bytes).toBeGreaterThan(0);
      const audio = await request.get(`${conv}/${rec.id}/audio`, { headers: auth });
      expect(audio.status()).toBe(200);
      expect((await audio.body()).length).toBe(rec.audio.bytes);
    }

    // ── replay ──────────────────────────────────────────────────────────
    const player = panel.locator('#vx-player');
    await expect(player.locator('.vx-pexp')).toHaveText('Expires in 30 days');
    await expect(player.locator('.vx-pturn')).toHaveCount(full.turns.length);
    const workerTurn = player.locator('.vx-pturn', { hasText: REPLY_A }).first();
    await expect(workerTurn.locator('.vx-lat')).toContainText('first audio');
    const spokenBefore = (await page.evaluate(() => (window as any).__spoken as string[])).length;
    await page.locator('#vx-pplay').click();
    await expect(page.locator('#vx-pplay')).toHaveAttribute('aria-label', 'Pause');
    await workerTurn.click();
    await expect.poll(async () => (await page.evaluate(() => (window as any).__spoken as string[])).slice(spokenBefore).join(' '),
      { message: 'replay speaks the worker turn', timeout: 10_000 }).toContain('deploy finished');
    await page.locator('#vx-speed').click();
    await expect(page.locator('#vx-speed')).toContainText('1.25');
    await page.locator('#vx-pplay').click();
    await expect(page.locator('#vx-pplay')).toHaveAttribute('aria-label', 'Play');
    await player.getByRole('button', { name: 'Details' }).click();
    await expect(player.locator('.vx-pbody')).toContainText('First audio');

    // ── layout ───────────────────────────────────────────────────────────
    const geo = await page.evaluate(() => {
      const p = document.getElementById('peek-converse-panel')!;
      const box = (s: string) => { const r = document.querySelector(s)!.getBoundingClientRect(); return { w: r.width, h: r.height }; };
      return { overflow: p.scrollWidth - p.clientWidth, call: box('#vx-call'), play: box('.vx-row-play'), mute: box('#vx-mute') };
    });
    expect(geo.overflow, 'no horizontal overflow in the Converse panel').toBeLessThanOrEqual(1);
    for (const k of ['call', 'play', 'mute'] as const) {
      expect(geo[k].w, `${k} width`).toBeGreaterThanOrEqual(44);
      expect(geo[k].h, `${k} height`).toBeGreaterThanOrEqual(44);
    }

    // ── delete, then expiry ──────────────────────────────────────────────
    await row.locator('.vx-row-more').click();
    await row.getByRole('menuitem', { name: 'Delete' }).click();
    await row.getByRole('menuitem', { name: 'Tap again to delete' }).click();
    await expect(panel.locator('.vx-row')).toHaveCount(0, { timeout: 10_000 });
    expect((await request.get(`${conv}/${rec.id}`, { headers: auth })).status()).toBe(404);
    const oldId = `vx-old-${Date.now()}`;
    const old = await request.post(conv, { headers: auth, data: {
      id: oldId, title: 'Old chat', engine: 'browser', started_at: Date.now() - 40 * 86_400_000,
      turns: [{ role: 'user', text: 'hello from last month', t_ms: 0 }],
    } });
    expect(old.status()).toBe(200);
    const hidden = await (await request.get(conv, { headers: auth })).json();
    expect(hidden.conversations.map((c: any) => c.id)).not.toContain(oldId);
    expect(hidden.expired_pending_sweep).toBe(1);
    expect((await request.get(`${conv}/${oldId}`, { headers: auth })).status()).toBe(410);
    const swept = await (await request.post('/api/converse/sweep', { headers: auth })).json();
    expect(swept.measured).toBe(true);
    expect(swept.verdict).toBe('removed');
    const after = await (await request.get(conv, { headers: auth })).json();
    expect(after.n_considered).toBe(0);
  } finally {
    await cleanup();
  }
});

test('Converse (OpenAI Realtime): ask_worker goes to the chat companion, usage and latency are logged, and a failed mint falls back', async ({ page, request }, testInfo) => {
  test.setTimeout(120_000);
  // RTCPeerConnection, stubbed: the data channel opens once the answer is
  // set, and the test plays the Realtime server events into it.
  await page.addInitScript(() => {
    const w = window as any;
    w.__dcSent = [];
    class FakeDC {
      label: string; readyState = 'connecting'; onmessage: any = null; onopen: any = null;
      constructor(l: string) { this.label = l; w.__dc = this; }
      send(s: string) { w.__dcSent.push(JSON.parse(s)); }
      close() { this.readyState = 'closed'; }
    }
    class FakePC {
      ontrack: any = null; tracks: any[] = [];
      constructor() { w.__pc = this; }
      addTrack(t: any) { this.tracks.push(t); }
      addTransceiver() {}
      createDataChannel(l: string) { return new FakeDC(l); }
      async createOffer() { return { type: 'offer', sdp: 'v=0 e2e-offer' }; }
      async setLocalDescription() {}
      async setRemoteDescription(d: any) {
        w.__answer = d.sdp;
        setTimeout(() => {
          const st = w.__tone();
          this.ontrack && this.ontrack({ streams: [st], track: st.getAudioTracks()[0] });
          w.__dc.readyState = 'open';
          w.__dc.onopen && w.__dc.onopen({});
        }, 50);
      }
      close() {}
    }
    w.RTCPeerConnection = FakePC;
    w.__rt = (ev: any) => w.__dc.onmessage({ data: JSON.stringify(ev) });
  });
  const { auth, worker, cleanup } = await boot(page, request, testInfo, 'rt');
  const conv = `/api/converse/${worker}/conversations`;
  const chat = chatStub(page, worker, { reply: () => [CHAT_ANSWER] });
  await chat.install();
  let mintFails = true;
  const pageErrors: string[] = [];
  page.on('pageerror', e => pageErrors.push(String(e)));
  const sdp: { auth: string; type: string }[] = [];
  await page.route('**/api/converse/config*', r => r.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({
    measured: true, n_considered: 2, retention_days: 30, retention_source: 'default',
    engines: [{ id: 'openai', label: 'OpenAI Realtime', available: true, default: true, model: 'gpt-realtime-2.1', voice: 'marin', reason: 'stub' },
              { id: 'browser', label: 'Browser speech', available: true }] }) }));
  await page.route(`**/api/converse/${worker}/realtime-session`, r => mintFails
    ? r.fulfill({ status: 502, contentType: 'application/json', body: JSON.stringify({ ok: false, fallback: 'browser', reason: 'OpenAI refused the session (401): stub' }) })
    : r.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({
        ok: true, client_secret: 'ek_e2e_stub', expires_at: 0, model: 'gpt-realtime-2.1', voice: 'marin',
        calls_url: 'https://api.openai.com/v1/realtime/calls', tool: 'ask_worker' }) }));
  // A fulfilled response is still subject to CORS, as OpenAI's real one is.
  const cors = { 'Access-Control-Allow-Origin': '*', 'Access-Control-Allow-Headers': 'authorization, content-type', 'Access-Control-Allow-Methods': 'POST, OPTIONS' };
  await page.route('https://api.openai.com/**', async r => {
    if (r.request().method() === 'OPTIONS') { await r.fulfill({ status: 204, headers: cors }); return; }
    sdp.push({ auth: r.request().headers()['authorization'] || '', type: r.request().headers()['content-type'] || '' });
    await r.fulfill({ status: 201, headers: { ...cors, 'Content-Type': 'application/sdp' }, body: 'v=0 e2e-answer' });
  });
  try {
    const panel = await openConverse(page, worker);
    await expect(panel.locator('#vx-engine')).toHaveValue('openai');
    await expect(panel.locator('#vx-powered')).toContainText('OpenAI Realtime');

    // ── a failed mint falls back to browser speech ───────────────────────
    await page.locator('#vx-call').click();
    await expect(page.locator('#vx-live')).toContainText('Using browser speech instead', { timeout: 10_000 });
    await expect(panel.locator('#vx-powered')).toContainText('Browser speech');
    await page.waitForFunction(() => (window as any).__rec?.started === true);
    await page.locator('#vx-call').click();
    await expect(page.locator('#vx-call-lbl')).toHaveText('Start');

    // ── the Realtime session ─────────────────────────────────────────────
    mintFails = false;
    await page.locator('#vx-call').click();
    await page.waitForFunction(() => (window as any).__dc?.readyState === 'open', null, { timeout: 10_000 }).catch(async e => {
      console.log('[converse-debug]', JSON.stringify(sdp), pageErrors.join(' | '), await page.evaluate(() => { const v = (0, eval)('_vx'); return JSON.stringify({ active: v.active, ending: v.ending, engine: v.engine, state: v.state, id: v.id, peek: (0, eval)('peekSession'), lbl: document.getElementById('vx-call-lbl')?.textContent, turns: (v.turns || []).map((t: any) => t.text) }); }));
      throw e;
    });
    await expect(panel.locator('#vx-powered')).toContainText('OpenAI Realtime');
    expect(sdp[0], 'the SDP offer goes to OpenAI with the minted client secret').toEqual({ auth: 'Bearer ek_e2e_stub', type: 'application/sdp' });
    expect(await page.evaluate(() => (window as any).__answer)).toBe('v=0 e2e-answer');
    expect(await page.evaluate(() => (window as any).__rec?.started === true), 'browser recognition is off under Realtime').toBe(false);
    const rt = (ev: any) => page.evaluate(e => (window as any).__rt(e), ev);
    await rt({ type: 'input_audio_buffer.speech_started', item_id: 'i1' });
    await page.waitForTimeout(300);
    await rt({ type: 'input_audio_buffer.speech_stopped', item_id: 'i1' });
    await rt({ type: 'conversation.item.input_audio_transcription.completed', item_id: 'i1', transcript: 'What is the worker doing right now?' });
    await rt({ type: 'response.function_call_arguments.done', call_id: 'call_1', name: 'ask_worker', arguments: JSON.stringify({ request: 'What are you working on?' }) });
    await expect.poll(() => chat.state.sends.length, { message: 'ask_worker reaches the chat companion', timeout: 10_000 }).toBe(1);
    expect(chat.state.sends[0]).toContain('What are you working on?');
    await expect.poll(async () => (await page.evaluate(() => (window as any).__dcSent as any[])).map(e => e.type), { timeout: 10_000 })
      .toEqual(['conversation.item.create', 'response.create']);
    const out = (await page.evaluate(() => (window as any).__dcSent as any[]))[0];
    expect(out.item.type).toBe('function_call_output');
    expect(out.item.call_id).toBe('call_1');
    expect(JSON.parse(out.item.output).reply).toBe(CHAT_ANSWER);
    await page.waitForTimeout(250);
    await rt({ type: 'output_audio_buffer.started', response_id: 'r2' });
    await rt({ type: 'response.output_audio_transcript.delta', response_id: 'r2', item_id: 'o2', delta: 'It is refactoring the board drain loop' });
    await rt({ type: 'response.output_audio_transcript.done', response_id: 'r2', item_id: 'o2', transcript: 'It is refactoring the board drain loop, and the tests are green.' });
    await page.waitForTimeout(400);
    await rt({ type: 'output_audio_buffer.stopped', response_id: 'r2' });
    await rt({ type: 'response.done', response: { id: 'r2', output: [], usage: {
      input_token_details: { audio_tokens: 120, text_tokens: 800, cached_tokens: 0 }, output_token_details: { audio_tokens: 300, text_tokens: 40 } } } });
    await expect(page.locator('#vx-live')).toContainText('tests are green');
    await page.locator('#vx-call').click();
    await expect(page.locator('#vx-call-lbl')).toHaveText('Start');
    expect(chat.state.terminal, 'the worker terminal send path was never called').toBe(0);

    // Hanging up queues the FINAL save behind any mid-call one (_vxSave
    // serialises them), and it lands after the click resolves. Reading once
    // could see an earlier save with no usage yet (CI 2026-10-16, f3ad69cc4:
    // Expected 120, Received 0), so wait for the final record.
    let full: any = null;
    await expect.poll(async () => {
      const listed = await (await request.get(conv, { headers: auth })).json();
      const rec = listed.conversations.find((c: any) => c.engine === 'openai');
      if (!rec) return 'not logged yet';
      full = await (await request.get(`${conv}/${rec.id}`, { headers: auth })).json();
      return full.ended_at ? 'final' : 'mid-call save';
    }, { message: 'the Realtime conversation is logged with its final save', timeout: 15_000 }).toBe('final');
    expect(full.usage.input_audio_tokens).toBe(120);
    expect(full.usage.output_audio_tokens).toBe(300);
    expect(full.usage.audio_in_ms).toBeGreaterThan(0);
    expect(full.usage.audio_out_ms).toBeGreaterThan(0);
    expect(full.usage.tool_calls).toBe(1);
    const roles = full.turns.map((t: any) => t.role);
    const userAt = roles.indexOf('user');
    const askAt = full.turns.findIndex((t: any) => t.role === 'system' && t.text.startsWith('Asked '));
    expect(userAt, 'the transcribed question is logged before the tool call: ' + JSON.stringify(full.turns)).toBeGreaterThanOrEqual(0);
    expect(userAt).toBeLessThan(askAt);
    const reply = full.turns.find((t: any) => t.role === 'worker');
    expect(reply.text).toContain('tests are green');
    expect(reply.first_audio_ms, 'Realtime first-audio latency recorded').toBeGreaterThanOrEqual(0);
    console.log(`[converse] realtime engine (stubbed transport, includes the ask_worker round trip): first audio ${reply.first_audio_ms}ms`);
    expect(full.turns.some((t: any) => t.role === 'system' && t.text.includes(CHAT_ANSWER))).toBe(true);
  } finally {
    await cleanup();
  }
});
