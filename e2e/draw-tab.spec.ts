// Draw tab (Ethan, 2026-10-12): freehand diagrams on Excalidraw, autosaved as
// standard .excalidraw files in a chosen folder, a shared component library on
// disk, snap toggles, export, live reload from disk and Send to worker.
//
// Every assertion that matters reads the FILE ON DISK or the request that left
// the page, not the UI's own claim, because the failure this guards against is
// a status line that says "saved" over a write that never happened.
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { APIRequestContext, Page, TestInfo } from '@playwright/test';
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

type Win = Window & { _draw: any; switchView: (v: string) => void; _AMUX_DATA_DIR: string; _AMUX_AUTH_TOKEN: string };

// Switch Excalidraw's tool and wait until it is active. setActiveTool applies on
// the editor's next state update, so a drag sent straight after it could still
// draw with the previous tool (main CI, 2026-10-13: a rectangle drag drew a
// second freedraw stroke).
async function setTool(page: Page, type: string) {
  await page.evaluate((t) => (window as unknown as Win)._draw.api.setActiveTool({ type: t }), type);
  await page.waitForFunction((t) => (window as unknown as Win)._draw.api.getAppState().activeTool.type === t, type);
}

async function openDraw(page: Page) {
  await page.waitForFunction(() => typeof (window as unknown as Win).switchView === 'function');
  await expect(page.locator('#tab-draw')).toHaveCount(1);
  await page.evaluate(() => (window as unknown as Win).switchView('draw'));
  await expect(page.locator('#draw-view')).toBeVisible();
  // The editor bundle loads lazily on first open, then the API arrives.
  await page.waitForFunction(() => { const d = (window as unknown as Win)._draw; return !!(d && d.api && d.path && !d.opening); }, null, { timeout: 30_000 });
  await expect(page.locator('#draw-host .excalidraw')).toBeVisible();
}

async function canvasBox(page: Page) {
  const box = await page.locator('#draw-host canvas.interactive').boundingBox();
  if (!box) throw new Error('no interactive canvas');
  return box;
}

async function drag(page: Page, from: [number, number], to: [number, number], steps = 8) {
  await page.mouse.move(from[0], from[1]);
  await page.mouse.down();
  for (let i = 1; i <= steps; i++) {
    await page.mouse.move(from[0] + ((to[0] - from[0]) * i) / steps, from[1] + ((to[1] - from[1]) * i) / steps + (i % 2 ? 6 : -6));
  }
  await page.mouse.up();
}

const sceneTypes = (page: Page) => page.evaluate(() =>
  (window as unknown as Win)._draw.api.getSceneElements().map((e: any) => e.type).sort());

async function waitSaved(page: Page) {
  await expect(page.locator('#draw-save-state')).toHaveAttribute('data-state', 'saved', { timeout: 15_000 });
}

test('Draw tab: draw, autosave to disk, reopen, library, snap, export, live reload, send to worker', async ({ page, request }, info) => {
  test.setTimeout(150_000);
  page.on('console', m => { if (/\[draw\]|error/i.test(m.text())) console.log('[page]', m.text().slice(0, 300)); });
  const dir = await mkdtemp(join(tmpdir(), 'amux-draw-'));
  await page.goto('/');
  const headers = await page.evaluate(() => ({ Authorization: 'Bearer ' + (window as unknown as Win)._AMUX_AUTH_TOKEN }));
  const pref = (key: string, value: string) => request.post('/api/prefs', { headers, data: { key, value } });
  const prior = await (await request.get('/api/prefs?key=draw_dir', { headers })).json();
  try {
    expect((await pref('draw_dir', dir)).ok()).toBeTruthy();
    expect((await pref('draw_last', '')).ok()).toBeTruthy();
    expect((await pref('draw_settings', JSON.stringify({ grid: false, align: true, connect: true }))).ok()).toBeTruthy();
    await page.reload();
    await openDraw(page);
    const path = await page.evaluate(() => (window as unknown as Win)._draw.path);
    expect(path, 'a new drawing lands in the chosen folder').toBe(join(dir, 'Untitled.excalidraw'));
    await expect(page.locator('#draw-save-state')).toHaveAttribute('data-state', 'new');

    // A freehand stroke and a rectangle, with real pointer events.
    const box = await canvasBox(page);
    const cx = box.x + box.width / 2, cy = box.y + box.height / 2;
    await setTool(page, 'freedraw');
    await drag(page, [cx - 120, cy - 60], [cx - 20, cy - 10], 12);
    await setTool(page, 'rectangle');
    await drag(page, [cx + 10, cy + 10], [cx + 110, cy + 70]);
    await expect.poll(() => sceneTypes(page)).toEqual(['freedraw', 'rectangle']);

    // Autosave wrote a valid .excalidraw file (read from disk, not the UI).
    await waitSaved(page);
    const onDisk = JSON.parse(await readFile(path, 'utf8'));
    expect(onDisk.type).toBe('excalidraw');
    expect(onDisk.elements.map((e: any) => e.type).sort()).toEqual(['freedraw', 'rectangle']);

    // Reload the page: the tab reopens the same file with both elements.
    await page.reload();
    await openDraw(page);
    expect(await page.evaluate(() => (window as unknown as Win)._draw.path)).toBe(path);
    await expect.poll(() => sceneTypes(page)).toEqual(['freedraw', 'rectangle']);

    // Snap toggles drive Excalidraw's own state and persist per user.
    const snap = () => page.evaluate(() => { const s = (window as unknown as Win)._draw.api.getAppState();
      return { grid: s.gridModeEnabled, align: s.objectsSnapModeEnabled, connect: s.isBindingEnabled }; });
    expect(await snap()).toEqual({ grid: false, align: true, connect: true });
    await page.locator('#draw-snap-grid').click();
    await page.locator('#draw-snap-align').click();
    await page.locator('#draw-snap-connect').click();
    await expect.poll(snap).toEqual({ grid: true, align: false, connect: false });
    await expect(page.locator('#draw-snap-grid')).toHaveAttribute('aria-pressed', 'true');
    await expect.poll(async () => JSON.parse((await (await request.get('/api/prefs?key=draw_settings', { headers })).json()).value))
      .toEqual({ grid: true, align: false, connect: false });
    await page.locator('#draw-snap-grid').click();
    await page.locator('#draw-snap-connect').click();

    // Save the rectangle to the library: the .excalidrawlib on disk has it.
    await page.evaluate(() => { const api = (window as unknown as Win)._draw.api;
      const rect = api.getSceneElements().find((e: any) => e.type === 'rectangle');
      api.updateScene({ appState: { selectedElementIds: { [rect.id]: true } } }); });
    const mobile = (page.viewportSize()?.width || 1280) <= 700;
    if (mobile) await page.locator('#draw-lib-open').click();
    await page.locator('#draw-lib-save').click();
    await page.locator('#modal-prompt-input').fill('Service');
    await page.locator('#modal-prompt-input').press('Enter');
    const libPath = join(await page.evaluate(() => (window as unknown as Win)._AMUX_DATA_DIR), 'draw', 'library.excalidrawlib');
    await expect.poll(async () => { try { return JSON.parse(await readFile(libPath, 'utf8')).libraryItems.map((i: any) => i.name); } catch { return null; } },
      { timeout: 10_000 }).toEqual(['Service']);
    const item = page.locator('.draw-lib-item').filter({ hasText: 'Service' });
    await expect(item).toBeVisible();

    // ...and bring it back onto the canvas: drag on desktop, tap on a phone.
    const before = (await sceneTypes(page)).length;
    if (mobile) await item.click();
    else await item.dragTo(page.locator('#draw-host canvas.interactive'), { targetPosition: { x: box.width / 2 + 160, y: box.height / 2 - 120 } });
    await expect.poll(() => sceneTypes(page)).toHaveLength(before + 1);
    await waitSaved(page);
    expect(JSON.parse(await readFile(path, 'utf8')).elements.filter((e: any) => !e.isDeleted && e.type === 'rectangle')).toHaveLength(2);

    // Export PNG and SVG as real downloads.
    await page.evaluate(() => (window as unknown as Win)._draw.api.updateScene({ appState: { selectedElementIds: {} } }));
    for (const [kind, label, check] of [
      ['png', 'PNG image', (b: Buffer) => b.subarray(1, 4).toString() === 'PNG'],
      ['svg', 'SVG image', (b: Buffer) => b.toString().includes('<svg')],
    ] as const) {
      await page.locator('#draw-export-btn').click();
      const dl = page.waitForEvent('download');
      await page.locator('#draw-export-menu button', { hasText: label }).click();
      const d = await dl;
      expect(d.suggestedFilename()).toBe('Untitled.' + kind);
      const bytes = await readFile(await d.path());
      expect(check(bytes), kind + ' bytes').toBe(true);
    }

    // Live reload: a worker rewrites the file on disk; the canvas follows.
    const external = JSON.parse(JSON.stringify(onDisk));
    external.elements = [Object.assign({}, onDisk.elements.find((e: any) => e.type === 'rectangle'), { id: 'from-disk', type: 'ellipse', version: 1 })];
    await new Promise(r => setTimeout(r, 1100));   // the server's ETag has 1s granularity
    await writeFile(path, JSON.stringify(external));
    await expect.poll(() => sceneTypes(page), { timeout: 10_000 }).toEqual(['ellipse']);
    await expect(page.locator('#draw-save-state')).toHaveAttribute('data-state', 'saved');

    // Send to worker: one PNG through the upload API, then the composer's send
    // path with the @upload reference and the drawing's own path.
    const sent: any[] = [];
    await page.route('**/api/sessions/*/send', async route => {
      sent.push({ url: route.request().url(), body: JSON.parse(route.request().postData() || '{}') });
      await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ ok: true }) });
    });
    await page.locator('.draw-act[aria-label="Send to worker"]').click();
    await page.locator('#draw-send-worker').fill('draw-e2e-worker');
    await page.locator('#draw-send-prompt').fill('build this');
    await page.locator('#modal-btns .btn.primary').click();
    await expect.poll(() => sent.length, { timeout: 15_000 }).toBe(1);
    expect(sent[0].url).toContain('/api/sessions/draw-e2e-worker/send');
    const text: string = sent[0].body.text;
    expect(text).toContain('build this');
    const upload = text.match(/@(\S+\.png)/);
    expect(upload, text).not.toBeNull();
    expect(upload![1]).toContain('/uploads/');
    expect((await readFile(upload![1])).subarray(1, 4).toString()).toBe('PNG');
    expect(text).toContain('@' + path);

    // A refused write is never reported as saved.
    await page.route('**/api/file', route => route.request().method() === 'PUT'
      ? route.fulfill({ status: 500, contentType: 'application/json', body: JSON.stringify({ error: 'disk full' }) })
      : route.continue());
    await setTool(page, 'rectangle');
    await drag(page, [cx - 100, cy + 40], [cx - 40, cy + 90]);
    await expect(page.locator('#draw-save-state')).toHaveAttribute('data-state', 'error', { timeout: 10_000 });
    await expect(page.locator('#draw-save-state')).toHaveText('Not saved');
    expect(JSON.parse(await readFile(path, 'utf8')).elements.map((e: any) => e.type)).toEqual(['ellipse']);
  } finally {
    await cleanup('restore draw_dir', () => pref('draw_dir', prior.value || ''), info);
    await cleanup('clear draw_last', () => pref('draw_last', ''), info);
    await cleanup('remove temp folder', () => rm(dir, { recursive: true, force: true }), info);
  }
});

test('Draw tab: a disk change over unsaved edits asks first; .excalidraw files from Files open in Draw', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  const dir = await mkdtemp(join(tmpdir(), 'amux-draw-c-'));
  const path = join(dir, 'shared.excalidraw');
  const scene = (types: string[]) => JSON.stringify({ type: 'excalidraw', version: 2, source: 'e2e', appState: {}, files: {},
    elements: types.map((t, i) => ({ id: 'e' + i, type: t, x: i * 150, y: 0, width: 100, height: 60, angle: 0, strokeColor: '#1e1e1e',
      backgroundColor: 'transparent', fillStyle: 'solid', strokeWidth: 2, strokeStyle: 'solid', roughness: 1, opacity: 100,
      groupIds: [], frameId: null, roundness: null, seed: i + 1, version: 1, versionNonce: i + 1, isDeleted: false,
      boundElements: null, updated: 1, link: null, locked: false })) });
  await writeFile(path, scene(['rectangle']));
  await page.goto('/');
  const headers = await page.evaluate(() => ({ Authorization: 'Bearer ' + (window as unknown as Win)._AMUX_AUTH_TOKEN }));
  const pref = (key: string, value: string) => request.post('/api/prefs', { headers, data: { key, value } });
  try {
    await pref('draw_dir', dir);
    await pref('draw_last', '');
    await page.reload();
    await page.waitForFunction(() => typeof (window as unknown as Win).switchView === 'function');
    // The Files tab hands a drawing to Draw instead of the text viewer.
    await page.evaluate(() => (window as unknown as Win).switchView('files'));
    await page.evaluate(p => (window as any).openFilePreview(p), path);
    await expect(page.locator('#draw-view')).toBeVisible();
    await page.waitForFunction(p => { const d = (window as unknown as Win)._draw; return !!(d && d.api && d.path === p && !d.opening); }, path, { timeout: 30_000 });
    await expect.poll(() => sceneTypes(page)).toEqual(['rectangle']);
    await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);

    // Unsaved local work (the server refuses the write), then a worker rewrites the file.
    await page.route('**/api/file', route => route.request().method() === 'PUT'
      ? route.fulfill({ status: 500, contentType: 'application/json', body: JSON.stringify({ error: 'disk full' }) })
      : route.continue());
    const box = await canvasBox(page);
    await setTool(page, 'diamond');
    await drag(page, [box.x + box.width / 2 - 40, box.y + box.height / 2 + 60], [box.x + box.width / 2 + 40, box.y + box.height / 2 + 120]);
    await expect(page.locator('#draw-save-state')).toHaveAttribute('data-state', 'error', { timeout: 10_000 });
    await new Promise(r => setTimeout(r, 1100));
    await writeFile(path, scene(['ellipse', 'ellipse']));
    // Not silently reloaded over the local edit: the user is asked.
    await expect(page.locator('#draw-conflict')).toBeVisible({ timeout: 10_000 });
    await expect(page.locator('#draw-save-state')).toHaveAttribute('data-state', 'conflict');
    expect((await sceneTypes(page)).sort()).toEqual(['diamond', 'rectangle']);
    await page.locator('#draw-conflict button', { hasText: 'Load disk version' }).click();
    await expect.poll(() => sceneTypes(page)).toEqual(['ellipse', 'ellipse']);
    await expect(page.locator('#draw-conflict')).toBeHidden();
  } finally {
    await cleanup('clear draw_dir', () => pref('draw_dir', ''), info);
    await cleanup('clear draw_last', () => pref('draw_last', ''), info);
    await cleanup('remove temp folder', () => rm(dir, { recursive: true, force: true }), info);
  }
});

test('Draw tab fits a phone: no horizontal overflow, library and actions reachable', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  const vw = page.viewportSize()?.width || 1280;
  test.skip(vw > 700, 'phone layout only');
  const dir = await mkdtemp(join(tmpdir(), 'amux-draw-m-'));
  await page.goto('/');
  const headers = await page.evaluate(() => ({ Authorization: 'Bearer ' + (window as unknown as Win)._AMUX_AUTH_TOKEN }));
  const pref = (key: string, value: string) => request.post('/api/prefs', { headers, data: { key, value } });
  try {
    await pref('draw_dir', dir);
    await pref('draw_last', '');
    await page.reload();
    await openDraw(page);
    const m = await page.evaluate(() => ({ sw: document.documentElement.scrollWidth, iw: innerWidth,
      host: document.getElementById('draw-host')!.getBoundingClientRect().width }));
    expect(m.sw, JSON.stringify(m)).toBeLessThanOrEqual(m.iw);
    expect(m.host, 'the canvas spans the screen').toBeGreaterThan(m.iw - 2);
    // Every amux control in the Draw chrome is a 44px touch target.
    const small = await page.evaluate(() => Array.from(document.querySelectorAll('#draw-view .draw-head button, #draw-view .draw-snapbar button'))
      .filter(b => (b as HTMLElement).offsetParent)
      .map(b => { const r = b.getBoundingClientRect(); return { id: b.id || b.getAttribute('aria-label') || b.textContent, w: r.width, h: r.height }; })
      .filter(r => r.w < 44 || r.h < 44));
    expect(small).toEqual([]);
    // The library is a bottom sheet behind a button.
    await expect(page.locator('#draw-lib')).not.toBeInViewport();
    await page.locator('#draw-lib-open').click();
    await expect(page.locator('#draw-lib')).toBeInViewport();
    await expect(page.locator('#draw-lib-save')).toBeVisible();
    await page.locator('#draw-lib-scrim').click({ position: { x: 20, y: 20 } });
    await expect(page.locator('#draw-lib')).not.toBeInViewport();
    // Export and Send are reachable.
    await page.locator('#draw-export-btn').click();
    await expect(page.locator('#draw-export-menu')).toBeVisible();
    const menu = await page.locator('#draw-export-menu').boundingBox();
    expect(menu!.x).toBeGreaterThanOrEqual(0);
    expect(menu!.x + menu!.width).toBeLessThanOrEqual(vw + 1);
  } finally {
    await cleanup('clear draw_dir', () => pref('draw_dir', ''), info);
    await cleanup('clear draw_last', () => pref('draw_last', ''), info);
    await cleanup('remove temp folder', () => rm(dir, { recursive: true, force: true }), info);
  }
});

// Draw tab (Ethan, 2026-10-13): "in draw when I click one it should open it,
// live mode should be zen mode and it should be full but it should still show
// the amux harness".
//
// Opening: every assertion reads the scene's element ids against the FILE ON
// DISK, and checks the other file is byte-for-byte unchanged, because the
// failure behind the report was a drawing whose content never reached (or was
// lost from) its file while the list still showed a picture of it.
//
// Live mode: Excalidraw's zen mode inside the amux shell. The header and tab
// bar keep their boxes, the canvas fills everything below them.
// These live in this file, not their own, because every Draw test writes the
// same global prefs (draw_dir, draw_last) and files run in parallel workers.
const sceneOf = (els: { id: string; type: string; x: number }[]) => JSON.stringify({ type: 'excalidraw', version: 2, source: 'e2e', appState: {}, files: {},
  elements: els.map((e, i) => ({ id: e.id, type: e.type, x: e.x, y: 0, width: 100, height: 60, angle: 0, strokeColor: '#1e1e1e',
    backgroundColor: 'transparent', fillStyle: 'solid', strokeWidth: 2, strokeStyle: 'solid', roughness: 1, opacity: 100,
    groupIds: [], frameId: null, roundness: null, seed: i + 1, version: 1, versionNonce: i + 1, isDeleted: false,
    boundElements: null, updated: 1, link: null, locked: false })) });

const sceneIds = (page: Page) => page.evaluate(() =>
  (window as unknown as Win)._draw.api.getSceneElements().map((e: any) => e.id).sort());
const diskIds = async (path: string) => JSON.parse(await readFile(path, 'utf8')).elements.filter((e: any) => !e.isDeleted).map((e: any) => e.id).sort();
const isPhone = (page: Page) => (page.viewportSize()?.width || 1280) <= 700;

// The library is a sidebar on a desktop and a bottom sheet on a phone; open
// whichever is closed, then the Drawings tab.
async function libOnScreen(page: Page) {
  return page.locator('#draw-lib').evaluate(el => { const r = el.getBoundingClientRect();
    return getComputedStyle(el).display !== 'none' && r.height > 0 && r.top < innerHeight - 10; });
}
async function showDrawings(page: Page) {
  if (!(await libOnScreen(page))) await page.locator('#draw-lib-open').click();
  await page.locator('#draw-libtab-drawings').click();
  await expect(page.locator('#draw-lib')).toBeInViewport();
}

// Click a row in the library. On a phone the sheet then closes so the opened
// drawing is what shows.
async function clickRow(page: Page, sel: string) {
  await showDrawings(page);
  await page.locator(sel).scrollIntoViewIfNeeded();
  // Tap once the list has stopped moving, as a person does. On iOS WebKit a
  // tap that lands while the library is still settling from a scroll only
  // stops the scroll and fires no click (draw-tab.spec.ts:356, iOS: the trace
  // shows the tap on Beta with the list at scrollTop 303 and no handler run).
  await page.waitForFunction(() => new Promise(done => {
    const body = document.getElementById('draw-lib-body');
    if (!body) return done(true);
    let last = body.scrollTop, still = 0;
    const tick = () => { const now = body.scrollTop; still = now === last ? still + 1 : 0; last = now; if (still >= 6) done(true); else requestAnimationFrame(tick); };
    requestAnimationFrame(tick);
  }));
  await page.locator(sel).click();
  if (isPhone(page)) await expect(page.locator('#draw-lib')).not.toBeInViewport();
}

const addEl = (page: Page, id: string, type = 'rectangle') => page.evaluate(([nid, t]) => {
  const d = (window as unknown as Win)._draw, X = d.lib.X;
  const els = X.convertToExcalidrawElements([{ type: t, x: 400, y: 200 + Math.random() * 200, width: 80, height: 40, id: nid }], { regenerateIds: false });
  d.api.updateScene({ elements: d.api.getSceneElementsIncludingDeleted().concat(els) });
}, [id, type]);

async function withDrawPrefs(page: Page, request: APIRequestContext, info: TestInfo, prefix: string, body: (dir: string) => Promise<void>) {
  const dir = await mkdtemp(join(tmpdir(), prefix));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  const headers = await page.evaluate(() => ({ Authorization: 'Bearer ' + (window as unknown as Win)._AMUX_AUTH_TOKEN }));
  const pref = (key: string, value: string) => request.post('/api/prefs', { headers, data: { key, value } });
  const prior = await (await request.get('/api/prefs?key=draw_dir', { headers })).json();
  try {
    expect((await pref('draw_dir', dir)).ok()).toBeTruthy();
    expect((await pref('draw_last', '')).ok()).toBeTruthy();
    await body(dir);
  } finally {
    await cleanup('restore draw_dir', () => pref('draw_dir', prior.value || ''), info);
    await cleanup('clear draw_last', () => pref('draw_last', ''), info);
    await cleanup('remove temp folder', () => rm(dir, { recursive: true, force: true }), info);
  }
}

test('Draw: clicking a drawing in Recent or In this folder opens it, and no file is written over', async ({ page, request }, info) => {
  test.setTimeout(150_000);
  page.on('console', m => { if (/\[draw\]/.test(m.text())) console.log('[page]', m.text().slice(0, 300)); });
  await withDrawPrefs(page, request, info, 'amux-draw-open-', async dir => {
    const a = join(dir, 'Alpha.excalidraw'), b = join(dir, 'Beta.excalidraw');
    await writeFile(a, sceneOf([{ id: 'alpha-rect', type: 'rectangle', x: 0 }, { id: 'alpha-ell', type: 'ellipse', x: 150 }]));
    await writeFile(b, sceneOf([{ id: 'beta-diamond', type: 'diamond', x: 0 }]));
    await page.reload();
    await openDraw(page);
    const saved = () => expect(page.locator('#draw-save-state')).toHaveAttribute('data-state', 'saved', { timeout: 15_000 });
    // Edit each through the app so both files were last written by it.
    await page.evaluate(p => (window as any)._drawOpen(p), a);
    await expect.poll(() => sceneIds(page)).toEqual(['alpha-ell', 'alpha-rect']);
    await addEl(page, 'alpha-new');
    // The status already read "saved" from opening, so wait on the file itself.
    await expect.poll(() => diskIds(a), { timeout: 15_000 }).toEqual(['alpha-ell', 'alpha-new', 'alpha-rect']);
    await saved();
    await page.evaluate(p => (window as any)._drawOpen(p), b);
    await expect.poll(() => sceneIds(page)).toEqual(['beta-diamond']);
    await addEl(page, 'beta-new');
    await expect.poll(() => diskIds(b), { timeout: 15_000 }).toEqual(['beta-diamond', 'beta-new']);
    await saved();
    const aBytes = await readFile(a, 'utf8');
    let bBytes = await readFile(b, 'utf8');

    // Recent drawings: click Alpha.
    await showDrawings(page);
    await clickRow(page, `.draw-recent[data-path="${a}"]`);
    await expect.poll(() => sceneIds(page)).toEqual(['alpha-ell', 'alpha-new', 'alpha-rect']);
    expect(await page.evaluate(() => (window as unknown as Win)._draw.path)).toBe(a);
    await expect(page.locator('#draw-title')).toHaveValue('Alpha');
    await page.screenshot({ path: join(process.env.DRAW_SHOTS_DIR || info.outputPath(), 'open-' + info.project.name + '.png') });
    expect(await readFile(b, 'utf8'), 'opening Alpha left Beta untouched').toBe(bBytes);
    expect(await readFile(a, 'utf8'), 'opening Alpha did not rewrite it').toBe(aBytes);

    // In this folder: click Beta.
    await clickRow(page, `.draw-pop-file[data-path="${b}"]`);
    await expect.poll(() => sceneIds(page)).toEqual(['beta-diamond', 'beta-new']);
    expect(await readFile(a, 'utf8'), 'opening Beta left Alpha untouched').toBe(aBytes);

    // After a reload the list still opens what it shows. (The recents list
    // is written 1.5s after a change; let that write land first.)
    await page.waitForFunction(() => !(window as unknown as Win)._draw.recentsTimer, null, { timeout: 10_000 });
    await page.waitForTimeout(500);
    await page.reload();
    await openDraw(page);
    await expect.poll(() => sceneIds(page)).toEqual(['beta-diamond', 'beta-new']);
    await showDrawings(page);
    await clickRow(page, `.draw-recent[data-path="${a}"]`);
    await expect.poll(() => sceneIds(page)).toEqual(['alpha-ell', 'alpha-new', 'alpha-rect']);
    expect(await readFile(b, 'utf8')).toBe(bBytes);

    // An edit still inside the autosave debounce survives switching away, and
    // a write already in flight is not overtaken: Alpha keeps both edits and
    // Beta is not written.
    let slowWrites = true;
    await page.route('**/api/file', async route => {
      if (slowWrites && route.request().method() === 'PUT') await new Promise(r => setTimeout(r, 1500));
      await route.fallback();
    });
    await addEl(page, 'alpha-inflight', 'ellipse');
    await expect(page.locator('#draw-save-state')).toHaveAttribute('data-state', 'saving', { timeout: 5_000 });
    await addEl(page, 'alpha-pending', 'ellipse');
    bBytes = await readFile(b, 'utf8');
    await clickRow(page, `.draw-pop-file[data-path="${b}"]`);
    await expect.poll(() => sceneIds(page), { timeout: 15_000 }).toEqual(['beta-diamond', 'beta-new']);
    expect(await diskIds(a)).toEqual(['alpha-ell', 'alpha-inflight', 'alpha-new', 'alpha-pending', 'alpha-rect']);
    expect(await readFile(b, 'utf8')).toBe(bBytes);
    slowWrites = false;

    // An editor that loses its scene (no elements at all, not even deleted
    // ones) is never saved over a file that has content.
    // The refusal is announced, and the drawing comes back from its file.
    const refused = page.waitForEvent('console', { predicate: m => m.text().includes('save-refused-empty-scene'), timeout: 10_000 });
    await page.evaluate(() => (window as unknown as Win)._draw.api.updateScene({ elements: [] }));
    await refused;
    await expect.poll(() => sceneIds(page)).toEqual(['beta-diamond', 'beta-new']);
    expect(await readFile(b, 'utf8'), 'the lost scene was not written').toBe(bBytes);

    // A drawing the user really empties saves empty, and its list entry stops
    // showing a picture of the old content.
    await page.evaluate(p => (window as any)._drawOpen(p), a);
    await expect.poll(() => sceneIds(page)).toEqual(await diskIds(a));
    await page.evaluate(() => { const api = (window as unknown as Win)._draw.api;
      api.updateScene({ elements: api.getSceneElementsIncludingDeleted().map((e: any) => ({ ...e, isDeleted: true, version: e.version + 1, versionNonce: e.versionNonce + 1 })) }); });
    // Wait on the file, as above: the status already reads "saved" from the
    // open, and on WebKit the change that flips it to dirty can land after a
    // status check, so saved() alone returned before this save was written.
    await expect.poll(() => diskIds(a), { timeout: 15_000 }).toEqual([]);
    await saved();
    await page.evaluate(p => (window as any)._drawOpen(p), a);
    await expect.poll(() => page.evaluate(() => { const d = (window as unknown as Win)._draw;
      const e = d.recents.find((x: any) => x.path === d.path); return e ? !!e.thumb : null; })).toBe(false);
  });
});

test('Draw: live mode is zen mode inside the amux shell, and every exit restores the layout', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  await withDrawPrefs(page, request, info, 'amux-draw-live-', async dir => {
    await writeFile(join(dir, 'Live.excalidraw'), sceneOf([{ id: 'live-rect', type: 'rectangle', x: 0 }]));
    await page.reload();
    await openDraw(page);
    await page.evaluate(p => (window as any)._drawOpen(p), join(dir, 'Live.excalidraw'));
    await expect.poll(() => sceneIds(page)).toEqual(['live-rect']);
    const vw = page.viewportSize()!.width, vh = page.viewportSize()!.height;
    const shell = () => page.evaluate(() => ['.header-row', '.tab-bar-outer'].map(sel => {
      const el = document.querySelector(sel) as HTMLElement | null;
      if (!el || !el.offsetParent) return { sel, visible: false };
      const r = el.getBoundingClientRect();
      return { sel, visible: r.height > 0, top: Math.round(r.top), bottom: Math.round(r.bottom) };
    }));
    const zen = () => page.evaluate(() => !!(window as unknown as Win)._draw.api.getAppState().zenModeEnabled);
    const before = await shell();
    expect(before.some(s => s.visible), JSON.stringify(before)).toBe(true);

    const enter = async () => {
      await page.locator('#draw-live-btn').click();
      await expect.poll(zen).toBe(true);
      await expect(page.locator('#draw-penbar')).toBeVisible();
    };
    const assertLive = async (label: string) => {
      expect(await shell(), label + ': the amux header and tab bar stay where they were').toEqual(before);
      await expect(page.locator('#draw-view .draw-head')).toBeHidden();
      await expect(page.locator('#draw-view .draw-status')).toBeHidden();
      await expect(page.locator('#draw-lib')).not.toBeInViewport();
      const shellBottom = Math.max(...before.filter(s => s.visible).map(s => s.bottom!));
      const c = (await page.locator('#draw-host').boundingBox())!;
      const m = { c, shellBottom, vw, vh, sw: await page.evaluate(() => document.documentElement.scrollWidth) };
      expect(c.y, label + ' canvas starts below the shell ' + JSON.stringify(m)).toBeGreaterThanOrEqual(shellBottom - 1);
      expect(c.y - shellBottom, label + ' and right below it ' + JSON.stringify(m)).toBeLessThan(24);
      expect(c.x, JSON.stringify(m)).toBeLessThanOrEqual(isPhone(page) ? 1 : 24);
      expect(c.x, JSON.stringify(m)).toBeGreaterThanOrEqual(-1);
      expect(c.x + c.width, JSON.stringify(m)).toBeGreaterThanOrEqual(vw - (isPhone(page) ? 1 : 24));
      expect(c.x + c.width, JSON.stringify(m)).toBeLessThanOrEqual(vw + 1);
      expect(c.y + c.height, JSON.stringify(m)).toBeGreaterThan(vh - 40);
      expect(c.y + c.height, JSON.stringify(m)).toBeLessThanOrEqual(vh + 1);
      expect(m.sw, 'no horizontal overflow').toBeLessThanOrEqual(vw);
      // The pen bar sits inside the screen.
      const pb = (await page.locator('#draw-penbar').boundingBox())!;
      expect(pb.x).toBeGreaterThanOrEqual(0);
      expect(pb.x + pb.width).toBeLessThanOrEqual(vw + 1);
      expect(pb.y + pb.height).toBeLessThanOrEqual(vh + 1);
    };
    const assertNormal = async (label: string) => {
      await expect.poll(zen, label + ': zen is off').toBe(false);
      await expect(page.locator('#draw-penbar')).toBeHidden();
      await expect(page.locator('#draw-view .draw-head')).toBeVisible();
      await expect(page.locator('#draw-view')).not.toHaveClass(/draw-live/);
      if (!isPhone(page)) await expect(page.locator('#draw-lib')).toBeVisible();
      expect(await shell()).toEqual(before);
    };

    // Exit 1: the pen bar's close button.
    await enter();
    await assertLive('live');
    await page.screenshot({ path: join(process.env.DRAW_SHOTS_DIR || info.outputPath(), 'live-' + info.project.name + '.png') });
    await page.locator('#draw-penbar [aria-label="Exit live mode"]').click();
    await assertNormal('pen bar close');

    // The autosave that follows a Live mode press is the page's own write. It
    // used to be bound to that press and disable the button while it ran, so
    // the next tap on WebKit hit a disabled button and live mode never started.
    await page.evaluate(() => {
      const b = document.getElementById('draw-live-btn') as HTMLButtonElement;
      (window as any).__liveLocks = 0;
      new MutationObserver(() => { if (b.disabled || b.dataset.pressLocked) (window as any).__liveLocks++; })
        .observe(b, { attributes: true, attributeFilter: ['disabled', 'data-press-locked'] });
    });
    const saved = page.waitForResponse(r => r.request().method() === 'PUT' && r.url().endsWith('/api/file')
      && String(JSON.parse(r.request().postData() || '{}').path || '').endsWith('Live.excalidraw'));
    await page.evaluate(() => { const api = (window as unknown as Win)._draw.api; const els = api.getSceneElements();
      api.updateScene({ elements: [...els, { ...els[0], id: 'live-dirty', x: 300, version: 2, versionNonce: 7 }] }); });
    expect((await saved).ok()).toBe(true);
    expect(await page.evaluate(() => (window as any).__liveLocks), 'autosave locked the Live mode button').toBe(0);

    // Exit 2: Excalidraw's own "Exit zen mode". Its phone layout has no such
    // button (zen exits there through the pen bar or Esc).
    if (!isPhone(page)) {
      await enter();
      await page.locator('#draw-host .disable-zen-mode').click();
      await assertNormal('Exit zen mode');
    }

    // Exit 3: Esc.
    await enter();
    await page.keyboard.press('Escape');
    await assertNormal('Esc');

    // Zen turned on from the editor's side is live mode too.
    await page.evaluate(() => (window as unknown as Win)._draw.api.updateScene({ appState: { zenModeEnabled: true } }));
    await expect(page.locator('#draw-view')).toHaveClass(/draw-live/);
    await expect(page.locator('#draw-penbar')).toBeVisible();
    await page.locator('#draw-penbar [aria-label="Exit live mode"]').click();
    await assertNormal('zen from the editor');
  });
});

// "looks like its stuck" (Ethan, 2026-10-13): the page behind the Draw view had
// scrolled, so the Draw header and the editor toolbar were off screen and the
// canvas sat under the snap bar. <html> is overflow:hidden, which stops a user
// scrolling back but not a focus() inside the editor or a header that grew
// after the view was measured. After every step here the page is at the top,
// the view fits the window, both toolbars are on screen, and a drag draws.
test('Draw: the page behind the editor never scrolls, and input still reaches the canvas', async ({ page, request }, info) => {
  test.setTimeout(150_000);
  await withDrawPrefs(page, request, info, 'amux-draw-pin-', async dir => {
    const one = join(dir, 'One.excalidraw'), two = join(dir, 'Two.excalidraw');
    await writeFile(one, sceneOf([{ id: 'one-rect', type: 'rectangle', x: 0 }]));
    await writeFile(two, sceneOf([{ id: 'two-ell', type: 'ellipse', x: 0 }]));
    await page.reload();
    await openDraw(page);
    await page.evaluate(p => (window as any)._drawOpen(p), one);
    await expect.poll(() => sceneIds(page)).toEqual(['one-rect']);

    const settled = async (label: string) => {
      await expect.poll(() => page.evaluate(() => (document.scrollingElement || document.documentElement).scrollTop), { message: label + ': page at the top' }).toBe(0);
      const vh = page.viewportSize()!.height;
      await expect.poll(() => page.evaluate(() => Math.round(document.getElementById('draw-view')!.getBoundingClientRect().bottom)),
        { message: label + ': the view fits the window' }).toBeLessThanOrEqual(vh + 1);
      for (const sel of ['#draw-view .draw-head', '#draw-host .App-toolbar']) {
        const b = await page.locator(sel).first().boundingBox();
        expect(b, label + ': ' + sel).toBeTruthy();
        expect(b!.y, label + ': ' + sel + ' top on screen').toBeGreaterThanOrEqual(0);
        expect(b!.y + b!.height, label + ': ' + sel + ' bottom on screen').toBeLessThanOrEqual(vh + 1);
      }
      // A drag on the canvas still draws.
      const before = (await sceneIds(page)).length;
      await setTool(page, 'rectangle');
      const c = (await page.locator('#draw-host canvas.interactive').boundingBox())!;
      const x = c.x + c.width * 0.55, y = c.y + c.height * 0.45;
      await drag(page, [x, y], [x + 50, y + 30], 6);
      await expect.poll(() => sceneIds(page).then(a => a.length), { message: label + ': a drag draws' }).toBe(before + 1);
    };

    await settled('after a drawing opened');
    await clickRow(page, '.draw-pop-file[data-path$="/Two.excalidraw"]');
    await expect.poll(() => sceneIds(page).then(a => a.includes('two-ell'))).toBe(true);
    await settled('after clicking another drawing');

    await page.locator('#draw-live-btn').click();
    await expect(page.locator('#draw-penbar')).toBeVisible();
    await page.locator('#draw-penbar [aria-label="Exit live mode"]').click();
    await expect(page.locator('#draw-penbar')).toBeHidden();
    await settled('after live mode in and out');

    const vp = page.viewportSize()!;
    await page.setViewportSize({ width: vp.width, height: vp.height - 120 });
    await settled('after the window got shorter');
    await page.setViewportSize(vp);
    await settled('after it grew back');

    // The shell above the view grows after the view was measured, and
    // something inside scrolls the page: both heal on their own.
    await page.evaluate(() => {
      const b = document.createElement('div');
      b.id = 'e2e-draw-banner'; b.style.height = '90px'; b.textContent = 'banner';
      const bar = document.querySelector('.tab-bar-outer')!;
      bar.parentElement!.insertBefore(b, bar);
    });
    await settled('after the header grew');
    await page.evaluate(() => document.getElementById('e2e-draw-banner')!.remove());
    await settled('after the header shrank back');
    // Something gives the page room to scroll (content below the view) and a
    // focus or scrollIntoView inside scrolls it: the page goes back to the top.
    await page.evaluate(() => {
      const s = document.createElement('div');
      s.id = 'e2e-draw-spacer'; s.style.height = '1500px';
      document.body.appendChild(s);
    });
    // Scroll only after the resize has been handled, so this step tests the
    // scroll path on its own.
    await page.waitForTimeout(400);
    await page.evaluate(() => { (document.scrollingElement || document.documentElement).scrollTop = 400; });
    await settled('after the page was scrolled');
    await page.evaluate(() => document.getElementById('e2e-draw-spacer')!.remove());
  });
});

// ── Folded in from draw-pref-order.spec.ts and draw-canvas-offset.spec.ts ──
// Every Draw test reads and writes the same server-side prefs (draw_settings,
// draw_dir, draw_last), so tests in separate files raced when two CI workers
// ran them at once (e.g. snap settings read back inverted, 2026-10-13). One file
// runs its tests in order in one worker.

// Draw tab: three quick snap toggles each saved the settings with their own
// POST, and whichever finished last won, so the stored settings could be older
// than what the editor showed (draw-tab.spec.ts failed on CI, 2026-10-12).
// The first save is held back here so it would finish last.

test('snap settings are stored in click order even when an early save is slow', async ({page}, info) => {
  test.skip(info.project.name !== 'desktop', 'request ordering is the same on every device');
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await page.evaluate(() => (window as any).switchView('draw'));
  await page.waitForFunction(() => { const d = (0, eval)('_draw'); return !!(d && d.api && d.path && !d.opening); }, null, {timeout: 30_000});
  const sent: string[] = [];
  let held = false;
  await page.route('**/api/prefs', async route => {
    const body = route.request().postDataJSON?.();
    if (route.request().method() !== 'POST' || !body || body.key !== 'draw_settings') return route.continue();
    sent.push(body.value);
    if (!held) { held = true; await new Promise(r => setTimeout(r, 800)); }
    await route.continue();
  });
  const before = await page.evaluate(() => JSON.stringify((0, eval)('_draw').settings));
  await page.evaluate(() => { const t = (0, eval)('_drawToggleSnap'); t('grid'); t('align'); t('connect'); });
  const want = await page.evaluate(() => JSON.stringify((0, eval)('_draw').settings));
  expect(want).not.toBe(before);
  const headers = await page.evaluate(() => ({Authorization: 'Bearer ' + (window as any)._AMUX_AUTH_TOKEN}));
  await expect.poll(async () => (await (await page.request.get('/api/prefs?key=draw_settings', {headers})).json()).value, {timeout: 10_000})
    .toBe(want);
  await page.waitForTimeout(1200);
  expect((await (await page.request.get('/api/prefs?key=draw_settings', {headers})).json()).value, 'the last click wins after every save lands').toBe(want);
  expect(sent.length).toBe(3);
});

// Draw tab: a stroke landed far from the cursor (Ethan, 2026-10-12). The editor
// caches where its canvas sits and re-measures only on its own resize and on
// scroll, so a canvas that MOVES without resizing left every stroke offset by
// the distance it moved. Reads the canvas pixels under the pointer.

test('a stroke lands under the cursor after the canvas moves without resizing', async ({page}, info) => {
  test.skip(info.project.name !== 'desktop', 'mouse drag; the offset cache is the same on every device');
  test.setTimeout(90_000);
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await page.evaluate(() => (window as any).switchView('draw'));
  await page.waitForFunction(() => { const d = (0, eval)('_draw'); return !!(d && d.api && d.path && !d.opening); }, null, {timeout: 30_000});
  await page.evaluate(() => (0, eval)('_draw').api.setActiveTool({type: 'freedraw'}));
  // Move the canvas down and right, same size: no resize, no scroll.
  await page.evaluate(() => {
    const w = document.getElementById('draw-canvas-wrap')!;
    w.style.position = 'relative'; w.style.left = '90px'; w.style.top = '70px';
  });
  await page.waitForTimeout(300);
  const box = (await page.locator('#draw-host canvas.interactive').boundingBox())!;
  const a = [box.x + box.width * 0.4, box.y + box.height * 0.4];
  await page.mouse.move(a[0] - 30, a[1]);
  await page.mouse.move(a[0], a[1]);
  await page.waitForTimeout(150);
  await page.mouse.down();
  for (let i = 1; i <= 12; i++) await page.mouse.move(a[0] + 10 * i, a[1]);
  await page.mouse.up();
  await expect.poll(() => page.evaluate(() => (0, eval)('_draw').api.getSceneElements().length)).toBe(1);
  await page.waitForTimeout(300);
  const ink = await page.evaluate(([x, y]) => {
    const c = document.querySelector('#draw-host canvas.static') as HTMLCanvasElement;
    const r = c.getBoundingClientRect(), sx = c.width / r.width, sy = c.height / r.height;
    const d = c.getContext('2d')!.getImageData(Math.round((x - r.left) * sx) - 6, Math.round((y - r.top) * sy) - 6, 12, 12).data;
    let n = 0; for (let i = 3; i < d.length; i += 4) if (d[i] > 40) n++;
    return n;
  }, [a[0] + 60, a[1]]);
  expect(ink, 'ink under the middle of the drag').toBeGreaterThan(0);
  const st = await page.evaluate(() => {
    const s = (0, eval)('_draw').api.getAppState(), r = document.querySelector('#draw-host .excalidraw')!.getBoundingClientRect();
    return {dx: Math.round(r.left - s.offsetLeft), dy: Math.round(r.top - s.offsetTop)};
  });
  expect(st, 'the editor knows where its canvas is').toEqual({dx: 0, dy: 0});
});

// The amux UI zoom (Settings, e.g. 130%) is CSS zoom on <html>. Excalidraw maps
// pointer coordinates for an unzoomed page, so under it a stroke landed
// 100-200px from the cursor (Ethan, 2026-10-13). The truth is the rendered
// screenshot: dark pixels must lie along the pointer's path. (Coordinates read
// from getBoundingClientRect cannot show it: they agree with the editor's cache
// while the ink is elsewhere.) The root drops CSS zoom while Draw is open and
// gets it back on leaving.
for (const zoom of ['130', '80']) {
  test(`a stroke lands under the cursor at ${zoom}% UI zoom`, async ({page, browser}, info) => {
    test.skip(!info.project.name.startsWith('desktop'), 'desktop applies the UI zoom as CSS zoom; touch devices scale font-size instead');
    test.setTimeout(120_000);
    await page.addInitScript((z) => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.setItem('amux_zoom', z); }, zoom);
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any).switchView === 'function');
    expect(await page.evaluate(() => document.documentElement.style.zoom)).toBe(String(Number(zoom) / 100));
    await page.evaluate(() => (window as any).switchView('draw'));
    await page.waitForFunction(() => { const d = (0, eval)('_draw'); return !!(d && d.api && d.path && !d.opening); }, null, {timeout: 30_000});
    expect(await page.evaluate(() => document.documentElement.style.zoom), 'no CSS zoom on the root while Draw is open').toBe('');
    await page.evaluate(() => (0, eval)('_draw').api.setActiveTool({type: 'freedraw'}));
    await page.waitForTimeout(300);
    const box = (await page.locator('#draw-host canvas.interactive').boundingBox())!;
    const y = Math.round(box.y + box.height * 0.5), x0 = Math.round(box.x + box.width * 0.3), x1 = Math.round(box.x + box.width * 0.7);
    await page.mouse.move(x0, y); await page.mouse.down();
    for (let i = 1; i <= 20; i++) await page.mouse.move(x0 + (x1 - x0) * i / 20, y);
    await page.mouse.up();
    await page.waitForTimeout(600);
    const shot = (await page.screenshot()).toString('base64');
    const dec = await browser.newPage();
    const vw = page.viewportSize()!.width;
    const onPath = await dec.evaluate(async ({shot, x0, x1, y, vw}) => {
      const img = new Image(); img.src = 'data:image/png;base64,' + shot; await img.decode();
      const c = document.createElement('canvas'); c.width = img.width; c.height = img.height;
      const g = c.getContext('2d')!; g.drawImage(img, 0, 0);
      const d = g.getImageData(0, 0, c.width, c.height).data;
      const k = img.width / vw;   // screenshot pixels per CSS pixel (2 on a Retina target)
      let n = 0;
      for (let x = x0 + 30; x < x1 - 30; x++) for (let yy = y - 4; yy <= y + 4; yy++) {
        const i = (c.width * Math.round(yy * k) + Math.round(x * k)) * 4; if (d[i] < 90 && d[i + 1] < 90 && d[i + 2] < 90) n++;
      }
      return n;
    }, {shot, x0, x1, y, vw});
    await dec.close();
    expect(onPath, `ink on the pointer's path at ${zoom}% (screenshot pixels)`).toBeGreaterThan((x1 - x0 - 60) * 2);
    await page.evaluate(() => (window as any).switchView('sessions'));
    expect(await page.evaluate(() => document.documentElement.style.zoom), 'the UI zoom comes back on leaving Draw').toBe(String(Number(zoom) / 100));
  });
}

// Every tab and device writes one recents file. A tab used to write only its
// own list, so a drawing another tab had opened vanished from Recent (the
// draw-realtime clients erased this file's Alpha entry in CI).
test('Draw: saving recents keeps entries another tab wrote', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  await withDrawPrefs(page, request, info, 'amux-draw-recents-', async dir => {
    const mine = join(dir, 'Mine.excalidraw');
    await writeFile(mine, sceneOf([{ id: 'mine-rect', type: 'rectangle', x: 0 }]));
    await page.reload();
    await openDraw(page);
    await page.evaluate(p => (window as any)._drawOpen(p), mine);
    await expect.poll(() => sceneIds(page)).toEqual(['mine-rect']);
    await page.waitForFunction(() => !(window as unknown as Win)._draw.recentsTimer, null, { timeout: 10_000 });
    // Another tab's write lands on disk after this tab loaded its list.
    const other = join(dir, 'OtherTab.excalidraw');
    await page.evaluate(async o => {
      const path = (window as any)._drawRecentsPath();
      const res = await fetch('/api/file/raw?path=' + encodeURIComponent(path), { cache: 'no-store' }); const raw = res.ok ? await res.text() : ''; const cur = raw ? JSON.parse(raw) : { recents: [] };
      cur.recents.unshift({ path: o, title: 'OtherTab', ts: Date.now() + 1 });
      await fetch('/api/file', { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ path, content: JSON.stringify(cur) }) });
    }, other);
    await page.evaluate(() => (window as any)._drawRememberRecent(true));
    await page.waitForFunction(() => !(window as unknown as Win)._draw.recentsTimer, null, { timeout: 10_000 });
    await expect.poll(() => page.evaluate(async () => {
      const path = (window as any)._drawRecentsPath();
      const res = await fetch('/api/file/raw?path=' + encodeURIComponent(path), { cache: 'no-store' }); const raw = res.ok ? await res.text() : ''; const cur = raw ? JSON.parse(raw) : { recents: [] };
      return cur.recents.map((x: any) => x.path);
    })).toEqual(expect.arrayContaining([mine, other]));
    await page.evaluate(() => (window as any)._drawLibRender());
    await showDrawings(page);
    await expect(page.locator(`.draw-recent[data-path="${other}"]`)).toHaveCount(1);
    // A re-render keeps the library where the user scrolled it: a scrolled
    // list used to jump back to the top under a tap (draw-tab.spec.ts:356, iOS).
    const kept = await page.evaluate(async () => {
      const body = document.getElementById('draw-lib-body')!;
      body.style.maxHeight = '120px';                 // force a scrollable list
      body.scrollTop = 40;
      const before = body.scrollTop;
      (window as any)._draw.recents.push({ path: '/tmp/force-rerender-' + Date.now() + '.excalidraw', title: 'z', ts: 1 });
      await (window as any)._drawLibRender();
      const after = body.scrollTop;
      body.style.maxHeight = '';
      return { before, after };
    });
    expect(kept.before, 'the list scrolled at all').toBeGreaterThan(0);
    expect(kept.after, 'scroll kept across a re-render').toBe(kept.before);
  });
});
