// Owner, 2026-11-21: "if there's a relative path that is being sent back in a
// response, I can't middle-mouse-button-click it to open a new tab". A path in
// worker output was a <span> with an onclick, so the browser had no address to
// open. It is now an <a> whose href is this dashboard's #path= link to the
// file, refreshed with the server's resolution of the relative path when the
// pointer reaches it. Real render pipeline (_peekHtml), real pointer input.
//
// What a middle click does with an uncancelled <a href> is the browser's, so
// this pins the three things amux owns: the href, that nothing amux runs
// cancels the press or opens the file in place, and that the href opened in a
// new tab lands on the file. A real headless middle click opened no tab about
// once in 15 runs on a loaded host with the link pressed and its href right,
// so it is not the assertion (same choice as own-link-new-tab-same-origin).
import { test, expect } from './fixtures';
import fs from 'fs';
import os from 'os';
import path from 'path';

const CWD = '/private/tmp/amux-e2e-newtab';
// The page is the bar's first tab (id 1) and shows no worker, so the link
// opens as a viewer that returns to tab 1 (e2e/link-viewer-return.spec.ts).
const VIEWER = '/?_viewer=1&_from_tab=1';
const RAW = '\x1b[37m⏺\x1b[39m Wrote the plan to notes/plan.md and the data to data/x.csv.\n'
  + '  Both are ready.\n';

// The server finds the file outside the blind join (AMUX-3511: the worker
// wrote the path from its repo root), so the two hrefs differ. A real file, so
// the new tab has something to open.
let resolved = '';
test.beforeAll(() => {
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'amux-newtab-')));
  fs.mkdirSync(path.join(dir, 'notes'));
  resolved = path.join(dir, 'notes', 'plan.md');
  fs.writeFileSync(resolved, '# The plan\n\nOpened in its own tab.\n');
});
test.afterAll(() => { if (resolved) fs.rmSync(path.dirname(path.dirname(resolved)), { recursive: true, force: true }); });

async function render(page: import('@playwright/test').Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route('**/api/fs/resolve?*', route => {
    const rel = new URL(route.request().url()).searchParams.get('rel');
    route.fulfill({ json: { resolved: rel === 'notes/plan.md' ? resolved : null } });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekHtml === 'function');
  await page.evaluate(({ cwd, raw }) => {
    eval('peekSessionDir = ' + JSON.stringify(cwd));
    const body = document.createElement('div');
    body.id = 'newtab-probe';
    body.className = 'overlay-body';
    body.innerHTML = (window as any)._peekHtml(raw);
    document.body.prepend(body);
  }, { cwd: CWD, raw: RAW });
}

// Every event a middle or ⌘/ctrl click sends, dispatched on the link. For
// each: did anything amux runs cancel it, and did the file open in place.
async function pressWith(link: import('@playwright/test').Locator) {
  return link.evaluate((a: HTMLAnchorElement) => {
    const w = window as any; const opened: string[] = []; const orig = w._openPathFromOutput;
    w._openPathFromOutput = (p: string) => { opened.push(p); };
    const cancelled: Record<string, boolean> = {};
    const press = (name: string, type: string, init: MouseEventInit) => {
      // On the link, after its own onclick (which stops propagation so the
      // press cannot toggle what is behind it): it reads what amux's handlers
      // left, then keeps this page from following the link. The capture-phase
      // document handlers have all run by then.
      const rec = (e: Event) => { cancelled[name] = e.defaultPrevented; e.preventDefault(); };
      a.addEventListener(type, rec, { once: true });
      a.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, ...init }));
      a.removeEventListener(type, rec);
    };
    press('middle-down', 'mousedown', { button: 1, buttons: 4 });
    press('middle-aux', 'auxclick', { button: 1 });
    press('ctrl-click', 'click', { button: 0, ctrlKey: true });
    press('meta-click', 'click', { button: 0, metaKey: true });
    w._openPathFromOutput = orig;
    return { cancelled, opened: opened.length };
  });
}

test('a relative path in a reply is a link that opens the file in a new tab', async ({ page, context }) => {
  test.setTimeout(60_000);
  await render(page);
  const link = page.locator('#newtab-probe a.md-link[data-path="notes/plan.md"]');
  const csv = page.locator('#newtab-probe a.file-link[data-path="data/x.csv"]');
  await expect(link, 'the relative .md path must render as a link').toHaveCount(1);
  await expect(csv).toHaveCount(1);
  const origin = new URL(page.url()).origin;
  // Before the pointer arrives: the blind join against the worker's cwd.
  await expect(link).toHaveAttribute('href', origin + VIEWER + '#path=' + CWD + '/notes/plan.md');
  // A server with no answer leaves the blind join in place.
  await csv.hover();
  await expect(csv).toHaveAttribute('data-resolved', '1');
  await expect(csv).toHaveAttribute('href', origin + VIEWER + '#path=' + CWD + '/data/x.csv');
  // Hovering asks the server; its answer becomes the href.
  await link.hover();
  await expect(link).toHaveAttribute('href', origin + VIEWER + '#path=' + resolved);

  // Nothing amux runs cancels a middle or modifier press, or opens the file
  // here instead.
  const none = { 'middle-down': false, 'middle-aux': false, 'ctrl-click': false, 'meta-click': false };
  expect(await pressWith(link), 'a middle or ⌘/ctrl click is left to the browser').toEqual({ cancelled: none, opened: 0 });
  // The press did not fold the reply the link sits in.
  await page.waitForTimeout(400);
  await expect(page.locator('#newtab-probe .ptc').first(), 'a press on the link must not fold the reply').not.toHaveClass(/collapsed/);

  // The tab the browser opens is that file in the viewer.
  const tab = await context.newPage();
  await tab.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await tab.goto((await link.getAttribute('href'))!);
  await expect(tab.locator('#file-overlay')).toHaveClass(/active/, { timeout: 30_000 });
  await expect(tab.locator('#file-title')).toContainText('plan.md');
  await tab.close();
});

test('a plain click still opens the file in place and opens no tab', async ({ page, context }) => {
  await render(page);
  await page.evaluate(() => {
    (window as any).__opened = [];
    (window as any)._openPathFromOutput = (p: string) => { (window as any).__opened.push(p); };
  });
  let tabs = 0;
  context.on('page', () => { tabs++; });
  const before = page.url();
  await page.locator('#newtab-probe a.file-link[data-path="data/x.csv"]').click();
  await expect.poll(() => page.evaluate(() => (window as any).__opened)).toEqual(['data/x.csv']);
  await page.waitForTimeout(300);
  expect(tabs, 'a plain click opens no new tab').toBe(0);
  expect(page.url(), 'a plain click does not navigate to the href').toBe(before);
});

test('a click that ends a text selection neither opens the file nor follows the href', async ({ page }) => {
  await render(page);
  const csv = page.locator('#newtab-probe a.file-link[data-path="data/x.csv"]');
  await csv.hover();  // the pointer arrives before any click, as it does for a person
  await expect(csv).toHaveAttribute('data-resolved', '1');
  const out = await csv.evaluate((a: HTMLAnchorElement) => {
    const w = window as any; const opened: string[] = []; const orig = w._openPathFromOutput;
    w._openPathFromOutput = (p: string) => { opened.push(p); };
    // The state a double-click or drag-select leaves: the link's text selected.
    const r = document.createRange(); r.selectNodeContents(a);
    const sel = window.getSelection()!; sel.removeAllRanges(); sel.addRange(r);
    let followed = true;
    a.addEventListener('click', e => { followed = !e.defaultPrevented; e.preventDefault(); }, { once: true });
    a.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, button: 0, detail: 2 }));
    sel.removeAllRanges(); w._openPathFromOutput = orig;
    return { followed, opened: opened.length };
  });
  expect(out, 'a selecting click stays on this page and opens nothing').toEqual({ followed: false, opened: 0 });
});
