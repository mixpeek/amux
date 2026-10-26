// One transcription setting for Dictate and Recordings (Ethan, 2026-10-25: "this
// should be the same as dictate whatever is used there. we should have a
// centralized transcription setting/config"). Settings > Connect > Transcription
// picks the engine; a recording that failed is transcribed again through the same
// engine Dictate uses. Gemini is a local stub (playwright.config.ts points every
// e2e server's AMUX_GEMINI_BASE at its own stub port), so nothing is billed.
import { test, expect, Page } from './fixtures';
import * as fs from 'fs';
import * as http from 'http';
import * as os from 'os';
import * as path from 'path';
import { geminiStubPort } from './test-env';
import { cleanup } from './teardown';

declare function switchView(view: string): void;

const STUB_TEXT = 'Pick up the van keys, then call the landlord.';

async function token(page: Page): Promise<string> {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  return page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
}

async function api(page: Page, tok: string, method: 'GET' | 'POST', url: string, data?: unknown, body?: Buffer) {
  const headers: Record<string, string> = { Authorization: `Bearer ${tok}` };
  const r = method === 'GET'
    ? await page.request.get(url, { headers })
    : await page.request.post(url, body ? { headers: { ...headers, 'Content-Type': 'audio/wav' }, data: body } : { headers, data });
  expect(r.status(), `${method} ${url}: ${await r.text()}`).toBeLessThan(300);
  return r.json();
}

// Gemini's generateContent, answering STUB_TEXT and remembering what it was sent.
function startStub(port: number): Promise<{ seen: string[]; close: () => Promise<void> }> {
  const seen: string[] = [];
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (c) => { body += c; });
    req.on('end', () => {
      seen.push(`${req.url} ${body.slice(0, 2000)}`);
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ candidates: [{ content: { parts: [{ text: STUB_TEXT }] } }] }));
    });
  });
  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', () => resolve({ seen, close: () => new Promise((r) => server.close(() => r())) }));
  });
}

// One second of 16 kHz mono silence.
function wav(): Buffer {
  const n = 32000;
  const b = Buffer.alloc(44 + n);
  b.write('RIFF', 0); b.writeUInt32LE(36 + n, 4); b.write('WAVEfmt ', 8);
  b.writeUInt32LE(16, 16); b.writeUInt16LE(1, 20); b.writeUInt16LE(1, 22);
  b.writeUInt32LE(16000, 24); b.writeUInt32LE(32000, 28); b.writeUInt16LE(2, 32); b.writeUInt16LE(16, 34);
  b.write('data', 36); b.writeUInt32LE(n, 40);
  return b;
}

let tok = '';
test.afterEach(async ({ page }) => {
  // Leave the shared server as every other spec expects it.
  if (tok) {
    await api(page, tok, 'POST', '/api/transcription', { engine: 'auto' });
    await api(page, tok, 'POST', '/api/dictation/config', { key: '' });
  }
});

test('Settings > Connect > Transcription is the one engine setting', async ({ page }) => {
  tok = await token(page);
  await page.evaluate(() => { (window as any).toggleSettings(); (window as any)._settingsTab('integrations'); });
  const sel = page.locator('#transcription-engine');
  await expect(sel).toBeVisible();
  await expect(sel).toHaveValue('auto');
  await expect(page.locator('#transcription-status')).toContainText('Dictate uses');
  await expect(page.locator('#transcription-status')).toContainText('Recordings use');
  await sel.selectOption('gemini');
  await expect(page.getByText('Transcription engine saved')).toBeVisible();
  // No key on this test server: the status says why, in the setting's own words.
  await expect(page.locator('#transcription-status')).toContainText('Gemini only');
  expect((await api(page, tok, 'GET', '/api/transcription')).engine).toBe('gemini');
  // The Recordings tab reads the same setting and links to it.
  const cfg = await api(page, tok, 'GET', '/api/recordings/config');
  expect(cfg.transcriber.setting.engine).toBe('gemini');
  expect(cfg.transcriber.why_unavailable).toContain('Settings > Connect > Transcription');
  // It survives a reload.
  await page.reload();
  await page.evaluate(() => { (window as any).toggleSettings(); (window as any)._settingsTab('integrations'); });
  await expect(page.locator('#transcription-engine')).toHaveValue('gemini');
});

test('a recording that failed is transcribed again through the engine Dictate uses', async ({ page }, info) => {
  tok = await token(page);
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-e2e-recordings-'));
  await api(page, tok, 'POST', '/api/recordings/config', { dir });
  await api(page, tok, 'POST', '/api/transcription', { engine: 'gemini' });
  await api(page, tok, 'POST', '/api/dictation/config', { key: 'e2e-not-a-real-gemini-key' });
  const port = geminiStubPort(Number(new URL(String(info.project.use.baseURL)).port));

  // Nothing answers on the stub port yet, so the first pass fails. (The e2e servers
  // run without periodic jobs; a transcribe request runs the pass itself.)
  const id = 'rtr' + Date.now().toString(36);
  const started = Date.now() - 5000;
  await api(page, tok, 'POST', `/api/recordings/upload?id=${id}&started_at=${started}&dur_ms=1000&tz_offset_min=0&mime=audio/wav`, undefined, wav());
  await api(page, tok, 'POST', `/api/recordings/${id}/transcribe`, {});
  await expect.poll(async () => (await api(page, tok, 'GET', `/api/recordings/${id}`)).transcript?.status, { timeout: 30_000 }).toBe('failed');

  const stub = await startStub(port);
  try {
    await page.evaluate(() => switchView('record'));
    const row = page.locator(`.recorder-item[data-rec-id="${id}"]`);
    await expect(row).toContainText('Transcription failed');
    await row.getByRole('button', { name: 'Transcribe again' }).click();
    await expect(page.getByText('Transcription queued')).toBeVisible();
    await expect.poll(async () => (await api(page, tok, 'GET', `/api/recordings/${id}`)).transcript?.status, { timeout: 60_000 }).toBe('done');
    const rec = await api(page, tok, 'GET', `/api/recordings/${id}`);
    expect(rec.transcript.engine).toBe('gemini');
    expect(stub.seen.some((s) => s.includes('key=e2e-not-a-real-gemini-key') && s.includes('inline_data'))).toBe(true);
    const md = fs.readFileSync(path.join(dir, rec.transcript.md), 'utf8');
    expect(md).toContain('engine: gemini');
    expect(md).toContain(STUB_TEXT);
    await page.reload();
    await page.evaluate(() => switchView('record'));
    await expect(page.locator(`.recorder-item[data-rec-id="${id}"]`)).toContainText(STUB_TEXT);
  } finally {
    await cleanup('close the Gemini stub', () => stub.close());
  }
});
