#!/usr/bin/env node
// END-TO-END: a file opened once while online opens again OFFLINE, from the
// device's own store. Covers a text file, a small image (inlined by the
// server) and a large image (streamed, > 5 MB). Offline is the server killed,
// not a browser flag: the page has no network path to any bytes.
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/file-offline.mjs
import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import crypto from 'node:crypto';
import { chromium } from 'playwright';
import { startAmux, waitFor } from './harness.mjs';

function png(w, h, noise) {
  const raw = Buffer.alloc((w * 3 + 1) * h);
  for (let y = 0; y < h; y++) { raw[y * (w * 3 + 1)] = 0; }
  if (noise) crypto.randomFillSync(raw); for (let y = 0; y < h; y++) raw[y * (w * 3 + 1)] = 0;
  const chunk = (t, d) => { const b = Buffer.alloc(8 + d.length + 4); b.writeUInt32BE(d.length, 0); b.write(t, 4); d.copy(b, 8);
    b.writeUInt32BE(zlib.crc32 ? zlib.crc32(Buffer.concat([Buffer.from(t), d])) : 0, 8 + d.length); return b; };
  const ihdr = Buffer.alloc(13); ihdr.writeUInt32BE(w, 0); ihdr.writeUInt32BE(h, 4); ihdr[8] = 8; ihdr[9] = 2;
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ihdr), chunk('IDAT', zlib.deflateSync(raw, { level: 0 })), chunk('IEND', Buffer.alloc(0))]);
}

const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY });
let browser;
try {
  const dir = path.join(amux.env.AMUX_FILES_ROOT || amux.root, 'files'); fs.mkdirSync(dir, { recursive: true });
  const files = {
    text: path.join(dir, 'notes.md'),
    small: path.join(dir, 'small.png'),
    large: path.join(dir, 'large.png'),
  };
  fs.writeFileSync(files.text, '# Offline notes\n\nthe quick offline fox\n');
  fs.writeFileSync(files.small, png(40, 30, false));
  fs.writeFileSync(files.large, png(1500, 1400, true));
  check('large image is over the 5 MB inline limit', fs.statSync(files.large).size > 5.5e6, fs.statSync(files.large).size);

  browser = await chromium.launch();
  const page = await (await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 } })).newPage();
  const pageErrors = []; page.on('pageerror', e => pageErrors.push(String(e)));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto(amux.base + '/', { waitUntil: 'domcontentloaded' });
  await page.waitForFunction(() => typeof openFilePreview === 'function');

  const view = async (p) => {
    await page.evaluate(p => openFilePreview(p), p);
    await page.waitForTimeout(800);
    return page.evaluate(() => {
      const b = document.getElementById('file-body'); const img = b && b.querySelector('img');
      return { text: (b && b.innerText) || '', title: document.getElementById('file-title')?.textContent || '',
        img: img ? { w: img.naturalWidth, complete: img.complete } : null };
    });
  };
  const waitImg = async (p) => { await page.evaluate(p => openFilePreview(p), p);
    return waitFor('image decoded', () => page.evaluate(() => { const i = document.querySelector('#file-body img'); return i && i.complete && i.naturalWidth > 0 ? i.naturalWidth : null; }), 20000, 300).catch(() => 0); };

  // ---- online: open each once ----
  check('online: text renders', (await view(files.text)).text.includes('quick offline fox'));
  check('online: small image renders', await waitImg(files.small) === 40);
  check('online: large image renders', await waitImg(files.large) === 1500);
  await page.waitForTimeout(3000);   // let background caching finish

  // ---- offline ----
  await amux.down();
  await page.evaluate(() => { try { closeFilePreview && closeFilePreview(); } catch (e) {} });
  const t = await view(files.text);
  check('offline: text opens from the device', t.text.includes('quick offline fox'), t);
  check('offline: small image opens from the device', await waitImg(files.small) === 40);
  const large = await waitImg(files.large);
  check('offline: large (streamed) image opens from the device', large === 1500, { naturalWidth: large });
  await page.screenshot({ path: path.join(amux.root, 'offline-large.png') });
  check('no uncaught page errors', pageErrors.length === 0, pageErrors.slice(0, 5));
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally {
  if (browser) await browser.close().catch(() => {});
  await amux.stop();
}
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
