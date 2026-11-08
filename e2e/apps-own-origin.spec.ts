// An app inside amux runs at `/` on a port of its own (app_ports.rs). Under
// /app/<id>/ a Vite dev server painted its server-rendered HTML and nothing
// else: its module imports, fonts and fetches asked for root paths the prefix
// could not catch, and its router saw /app/<id>/ as the page (owner,
// 2026-11-08: "a lot of issues with the app thing"). This app does each of
// those the way a dev server does, and its live-reload WebSocket must connect.
import { test, expect } from './fixtures';
import * as crypto from 'crypto';
import * as http from 'http';
import type { AddressInfo } from 'net';

const PAGE = `<!doctype html><html><head><link rel="stylesheet" href="/style.css"></head><body>
<h1>own origin</h1><p id="path"></p><p id="mod"></p><p id="api"></p><p id="ws"></p>
<script type="module">
  import { answer } from "/src/answer.js";
  document.getElementById('path').textContent = location.pathname;
  document.getElementById('mod').textContent = answer;
  fetch('/api/hello').then(r => r.text()).then(t => document.getElementById('api').textContent = t);
  const s = new WebSocket('wss://' + location.host + '/hmr', 'vite-hmr');
  s.onmessage = (e) => document.getElementById('ws').textContent = e.data + ' ' + s.protocol;
  s.onerror = () => document.getElementById('ws').textContent = 'ws failed';
</script></body></html>`;

function frame(text: string): Buffer {
  const b = Buffer.from(text);
  return Buffer.concat([Buffer.from([0x81, b.length]), b]);
}

test('an app opens at / on its own port: module imports, fonts, fetches and its WebSocket all reach it', async ({ page }, testInfo) => {
  const hits = new Set<string>();
  const app = http.createServer((req, res) => {
    const url = req.url || '/';
    hits.add(url.split('?')[0]);
    if (url === '/style.css') { res.writeHead(200, { 'Content-Type': 'text/css' }); res.end("@font-face{font-family:probe;src:url(/fonts/probe.woff2)}h1{font-family:probe}"); return; }
    if (url === '/fonts/probe.woff2') { res.writeHead(200, { 'Content-Type': 'font/woff2' }); res.end(Buffer.alloc(0)); return; }
    if (url === '/src/answer.js') { res.writeHead(200, { 'Content-Type': 'text/javascript' }); res.end('import { n } from "/src/n.js"; export const answer = "module " + n;'); return; }
    if (url === '/src/n.js') { res.writeHead(200, { 'Content-Type': 'text/javascript' }); res.end('export const n = 42;'); return; }
    if (url === '/api/hello') { res.writeHead(200, { 'Content-Type': 'text/plain' }); res.end('api ok'); return; }
    if (url === '/' || url.startsWith('/?')) { res.writeHead(200, { 'Content-Type': 'text/html' }); res.end(PAGE); return; }
    res.writeHead(404); res.end('not found');
  });
  app.on('upgrade', (req, socket) => {
    const accept = crypto.createHash('sha1').update(req.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
    const proto = String(req.headers['sec-websocket-protocol'] || '').split(',').map(s => s.trim()).includes('vite-hmr') ? 'Sec-WebSocket-Protocol: vite-hmr\r\n' : '';
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n${proto}\r\n`);
    socket.write(frame('hmr connected'));
    socket.on('error', () => {});
  });
  await new Promise<void>(r => app.listen(0, '127.0.0.1', () => r()));
  const port = (app.address() as AddressInfo).port;
  const tag = `own-${testInfo.project.name}-${Date.now()}`;
  try {
    // A frame cannot send the bearer token: sign this browser in with the owner cookie.
    await page.goto('/');
    await page.waitForFunction(() => !!(window as any)._AMUX_AUTH_TOKEN, { timeout: 20000 });
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
    await page.goto('/?_token=' + encodeURIComponent(token));
    await page.waitForURL(/_fresh=1/, { timeout: 20000 });
    await page.goto('/#view=apps');
    await page.getByRole('button', { name: 'Add app' }).click();
    await page.locator('#pub-new-target').fill(String(port));
    await page.locator('#pub-new-title').fill(tag);
    await page.locator('#modal-btns').getByRole('button', { name: 'Add' }).click();
    await page.locator('#pub-internal .pub-card', { hasText: tag }).click();

    const f = page.frameLocator('.pub-app-frame');
    await expect(f.locator('h1')).toHaveText('own origin');
    await expect(f.locator('#path'), 'the app sees its own page path').toHaveText('/');
    await expect(f.locator('#mod'), 'absolute module imports reach the app').toHaveText('module 42');
    await expect(f.locator('#api'), 'a runtime fetch to a root path reaches the app').toHaveText('api ok');
    await expect(f.locator('#ws'), 'the live-reload WebSocket connects with its subprotocol').toHaveText('hmr connected vite-hmr');
    // A stylesheet's url(/fonts/...) reaches the app too.
    await expect.poll(() => hits.has('/fonts/probe.woff2')).toBe(true);
    const src = new URL((await page.locator('.pub-app-frame').getAttribute('src'))!);
    expect(src.pathname).toBe('/');
    expect(src.origin).not.toBe(new URL(page.url()).origin);
    // The viewer still follows the app: its page shows in the status line.
    await expect(page.locator('#pages-status')).toContainText(`port ${port}`);
  } finally {
    app.close();
  }
});
