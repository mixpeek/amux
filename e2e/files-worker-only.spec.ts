// Ethan, 2026-11-27: "have a toggle on the Files List tab for Worker Details
// page, and that should just show exclusively the files that were created and
// modified by the particular worker". The worker's history is a real Claude
// transcript on disk where its server reads it, so the server's own scan
// decides what the toggle shows.
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

type Page = import('@playwright/test').Page;

async function boot(page: Page) {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.removeItem('amux_files_worker_only');
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function', { timeout: 20000 });
}

const projectName = (dir: string) => fs.realpathSync(dir).replace(/[^A-Za-z0-9]/g, '-');
const tool = (ts: string, name: string, file: string) => JSON.stringify({ type: 'assistant', timestamp: ts,
  message: { role: 'assistant', content: [{ type: 'tool_use', id: 'tu_' + Math.random().toString(36).slice(2), name, input: { file_path: file } }] } }) + '\n';
const created = (ts: string, file: string) => JSON.stringify({ type: 'user', timestamp: ts,
  toolUseResult: { type: 'create', filePath: file, content: 'x' } }) + '\n';

// Writes `lines` as the only transcript in `dir`'s Claude project folder.
function transcript(home: string, dir: string, lines: string): string {
  const proj = path.join(home, 'claude-config', 'projects', projectName(dir));
  fs.mkdirSync(proj, { recursive: true });
  const file = path.join(proj, '0b6f2a8e-' + Math.random().toString(16).slice(2, 10) + '-4f00-9a00-000000000000.jsonl');
  fs.writeFileSync(file, lines);
  return file;
}

// Each row's path relative to the folder shown, in display order.
function useTestClaudeConfig(home: string, worker: string) {
  const meta = path.join(home, 'sessions', worker + '.meta.json');
  const cur = fs.existsSync(meta) ? JSON.parse(fs.readFileSync(meta, 'utf8') || '{}') : {};
  fs.mkdirSync(path.dirname(meta), { recursive: true });
  fs.writeFileSync(meta, JSON.stringify({ ...cur, cc_config_dir: path.join(home, 'claude-config') }));
}

const rowNames = (page: Page) => page.evaluate(() => {
  const base = String((0, eval)('_filesPath') || '/').replace(/\/+$/, '') + '/';
  return Array.from(document.querySelectorAll('#files-body .fe-row[data-path]'))
    .map(r => (r as HTMLElement).dataset.path!).map(p => p.startsWith(base) ? p.slice(base.length) : p);
});

test('the worker Files tab toggles to only the files that worker created or edited', async ({ page }, info) => {
  const home = info.project.metadata.home as string;
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'amux-wf-')));
  const peerDir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'amux-wf-peer-')));
  fs.mkdirSync(path.join(dir, 'a'));
  fs.mkdirSync(path.join(dir, 'b'));
  for (const f of ['a/new.md', 'b/edited.txt', 'untouched.txt', 'read-only.md']) fs.writeFileSync(path.join(dir, f), f);
  // Modified times that disagree with the worker's write order, so the two sorts differ.
  fs.utimesSync(path.join(dir, 'a/new.md'), new Date('2026-11-01'), new Date('2026-11-01'));
  fs.utimesSync(path.join(dir, 'b/edited.txt'), new Date('2026-11-20'), new Date('2026-11-20'));
  const tx = transcript(home, dir,
    tool('2026-11-27T10:00:00Z', 'Write', path.join(dir, 'a/new.md'))
    + created('2026-11-27T10:00:01Z', path.join(dir, 'a/new.md'))
    + tool('2026-11-27T09:00:00Z', 'Edit', path.join(dir, 'b/edited.txt'))
    + tool('2026-11-27T09:30:00Z', 'Read', path.join(dir, 'read-only.md')));
  // A different worker edits untouched.txt; that must not count for this one.
  const peerTx = transcript(home, peerDir, tool('2026-11-27T11:00:00Z', 'Edit', path.join(dir, 'untouched.txt')));

  await boot(page);
  const name = `e2e-wf-${info.project.name}-${Date.now()}`;
  const peer = name + '-peer';
  const api = (p: string, method: string, body?: any) => page.evaluate(async ([u, m, b]) => {
    const r = await fetch(u, { method: m, headers: { 'Content-Type': 'application/json' }, body: b ? JSON.stringify(b) : undefined });
    return r.status; }, [p, method, body] as const);
  expect(await api('/api/sessions', 'POST', { name, dir })).toBe(201);
  expect(await api('/api/sessions', 'POST', { name: peer, dir: peerDir })).toBe(201);
  // Each worker's Claude runs under this test's config folder, the way a
  // worker records the folder its live process uses (claude_config::META_KEY).
  for (const w of [name, peer]) useTestClaudeConfig(home, w);
  try {
    await expect.poll(() => page.evaluate(async w => { await (window as any).fetchSessions(); return ((0, eval)('sessions') || []).some((s: any) => s.name === w); }, name), { timeout: 30000 }).toBe(true);
    await page.evaluate(w => (window as any).openPeek(w), name);
    await page.waitForSelector('#peek-overlay', { state: 'visible', timeout: 15000 });
    await page.locator('#peek-tab-files').click();
    await expect(page.locator('#files-view')).toBeVisible();

    // Off: the folder as it is.
    const toggle = page.locator('#files-worker-only-btn');
    await expect(toggle).toBeVisible();
    await expect(toggle).toHaveAttribute('aria-pressed', 'false');
    await expect.poll(() => rowNames(page)).toEqual(expect.arrayContaining(['a', 'b', 'untouched.txt', 'read-only.md']));

    // On: only what this worker wrote, with paths below this folder.
    await toggle.click();
    await expect(toggle).toHaveAttribute('aria-pressed', 'true');
    await expect.poll(() => rowNames(page).then(n => n.sort())).toEqual(['a/new.md', 'b/edited.txt']);
    await expect(page.locator('#files-body .fe-row[data-path="' + path.join(dir, 'a/new.md') + '"] .fe-worker-tag')).toHaveText('created');
    await expect(page.locator('#files-body .fe-row[data-path="' + path.join(dir, 'b/edited.txt') + '"] .fe-worker-tag')).toHaveText('edited');
    await expect(page.locator('#fe-hdr-opened')).toContainText('Worker wrote');
    await expect(page.locator('#fe-status')).toContainText('2 files written by ' + name);
    await page.screenshot({ path: info.outputPath('worker-only.png') });

    // The table's sorts still work: Modified (newest first) and the worker's last write.
    // Modified is the default, newest first: edited.txt changed on Nov 20, new.md on Nov 1.
    expect(await rowNames(page), 'newest modified first').toEqual(['b/edited.txt', 'a/new.md']);
    // The worker wrote new.md last (10:00 against 09:00).
    await page.locator('#fe-hdr-opened').click();
    await expect.poll(() => rowNames(page), 'newest worker write first').toEqual(['a/new.md', 'b/edited.txt']);
    await page.locator('#fe-hdr-opened').click();
    await expect.poll(() => rowNames(page), 'and reversed').toEqual(['b/edited.txt', 'a/new.md']);
    await page.locator('#fe-hdr-name').click();
    await expect.poll(() => rowNames(page), 'by name').toEqual(['a/new.md', 'b/edited.txt']);

    // A re-render keeps the filter.
    await page.evaluate(() => (window as any).loadFiles((0, eval)('_filesPath')));
    await expect.poll(() => rowNames(page).then(n => n.sort())).toEqual(['a/new.md', 'b/edited.txt']);

    // A subfolder scopes it, and says what is elsewhere.
    await page.evaluate(p => (window as any).loadFiles(p), path.join(dir, 'a'));
    await expect.poll(() => rowNames(page)).toEqual(['new.md']);
    await expect(page.locator('#fe-status')).toContainText('1 more outside this folder');
    await page.evaluate(p => (window as any).loadFiles(p), dir);

    // A later edit in the same transcript appears after the list is refreshed.
    fs.appendFileSync(tx, tool('2026-11-27T12:00:00Z', 'Edit', path.join(dir, 'untouched.txt')));
    await toggle.click();
    await toggle.click();
    await expect.poll(() => rowNames(page).then(n => n.sort())).toEqual(['a/new.md', 'b/edited.txt', 'untouched.txt']);

    // Opening a row opens that file.
    const row = page.locator('#files-body .fe-row[data-path="' + path.join(dir, 'b/edited.txt') + '"]');
    // A touch screen opens on a tap; a pointer selects on click and opens on a double-click.
    if (await page.evaluate(() => (window as any)._feTouchDevice())) await row.tap(); else await row.dblclick();
    await expect(page.locator('#file-overlay')).toHaveClass(/active/);
    await expect(page.locator('#file-overlay')).toHaveAttribute('data-path', path.join(dir, 'b/edited.txt'));
    await page.keyboard.press('Escape');

    // Off again: the plain folder and the Opened column.
    await page.evaluate(() => (window as any).closeFilePreview && (window as any).closeFilePreview());
    await toggle.click();
    await expect(toggle).toHaveAttribute('aria-pressed', 'false');
    await expect.poll(() => rowNames(page)).toEqual(expect.arrayContaining(['a', 'b', 'untouched.txt', 'read-only.md']));
    await expect(page.locator('#fe-hdr-opened')).toContainText('Opened');

    // The peer's list holds only its own write, and not this worker's.
    const peerFiles = await page.evaluate(async w => (await (await fetch('/api/sessions/' + encodeURIComponent(w) + '/files-written')).json()), peer);
    expect(peerFiles.measured).toBe(true);
    expect(peerFiles.files.map((f: any) => f.path)).toEqual([path.join(dir, 'untouched.txt')]);
  } finally {
    await cleanup('delete the test workers and their files', async () => {
      await api('/api/sessions/' + encodeURIComponent(name), 'DELETE');
      await api('/api/sessions/' + encodeURIComponent(peer), 'DELETE');
      for (const p of [dir, peerDir, path.dirname(tx), path.dirname(peerTx)]) fs.rmSync(p, { recursive: true, force: true });
    }, info);
  }
});

test('on a phone the toggle sits in the one toolbar row', async ({ page }, info) => {
  test.skip(info.project.name === 'desktop', 'phone layout');
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'amux-wf-m-')));
  await boot(page);
  const name = `e2e-wf-m-${info.project.name}-${Date.now()}`;
  const api = (p: string, method: string, body?: any) => page.evaluate(async ([u, m, b]) => {
    const r = await fetch(u, { method: m, headers: { 'Content-Type': 'application/json' }, body: b ? JSON.stringify(b) : undefined });
    return r.status; }, [p, method, body] as const);
  expect(await api('/api/sessions', 'POST', { name, dir })).toBe(201);
  try {
    await page.evaluate(([d, w]) => (window as any).openExplore(d, w), [dir, name]);
    const toggle = page.locator('#files-worker-only-btn');
    await expect(toggle).toBeVisible();
    await toggle.click();
    await expect(page.locator('#files-body')).toContainText('has not created or edited any files yet');
    const bar = await page.locator('#files-view .fe-toolbar').boundingBox();
    const btn = await toggle.boundingBox();
    const vw = page.viewportSize()!.width;
    // One row: the toggle, Back and the More menu share a line, and the bar is
    // shorter than two 44px controls stacked.
    expect(bar!.height, 'one row').toBeLessThan(88);
    for (const sel of ['#files-back-session-btn', '#files-overflow-btn']) {
      const b = await page.locator(sel).boundingBox();
      expect(Math.abs((b!.y + b!.height / 2) - (bar!.y + bar!.height / 2)), sel + ' on the same row').toBeLessThan(8);
    }
    expect(btn!.x + btn!.width).toBeLessThanOrEqual(vw);
    expect(btn!.width, 'touch target').toBeGreaterThanOrEqual(44);
    expect(btn!.height, 'touch target').toBeGreaterThanOrEqual(44);
    await expect(page.locator('#files-breadcrumb')).toBeVisible();
    await expect(toggle, 'the toggle says what it does on a phone').toContainText('This worker');
    expect((await page.locator('#files-breadcrumb').boundingBox())!.width, 'the path keeps some room').toBeGreaterThan(40);
    expect(btn!.y).toBeGreaterThanOrEqual(bar!.y);
    expect(btn!.y + btn!.height).toBeLessThanOrEqual(bar!.y + bar!.height + 1);
    await page.screenshot({ path: info.outputPath('phone.png') });
  } finally {
    await cleanup('delete the test worker', async () => {
      await api('/api/sessions/' + encodeURIComponent(name), 'DELETE');
      fs.rmSync(dir, { recursive: true, force: true });
    }, info);
  }
});
