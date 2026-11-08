// Editing the buttons above the message box (Ethan, 2026-11-08: "improve the
// ability to modify the input buttons ... today it doesnt save and the drag is
// not intuitive/ordering isnt saved locally"). In edit mode a press never
// sends; a drag moves the button among its neighbours and the order on screen
// is what is saved; the server holds it (a reload shows it); a save the
// server refused stays pending and wins at the next boot; arrow keys move a
// focused button; a later default removed on another device stays removed.
// Runs in every project, the 375px phone included.
import { test, expect, Page } from './fixtures';

const W = { name: 'chipedit-w', status: 'idle', running: true, renderer: 'terminal' };
const chip = (id: string) => ({ id, label: id.toUpperCase(), action: 'send', value: id });
const START = ['aa', 'bb', 'cc', 'dd'].map(chip);

type Server = { chips: string | null; offered: string | null; failPosts: boolean; posts: any[]; sends: number };

async function boot(page: Page, srv: Server, local?: { chips?: object[]; pending?: boolean }) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, (r) => r.request().method() === 'GET' ? r.fulfill({ json: [W] }) : r.fallback());
  await page.route(/\/api\/prefs(\?.*)?$/, async (r) => {
    const req = r.request();
    if (req.method() === 'POST') {
      const b = req.postDataJSON();
      if (b.key !== 'ui_chips' && b.key !== 'ui_chips_offered') return r.fallback();
      srv.posts.push(b);
      if (srv.failPosts) return r.fulfill({ status: 500, json: { error: 'down' } });
      if (b.key === 'ui_chips') srv.chips = b.value; else srv.offered = b.value;
      return r.fulfill({ json: { ok: true, key: b.key, value: b.value } });
    }
    const key = new URL(req.url()).searchParams.get('key');
    if (key === 'ui_chips') return r.fulfill({ json: { key, value: srv.chips } });
    if (key === 'ui_chips_offered') return r.fulfill({ json: { key, value: srv.offered } });
    return r.fallback();
  });
  await page.addInitScript((l: any) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    if (l && l.chips && !sessionStorage.getItem('seeded')) {
      localStorage.setItem('amux_chips', JSON.stringify(l.chips));
      if (l.pending) localStorage.setItem('amux_chips_pending', '1');
      sessionStorage.setItem('seeded', '1');
    }
  }, local || null);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).renderChips === 'function' && (0, eval)('typeof _chipsSynced !== "undefined" && _chipsSynced') === true);
  await page.evaluate((w: any) => {
    const g = globalThis as any;
    if (!g.eval('sessions').some((x: any) => x.name === w.name)) g.eval('sessions').push(w);
    g.openPeek(w.name);
    g.setPeekTab('terminal');
  }, W);
  await expect(page.locator('#peek-chips .chip[data-chip-idx]').first()).toBeVisible();
}
const shown = (page: Page) => page.locator('#peek-chips .chip[data-chip-idx]').allInnerTexts()
  .then((t) => t.map((x) => x.replace(/[^A-Z]/g, '')).filter((x) => /^(AA|BB|CC|DD)$/.test(x)));
const saved = (page: Page) => page.evaluate(() => JSON.parse(localStorage.getItem('amux_chips') || '[]').map((c: any) => c.id));

async function dragOnto(page: Page, from: string, to: string, side: 'left' | 'right') {
  const a = (await page.locator('#peek-chips .chip[data-chip-idx]', { hasText: from }).boundingBox())!;
  const b = (await page.locator('#peek-chips .chip[data-chip-idx]', { hasText: to }).boundingBox())!;
  await page.mouse.move(a.x + a.width / 2, a.y + a.height / 2);
  await page.mouse.down();
  await page.mouse.move(a.x + a.width / 2 + 8, a.y + a.height / 2, { steps: 3 });
  const tx = side === 'right' ? b.x + b.width * 0.85 : b.x + b.width * 0.15;
  await page.mouse.move(tx, b.y + b.height / 2, { steps: 12 });
  await page.waitForTimeout(150);
  await page.mouse.up();
}

test('edit mode: a press does not send, a drag reorders what is shown and the server keeps it', async ({ page }) => {
  const srv: Server = { chips: JSON.stringify(START), offered: '["sendnow"]', failPosts: false, posts: [], sends: 0 };
  await boot(page, srv, { chips: START });
  expect(await shown(page)).toEqual(['AA', 'BB', 'CC', 'DD']);
  // Count, not stub: a press in edit mode must send nothing, so a stub would never match.
  page.on('request', (r) => { if (/\/api\/sessions\/chipedit-w\/(send|keys)/.test(r.url())) srv.sends++; });
  await page.locator('#peek-chips .chip-edit-toggle').click();
  await expect(page.locator('#peek-chips')).toHaveClass(/editing/);

  // A press while editing picks the button up; it sends nothing.
  // On its label, clear of the × that removes it.
  await page.locator('#peek-chips .chip[data-chip-idx]', { hasText: 'BB' }).click({ position: { x: 6, y: 10 } });
  await page.waitForTimeout(300);
  expect(srv.sends).toBe(0);

  // Drag AA onto the right half of CC: it lands after CC.
  await dragOnto(page, 'AA', 'CC', 'right');
  await expect.poll(() => shown(page)).toEqual(['BB', 'CC', 'AA', 'DD']);
  expect(await saved(page)).toEqual(['bb', 'cc', 'aa', 'dd']);
  await expect.poll(() => srv.chips && JSON.parse(srv.chips).map((c: any) => c.id), { timeout: 5000 }).toEqual(['bb', 'cc', 'aa', 'dd']);
  expect(await page.evaluate(() => localStorage.getItem('amux_chips_pending'))).toBeNull();

  // Keyboard: ArrowLeft moves the focused button one place, and it stays focused.
  await page.locator('#peek-chips .chip[data-chip-idx]', { hasText: 'DD' }).focus();
  await page.keyboard.press('ArrowLeft');
  await expect.poll(() => shown(page)).toEqual(['BB', 'CC', 'DD', 'AA']);
  expect(await page.evaluate(() => (document.activeElement as HTMLElement).innerText.replace(/[^A-Z]/g, ''))).toBe('DD');
  await expect.poll(() => srv.chips && JSON.parse(srv.chips).map((c: any) => c.id), { timeout: 5000 }).toEqual(['bb', 'cc', 'dd', 'aa']);

  // A reload shows the saved order (the server's copy wins at boot).
  await page.evaluate(() => localStorage.setItem('amux_chips', '[]'));
  await page.reload();
  await page.waitForFunction(() => (0, eval)('typeof _chipsSynced !== "undefined" && _chipsSynced') === true);
  expect(await saved(page)).toEqual(['bb', 'cc', 'dd', 'aa']);
});

test('a refresh of the bar while a button is held does not cancel the drag', async ({ page }) => {
  // CI, 2026-11-08: the bar was rebuilt between the press and the move and the
  // drag never started (beacon drag_not_started).
  const srv: Server = { chips: JSON.stringify(START), offered: '["sendnow"]', failPosts: false, posts: [], sends: 0 };
  await boot(page, srv, { chips: START });
  await page.locator('#peek-chips .chip-edit-toggle').click();
  const a = (await page.locator('#peek-chips .chip[data-chip-idx]', { hasText: 'DD' }).boundingBox())!;
  const b = (await page.locator('#peek-chips .chip[data-chip-idx]', { hasText: 'AA' }).boundingBox())!;
  await page.mouse.move(a.x + a.width / 2, a.y + a.height / 2);
  await page.mouse.down();
  await page.evaluate(() => (0, eval)('refreshAllChipBars()'));
  await page.mouse.move(a.x + a.width / 2 + 8, a.y + a.height / 2, { steps: 3 });
  await page.mouse.move(b.x + b.width * 0.15, b.y + b.height / 2, { steps: 12 });
  await page.waitForTimeout(150);
  await page.mouse.up();
  await expect.poll(() => shown(page)).toEqual(['DD', 'AA', 'BB', 'CC']);
  await expect.poll(() => saved(page)).toEqual(['dd', 'aa', 'bb', 'cc']);
});

test('a save the server refused stays pending, wins the next boot, and is sent again', async ({ page }) => {
  const srv: Server = { chips: JSON.stringify(START), offered: '["sendnow"]', failPosts: true, posts: [], sends: 0 };
  await boot(page, srv, { chips: START });
  await page.locator('#peek-chips .chip-edit-toggle').click();
  await dragOnto(page, 'DD', 'AA', 'left');
  await expect.poll(() => shown(page)).toEqual(['DD', 'AA', 'BB', 'CC']);
  await expect.poll(() => srv.posts.length, { timeout: 5000 }).toBeGreaterThan(0);
  expect(await page.evaluate(() => localStorage.getItem('amux_chips_pending'))).not.toBeNull();

  // The server still has the old order, and it keeps refusing through the
  // reload (the flush on leaving fails too). Booting, this device keeps its
  // order rather than taking the server's.
  await page.reload();
  await page.waitForFunction(() => (0, eval)('typeof _chipsSynced !== "undefined" && _chipsSynced') === true);
  expect(JSON.parse(srv.chips!).map((c: any) => c.id)).toEqual(['aa', 'bb', 'cc', 'dd']);
  expect(await saved(page)).toEqual(['dd', 'aa', 'bb', 'cc']);
  expect(await page.evaluate(() => localStorage.getItem('amux_chips_pending'))).not.toBeNull();
  // Back online, the next push lands and the pending mark clears.
  srv.failPosts = false;
  await page.evaluate(() => (0, eval)('_pushChipsNow()'));
  await expect.poll(() => srv.chips && JSON.parse(srv.chips).map((c: any) => c.id), { timeout: 5000 }).toEqual(['dd', 'aa', 'bb', 'cc']);
  await expect.poll(() => page.evaluate(() => localStorage.getItem('amux_chips_pending'))).toBeNull();
});

test('a later default removed on another device is not added back here', async ({ page }) => {
  // Send Now was offered and removed elsewhere: the server's list lacks it and
  // its offered list names it. This device never recorded the offer.
  const srv: Server = { chips: JSON.stringify(START), offered: '["sendnow"]', failPosts: false, posts: [], sends: 0 };
  await boot(page, srv);
  await page.waitForTimeout(600);
  expect(await saved(page)).toEqual(['aa', 'bb', 'cc', 'dd']);
  await expect(page.locator('#peek-chips .chip', { hasText: 'Send Now' })).toHaveCount(0);
  expect(srv.posts.filter((p) => p.key === 'ui_chips')).toHaveLength(0);
});
