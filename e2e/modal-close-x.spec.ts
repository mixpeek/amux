// Ethan, 2026-10-28: "go thru every popup and modal in amux and make sure they
// all have consistent X in the top right to close". Every dialog gets exactly
// one borderless ✕ (.modal-close) at the top right of its box, and pressing it
// closes the dialog (app.js _modalXEnsure). Each dialog below is opened the way
// the app opens it, then measured.
import { test, expect, Page } from './fixtures';

const DIALOGS: Array<{ name: string; open: string; root: string }> = [
  { name: 'confirm', open: "showConfirm('Delete this?')", root: '#modal-backdrop' },
  { name: 'prompt', open: "showPrompt('Name it')", root: '#modal-backdrop' },
  { name: 'themes', open: 'openThemes()', root: '#theme-dialog' },
  { name: 'style guide', open: 'openStyleGuide()', root: '#ui-guide-overlay' },
  { name: 'about', open: 'openAbout()', root: '#about-overlay' },
  { name: 'queue', open: 'showQueueModal()', root: '#queue-overlay' },
  { name: 'message history', open: 'openCmdHistoryModal()', root: '#cmd-history-modal' },
  { name: 'filters', open: 'openFiltersModal()', root: '#filters-modal' },
  { name: 'saved messages', open: '_openSavedMessages()', root: '#saved-messages-modal' },
  { name: 'new schedule', open: 'openSchedModal()', root: '#sched-overlay' },
  { name: 'new card', open: 'openBoardAdd()', root: '#board-edit-overlay' },
  { name: 'new worker', open: 'openCreate()', root: '#create-overlay' },
  { name: 'connection history', open: 'showConnHistory()', root: '.conn-hist-overlay' },
  { name: 'event', open: 'openEventModal()', root: '.modal-overlay.active' },
  { name: 'chip detail', open: "_chipDetailModal('x-chip-detail', 'Chip', 'detail', '<p>body</p>')", root: '#x-chip-detail' },
];

async function settle(page: Page) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._modalXEnsure === 'function' && typeof (window as any).openThemes === 'function');
}

for (const d of DIALOGS) {
  test(`${d.name}: one ✕ at the top right, and it closes the dialog`, async ({ page }) => {
    await settle(page);
    await page.evaluate((expr) => { try { (0, eval)(expr); } catch (e) { (window as any).__openErr = String(e); } }, d.open);
    const root = page.locator(d.root).last();
    await expect(root, 'the dialog opened').toBeVisible({ timeout: 10000 });
    // The X is added on the next frame after the dialog shows.
    await expect.poll(() => root.evaluate((el) => {
      const vis = (e: Element) => { const r = e.getBoundingClientRect(); const cs = getComputedStyle(e); return r.width > 0 && r.height > 0 && cs.visibility !== 'hidden' && cs.display !== 'none'; };
      return [...el.querySelectorAll('.modal-close')].filter(vis).length;
    })).toBe(1);
    // Sheets open from scale(0.95): measure once the opening animation is over.
    await root.evaluate((el) => Promise.all(el.getAnimations({ subtree: true })
      .filter((a) => a.effect?.getTiming().iterations !== Infinity).map((a) => a.finished.catch(() => {}))));
    const m = await root.evaluate((el) => {
      const r = el.getBoundingClientRect();
      const fills = r.width >= innerWidth - 2 && r.height >= innerHeight - 2;
      // The dialog: a child that says it is one, else the first visible child smaller than a viewport-filling backdrop.
      const named = [...el.children].find((c) => c.matches('[role="dialog"], .modal, dialog') && c.getBoundingClientRect().width > 0);
      const box = named || (fills ? ([...el.children].find((c) => { const b = c.getBoundingClientRect(); return b.width > 0 && (b.width < innerWidth - 2 || b.height < innerHeight - 2); }) || el) : el);
      const b = box.getBoundingClientRect();
      const x = [...el.querySelectorAll('.modal-close')].find((e) => e.getBoundingClientRect().width > 0)!;
      const xr = x.getBoundingClientRect();
      return { glyph: (x.textContent || '').trim(), label: x.getAttribute('aria-label') || '', w: xr.width, h: xr.height,
        fromRight: Math.round(b.right - xr.right), fromTop: Math.round(xr.top - b.top), cls: x.className };
    });
    expect(m.glyph, 'the same glyph everywhere').toBe('✕');
    expect(m.label, 'it has an accessible name').not.toBe('');
    expect(m.w >= 44 && m.h >= 44, `a 44px touch target (${m.w}x${m.h})`).toBe(true);
    expect(m.fromRight, 'at the right edge of the dialog').toBeLessThanOrEqual(40);
    expect(m.fromTop, 'at the top of the dialog').toBeLessThanOrEqual(56);
    expect(m.fromTop, 'inside the dialog').toBeGreaterThanOrEqual(-2);
    const shownBy = await root.evaluate((el) => ['active', 'open', 'show', 'visible'].filter((c) => el.classList.contains(c)));
    await root.locator('.modal-close').filter({ visible: true }).first().click();
    // Closed: gone, hidden, faded out, or no longer carrying the class that showed it.
    await expect.poll(() => page.evaluate(([sel, cls]) => {
      const el = [...document.querySelectorAll(sel as string)].pop();
      if (!el) return 'closed';
      const cs = getComputedStyle(el); const r = el.getBoundingClientRect();
      if (cs.display === 'none' || cs.visibility === 'hidden' || cs.opacity === '0' || !r.width || !r.height) return 'closed';
      if ((cls as string[]).length && !(cls as string[]).some((c) => el.classList.contains(c))) return 'closed';
      return 'open: ' + el.className;
    }, [d.root, shownBy] as const), { message: 'the X closes it', timeout: 5000 }).toBe('closed');
  });
}

// The X must close a dialog through its own logic, not just hide it: a hidden
// showAlert whose promise never resolves hangs every `await showAlert(...)`.
for (const kind of ['showAlert', 'showConfirm', 'showPrompt']) {
  test(`${kind}: the X settles the promise the caller awaits`, async ({ page }) => {
    await settle(page);
    await page.evaluate((k) => {
      (window as any).__settled = 'pending';
      (window as any)[k]('Saved').then((v: unknown) => { (window as any).__settled = 'settled:' + String(v); });
    }, kind);
    const x = page.locator('#modal-backdrop .modal-close').filter({ visible: true });
    await expect(x).toHaveCount(1, { timeout: 10000 });
    await x.click();
    await expect.poll(() => page.evaluate(() => (window as any).__settled)).toMatch(/^settled:/);
  });
}

// A dialog that focuses its own X on open and closes on Escape while focus is
// inside it (_chipDetailModal) must keep both after the X is normalised.
test('a dialog that focuses its X keeps focus inside, so Escape still closes it', async ({ page }) => {
  await settle(page);
  await page.evaluate(() => (window as any)._chipDetailModal('x-focus', 'Chip', 'detail', '<p>body</p>'));
  const root = page.locator('#x-focus');
  await expect(root.locator('.modal-close')).toHaveText('✕');
  await expect.poll(() => page.evaluate(() => !!document.activeElement?.closest('#x-focus'))).toBe(true);
  await expect(root.locator('header .modal-close'), 'the X stays in the header').toHaveCount(1);
  await page.keyboard.press('Escape');
  await expect(root).toHaveCount(0);
});

// When the X has to move (a header that is not a flex row leaves it mid-line,
// so it is pinned to the corner) and it had focus, focus goes with it.
test('a focused X that has to move keeps focus', async ({ page }) => {
  await settle(page);
  await page.evaluate(() => {
    const o = document.createElement('div');
    o.id = 'x-move'; o.className = 'modal-overlay active';
    o.innerHTML = '<section class="modal" role="dialog"><div class="x-head" style="display:block">'
      + '<button class="modal-close" aria-label="Close">×</button><h3>A title long enough to fill the line</h3></div><p>body</p></section>';
    document.body.appendChild(o);
    (o.querySelector('.modal-close') as HTMLElement).focus();
  });
  const x = page.locator('#x-move .modal-close');
  await expect(x).toHaveClass(/modal-close-corner/);
  await expect.poll(() => page.evaluate(() => document.activeElement?.matches('#x-move .modal-close') ?? false)).toBe(true);
});
