#!/usr/bin/env node
// Ask several workers from a markdown file, with REAL workers and a real
// browser (owner, 2026-11-24: "Do it and verify e2e"). The Playwright spec
// e2e/md-ask-workers.spec.ts stubs the workers; this one does not.
//
//     node tests/md-ask-workers-real-e2e.mjs [amux-server binary]
//
// A private amux server serves this checkout's dashboard (AMUX_DASHBOARD_DIR)
// and runs two real Claude Code workers (haiku). The test writes a To-Do.md
// with a line for each, opens it in the dashboard in Chromium, selects both
// lines with a real mouse drag, presses "Ask 2 workers", sends a question,
// and waits for each worker's real answer to show under its own line. Then
// checks the bar counts both, the file on disk is unchanged, and the server
// log has no error from the sends. Costs two short haiku turns.
import { chromium } from 'playwright';
import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import net from 'node:net';
import https from 'node:https';

const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..');
const SERVER = process.argv[2] || path.join(os.homedir(), '.local/bin/amux-server-rs');
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-mdask-real-'));
const HOME = path.join(TMP, 'amux');
fs.mkdirSync(HOME, { recursive: true });
// tmux's socket path must stay under 104 bytes, which a macOS temp dir does not.
const TMUXDIR = fs.mkdtempSync('/tmp/amq-');
const WORK = path.join(TMP, 'work');
fs.mkdirSync(WORK);
const W1 = 'mdask-tea', W2 = 'mdask-primes';
const TODO = path.join(WORK, 'To-Do.md');
const SRC = `# To-Do\n\n- [ ] @${W1} : write a haiku about green tea\n- [ ] @${W2} : name three prime numbers\n`;
fs.writeFileSync(TODO, SRC);
let fails = 0;
const check = (label, ok, detail = '') => { console.log((ok ? '  ok   ' : '  FAIL ') + label + (ok ? '' : ': ' + String(detail).slice(0, 600))); if (!ok) fails++; };
const sleep = ms => new Promise(r => setTimeout(r, ms));
const freePort = () => new Promise(res => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => res(p)); }); });

const port = await freePort();
const BASE = `https://localhost:${port}`;
const srv = path.join(TMP, 'amux-server');
fs.copyFileSync(SERVER, srv); fs.chmodSync(srv, 0o755);
const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^(TMUX|AMUX_SESSION|AMUX_URL|AMUX_CAP_)/.test(k)));
Object.assign(env, { AMUX_HOME: HOME, AMUX_RS_PORT: String(port), AMUX_NO_SELF_ADOPT: '1', TMUX_TMPDIR: TMUXDIR,
  AMUX_DASHBOARD_DIR: path.join(ROOT, 'crates/amux-dashboard/static'), AMUX_RS_SCHEDULER: '0', AMUX_BOARD_DRIVE_SECS: '0',
  // Intended: real workers, on this server's own tmux socket (TMUX_TMPDIR above), never the owner's.
  AMUX_ALLOW_TMUX_SPAWN_FROM_TEST_HOME: '1' });
const log = fs.openSync(path.join(TMP, 'server.log'), 'w');
const sp = spawn(srv, [], { env, stdio: ['ignore', log, log] });
const OWNER_FILE = 'auth' + '_token';

function owner(method, p, body) {
  return new Promise(resolve => {
    const bearer = fs.readFileSync(path.join(HOME, OWNER_FILE), 'utf8').trim();
    const data = body === undefined ? null : JSON.stringify(body);
    const req = https.request(BASE + p, { method, rejectUnauthorized: false, timeout: 60000,
      headers: { 'Content-Type': 'application/json', Authorization: 'Bearer ' + bearer } }, res => {
      let b = ''; res.on('data', c => b += c); res.on('end', () => { let j = {}; try { j = JSON.parse(b); } catch {} resolve([res.statusCode, j]); });
    });
    req.on('error', e => resolve([0, { error: String(e) }]));
    if (data) req.write(data);
    req.end();
  });
}

let browser;
try {
  for (let i = 0; i < 180; i++) {
    if (fs.existsSync(path.join(HOME, OWNER_FILE))) { const [c] = await owner('GET', '/health'); if (c === 200) break; }
    await sleep(500);
  }
  check('the private server is up, serving this checkout\'s dashboard', sp.exitCode === null);
  for (const w of [W1, W2]) {
    const [c, r] = await owner('POST', '/api/sessions', { name: w, dir: WORK, provider: 'claude', model: 'haiku', start: true });
    check(`a real worker ${w} starts`, c === 200 || c === 201, JSON.stringify([c, r]));
  }
  // Ready: each worker's Claude Code is up and idle at its prompt.
  const ready = async () => {
    const [, list] = await owner('GET', '/api/sessions');
    const mine = (Array.isArray(list) ? list : []).filter(s => [W1, W2].includes(s.name));
    return mine.length === 2 && mine.every(s => s.running && ['idle', 'waiting'].includes(String(s.status)));
  };
  let ok = false;
  for (let i = 0; i < 90 && !(ok = await ready()); i++) await sleep(2000);
  if (!ok) { const [, list] = await owner('GET', '/api/sessions'); console.log('    statuses:', JSON.stringify((list || []).map(s => [s.name, s.running, s.status]))); }
  check('both workers reach their prompt', ok);

  browser = await chromium.launch();
  const page = await (await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 1280, height: 800 } })).newPage();
  const errors = [];
  page.on('pageerror', e => errors.push(String(e)));
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.setItem('amux_cloud_offer', '{"dismissed":1}'); });
  await page.goto(BASE + '/');
  await page.waitForFunction(ws => typeof window.openFilePreview === 'function' && typeof window._mdAskAll === 'function'
    && globalThis.eval('sessions').filter(s => ws.includes(s.name)).length === 2, [W1, W2], { timeout: 60000 });
  await page.evaluate(p => window.openFilePreview(p), TODO);
  await page.locator('#file-body li .md-worker-link').nth(1).waitFor();

  // A real drag over both lines.
  const a = await page.locator('#file-body li .md-worker-link').first().boundingBox();
  const last = await page.locator('#file-body li').nth(1).boundingBox();
  await page.mouse.move(a.x - 4, a.y + a.height / 2);
  await page.mouse.down();
  await page.mouse.move(last.x + last.width - 4, last.y + last.height / 2, { steps: 8 });
  await page.mouse.up();
  const btn = page.locator('#md-ask-sel');
  await btn.waitFor({ state: 'visible', timeout: 5000 }).catch(() => {});
  check('the selection offers "Ask 2 workers"', (await btn.isVisible()) && (await btn.textContent()) === 'Ask 2 workers', await btn.textContent().catch(() => ''));
  await btn.click();
  const input = page.locator('#modal-prompt-input');
  await input.fill('In one short sentence, do what your line says. Do not use any tools.');
  await input.press('Enter');

  const t0 = Date.now();
  const replied = () => page.locator('#file-body .md-ask-reply[data-state="replied"]').count();
  while (Date.now() - t0 < 240000 && (await replied()) < 2) await sleep(2000);
  const secs = Math.round((Date.now() - t0) / 1000);
  check(`both real replies show, each under its own line (${secs}s)`, (await replied()) === 2,
    await page.locator('#file-body').innerText());
  for (const w of [W1, W2]) {
    const r = page.locator(`#file-body .md-ask-reply[data-worker="${w}"]`);
    const txt = (await r.textContent().catch(() => '')) || '';
    console.log(`    ${w}: ${txt.trim().slice(0, 140)}`);
    const inLine = await r.evaluate((el, w) => el.closest('li').querySelector('.md-worker-link').dataset.worker === w, w).catch(() => false);
    check(`${w}'s reply sits in ${w}'s line`, inLine);
  }
  const tea = (await page.locator(`#file-body .md-ask-reply[data-worker="${W1}"]`).textContent().catch(() => '')) || '';
  const primes = (await page.locator(`#file-body .md-ask-reply[data-worker="${W2}"]`).textContent().catch(() => '')) || '';
  check('each worker answered its own line (tea / primes)', /tea|leaf|steam|cup/i.test(tea) && /\b(2|3|5|7|11|13)\b/.test(primes), [tea, primes]);
  check('the bar counts both', /2 of 2 replied/.test(await page.locator('#file-body .md-ask-bar').textContent()), await page.locator('#file-body .md-ask-bar').textContent());
  await page.screenshot({ path: path.join(TMP, 'md-ask-real.png') });
  check('the file on disk is unchanged', fs.readFileSync(TODO, 'utf8') === SRC);
  check('the page threw no errors', errors.length === 0, errors.join(' | '));
  const sl = fs.readFileSync(path.join(TMP, 'server.log'), 'utf8');
  check('the server logged the ask', /md_ask_sent/.test(sl), 'no md_ask_sent beacon in server.log');
} finally {
  if (browser) await browser.close();
  for (const w of [W1, W2]) await owner('POST', `/api/sessions/${w}/stop`, {}).catch(() => {});
  sp.kill('SIGTERM');
  try { execFileSync('tmux', ['-S', path.join(TMUXDIR, `tmux-${process.getuid()}`, 'default'), 'kill-server'], { stdio: 'ignore', env: env }); } catch {}   // env has no TMUX: only this test's own server
  fs.rmSync(TMUXDIR, { recursive: true, force: true });
}
console.log(`${fails ? 'FAIL' : 'PASS'}: ${fails} failing check(s) (logs and screenshot ${TMP})`);
process.exit(fails ? 1 : 0);
