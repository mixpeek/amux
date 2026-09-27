#!/usr/bin/env node
// Chat-mode streaming, end to end (AMUX-5263).
//
// Boots THIS checkout's amux-server against a throwaway AMUX_HOME on its own
// port (the e2e harness shape, serve-head.sh), with the chat adapter's claude
// binary swapped for e2e/fixtures/fake-claude-stream.py, which replays the
// real stream-json event shapes at a watchable pace. Then drives the real
// dashboard (this checkout's index.html/app.js/app.css) in Chrome at 390px and
// 1280px and checks what a person would see:
//   1. text arrives incrementally (sampled lengths strictly grow mid-stream)
//   2. the tool card and thinking appear live, collapsed
//   3. a forced mid-stream reconnect neither drops nor repeats a delta
//   4. the finished bubble renders exactly renderMarkdown(message.text)
//   5. Stop interrupts a running turn and the reply is kept, marked stopped
//
// usage: node e2e/chat-streaming.mjs <amux-server binary> <screenshot dir>
// Playwright is resolved from AMUX_PW_ROOT (a dir with node_modules), else here.
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const req = createRequire(path.join(process.env.AMUX_PW_ROOT || repo, 'package.json'));
const { chromium } = req('playwright');
const [bin, shots] = process.argv.slice(2);
if (!bin || !shots) { console.error('usage: chat-streaming.mjs <server-bin> <shot-dir>'); process.exit(2); }
fs.mkdirSync(shots, { recursive: true });
process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0';

const port = 18000 + Math.floor(Math.random() * 800);
const base = `https://localhost:${port}`;
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-chat-e2e-'));
const env = {};
for (const [k, v] of Object.entries(process.env)) if (!k.startsWith('AMUX_')) env[k] = v;
Object.assign(env, {
  AMUX_HOME: home, AMUX_RS_PORT: String(port), AMUX_NO_SELF_ADOPT: '1',
  AMUX_CHAT_CLAUDE_BIN: path.join(repo, 'e2e/fixtures/fake-claude-stream.py'),
  // Slow enough that a loaded CI box still samples several frames per turn.
  FAKE_CLAUDE_DELAY_MS: process.env.FAKE_CLAUDE_DELAY_MS || '80',
});
const server = spawn(bin, [], { env, stdio: ['ignore', fs.openSync(path.join(shots, 'server.log'), 'w'), 'pipe'] });
let serverErr = '';
server.stderr.on('data', d => { serverErr = (serverErr + d).slice(-4000); });

let failures = 0;
const pass = m => console.log('PASS ' + m);
const fail = m => { failures++; console.log('FAIL ' + m); };
const check = (ok, m) => (ok ? pass(m) : fail(m));
const sleep = ms => new Promise(r => setTimeout(r, ms));
const api = async (p, body) => {
  const r = await fetch(base + p, body === undefined ? {} : {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
  const t = await r.text();
  try { return { status: r.status, body: JSON.parse(t) }; } catch (e) { return { status: r.status, body: t }; }
};

async function up() {
  for (let i = 0; i < 120; i++) {
    try { if ((await fetch(base + '/health')).ok) return; } catch (e) {}
    await sleep(500);
  }
  throw new Error('server did not come up: ' + serverErr);
}

async function openChat(browser, width, name) {
  const ctx = await browser.newContext({ viewport: { width, height: width < 600 ? 844 : 860 },
    ignoreHTTPSErrors: true, serviceWorkers: 'block', deviceScaleFactor: width < 600 ? 2 : 1 });
  const page = await ctx.newPage();
  const stat = p => path.join(repo, 'crates/amux-dashboard/static', p);
  for (const a of ['app.js', 'app.css', 'state/kernel.js'])
    await page.route('**/' + a, r => r.fulfill({ path: stat(a), contentType: a.endsWith('.css') ? 'text/css' : 'text/javascript' }));
  await page.route(base + '/', r => r.fulfill({ path: stat('index.html'), contentType: 'text/html' }));
  // The shell loads ~10 blocking scripts from jsdelivr. On a slow network that
  // held DOMContentLoaded past 60s (measured 2026-09-26: 10s for one file), a
  // failure about the network rather than chat. Cache them across runs.
  const cdnCache = path.join(os.tmpdir(), 'amux-e2e-cdn-cache');
  fs.mkdirSync(cdnCache, { recursive: true });
  await page.route('https://cdn.jsdelivr.net/**', async r => {
    const f = path.join(cdnCache, r.request().url().replace(/[^A-Za-z0-9._-]/g, '_').slice(-180));
    if (fs.existsSync(f)) return r.fulfill({ path: f, contentType: f.includes('.css') ? 'text/css' : 'text/javascript' });
    // Bounded: a CDN file that will not arrive becomes an empty script (and
    // is named), not a hung page. Nothing chat renders depends on one; Motion
    // no-ops when absent by design.
    try {
      const res = await r.fetch({ timeout: 20000 });
      if (res.ok()) fs.writeFileSync(f, await res.body());
      return r.fulfill({ response: res });
    } catch (e) {
      console.log('cdn: gave up on ' + r.request().url() + ' (' + e.message.split('\n')[0] + ')');
      return r.fulfill({ status: 200, body: '', contentType: 'text/javascript' });
    }
  });
  // Any other third-party host (analytics, fonts) is not under test.
  await page.route(u => !u.href.startsWith(base) && !u.href.startsWith('https://cdn.jsdelivr.net/'), r => r.abort());
  page.on('pageerror', e => console.log('pageerror:', e.message));
  // domcontentloaded: the page's own long-poll and CDN assets can hold 'load' open on a busy box.
  await page.goto(base + '/', { waitUntil: 'domcontentloaded', timeout: 60000 });
  await page.waitForFunction(n => typeof sessions !== 'undefined' && sessions.some(s => s.name === n), name, { timeout: 30000 });
  await page.evaluate(n => openPeek(n), name);
  await page.waitForSelector('#peek-body.peek-chat .chat-log', { timeout: 15000 });
  // Record the live-assembled text at the moment each turn finishes, so the
  // stream can be compared with what the server persisted.
  await page.evaluate(() => {
    window.__liveAtDone = [];
    const orig = _chatOnEvent;
    _chatOnEvent = function (name, m) {
      if (m.type === 'done' && _chat.streaming) window.__liveAtDone.push({ live: _chat.streaming.text, final: m.message.text });
      return orig.apply(this, arguments);
    };
  });
  return { ctx, page };
}

async function streamTurn(page, name, width, label) {
  const before = await page.evaluate(() => window.__liveAtDone.length);
  const sent = await api(`/api/sessions/${name}/send`, { text: 'What crates are in this workspace?' });
  check(sent.status < 300, `${label}: send accepted (${sent.status})`);
  await page.waitForSelector('.chat-msg.is-streaming', { timeout: 15000 });
  const lengths = [];
  let shotTool = false, shotMid = false, reconnected = false;
  for (let i = 0; i < 400; i++) {
    const s = await page.evaluate(() => {
      const live = document.querySelector('.chat-msg.is-streaming');
      return live ? { len: (live.querySelector('.chat-live-md') || {}).textContent?.length || 0,
        tools: live.querySelectorAll('.chat-tool').length, thinking: !!live.querySelector('.chat-thinking'),
        text: _chat.streaming ? _chat.streaming.text.length : 0 } : null;
    });
    if (!s) break;
    lengths.push(s.len);
    if (!shotTool && s.tools > 0) {
      shotTool = true;
      await page.screenshot({ path: path.join(shots, `${width}-1-tool-card.png`) });
      check(s.thinking, `${label}: thinking shown (collapsed) while streaming`);
    }
    if (!reconnected && s.text > 60) {
      // Drop the stream mid-reply and reconnect from the last applied cursor.
      reconnected = true;
      await page.evaluate(n => { _chat.es.close(); _chatConnect(n); }, name);
    }
    if (!shotMid && s.len > 180) {
      shotMid = true;
      await page.screenshot({ path: path.join(shots, `${width}-2-mid-stream.png`) });
    }
    await sleep(60);
  }
  const grew = lengths.filter((v, i) => i > 0 && v > lengths[i - 1]).length;
  check(grew >= 5, `${label}: text appeared incrementally (${grew} growth steps over ${lengths.length} samples)`);
  await page.waitForFunction(b => window.__liveAtDone.length > b, before, { timeout: 30000 });
  const d = await page.evaluate(() => window.__liveAtDone[window.__liveAtDone.length - 1]);
  check(reconnected, `${label}: forced a mid-stream reconnect`);
  check(d.live === d.final, `${label}: streamed text after reconnect equals persisted text (${d.live.length}/${d.final.length} chars, no dup, no gap)`);
  const same = await page.evaluate(() => {
    const m = _chat.messages[_chat.messages.length - 1];
    const bubble = [...document.querySelectorAll('.chat-msg.chat-assistant .chat-bubble')].pop();
    return bubble.innerHTML.endsWith(renderMarkdown(m.text)) && (m.tool_calls || []).length === 1;
  });
  check(same, `${label}: finished bubble is exactly renderMarkdown(text), tool call persisted`);
  await page.screenshot({ path: path.join(shots, `${width}-3-done.png`) });
}

async function interruptTurn(page, name, label) {
  const sent = await api(`/api/sessions/${name}/send`, { text: 'Now a long answer please.' });
  check(sent.status < 300, `${label}: second send accepted`);
  await page.waitForFunction(() => _chat.streaming && _chat.streaming.text.length > 100, null, { timeout: 30000 });
  await page.click('.chat-stop');
  await page.waitForFunction(() => { const m = _chat.messages[_chat.messages.length - 1]; return m && m.role === 'assistant' && m.interrupted; }, null, { timeout: 15000 });
  const m = await page.evaluate(() => _chat.messages[_chat.messages.length - 1]);
  check(m.interrupted && !m.error && m.text.length > 100, `${label}: Stop interrupted the turn, partial reply kept (${m.text.length} chars), no error`);
  await page.screenshot({ path: path.join(shots, 'interrupted.png') });
}

try {
  await up();
  pass('worktree server up on ' + port);
  const name = 'chat-e2e';
  const c = await api('/api/sessions', { name, worker_type: 'chat', provider: 'claude' });
  check(c.status < 300, `created chat worker (${c.status} ${JSON.stringify(c.body).slice(0, 120)})`);
  const st = await api(`/api/sessions/${name}/start`, {});
  check(st.status < 300, `started chat worker (${st.status})`);
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  const mobile = await openChat(browser, 390, name);
  await streamTurn(mobile.page, name, 390, '390px');
  const desk = await openChat(browser, 1280, name);
  await streamTurn(desk.page, name, 1280, '1280px');
  await interruptTurn(desk.page, name, '1280px');
  const hist = await api(`/api/sessions/${name}/chat?limit=10`);
  check(hist.body.stream_ring && hist.body.stream_ring.retained_events > 0, `history reports the bounded ring ${JSON.stringify(hist.body.stream_ring)}`);
  await browser.close();
} catch (e) {
  fail('exception: ' + (e && e.stack || e));
} finally {
  server.kill('SIGTERM');
}
console.log(failures ? `${failures} FAILED` : 'ALL PASSED');
process.exit(failures ? 1 : 0);
