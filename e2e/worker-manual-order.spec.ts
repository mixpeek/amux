import { test, expect, Page } from './fixtures';

// The Manual sort (owner, 2026-11-25): dragging exists only there. Under any
// other sort the grip is hidden but keeps its space, and the grab cursor is on
// the grip alone, never the whole card. Pins stay on top and reorder among
// themselves; a worker you have not placed joins at the bottom.
const base = [
  { name: 'pin-a', status: 'idle', pinned: true, last_activity: 1, session_created: 1 },
  { name: 'pin-b', status: 'idle', pinned: true, last_activity: 2, session_created: 2 },
  { name: 'alpha', status: 'active', last_activity: 500, session_created: 10 },
  { name: 'bravo', status: 'idle', last_activity: 400, session_created: 20 },
  { name: 'charlie', status: 'idle', last_activity: 300, session_created: 30 },
].map(w => ({ running: true, pinned: false, dir: '/tmp/', provider: 'codex', tags: [], ...w }));

async function prepare(page: Page, opts: { sort: string; order?: string[]; frozen?: boolean; workers?: any[]; tall?: boolean }) {
  const workers = opts.workers || base;
  // Drag tests: the project's width, tall enough that both cards are on screen,
  // set BEFORE load so no resize reflow moves a card between measure and press.
  if (opts.tall) await page.setViewportSize({ width: page.viewportSize()!.width, height: 1100 });
  await page.addInitScript(({ workers, opts }) => {
    if (sessionStorage.getItem('seeded')) return;   // a reload keeps what the page saved
    sessionStorage.setItem('seeded', '1');
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.setItem('amux_sessions_cache', JSON.stringify(workers));
    localStorage.setItem('amux_layout', 'list');
    localStorage.setItem('amux_sort_mode', opts.sort);
    if (opts.order) localStorage.setItem('amux_card_order', JSON.stringify(opts.order));
    else localStorage.removeItem('amux_card_order');
    if (opts.frozen) localStorage.setItem('amux_frozen', '1'); else localStorage.removeItem('amux_frozen');
  }, { workers, opts });
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: workers }));
  await page.goto('/');
  await expect(page.locator('#cards .card[data-session]')).toHaveCount(workers.length);
}

const names = (page: Page) => page.locator('#cards .card[data-session]')
  .evaluateAll(nodes => nodes.map(n => (n as HTMLElement).dataset.session));
const grip = (page: Page, name: string) => page.locator(`#cards .card[data-session="${name}"] .card-drag-handle`);

// Drag the way a hand does: move a little at a time, watch where the
// placeholder sits, and let go once it is past the target (or the path ends).
// The floating copy Sortable draws under the pointer is not part of the order.
const live = (page: Page) => page.locator('#cards .card[data-session]:not(.sortable-fallback):not(.sortable-drag)')
  .evaluateAll(nodes => nodes.map(n => (n as HTMLElement).dataset.session));
async function drag(page: Page, from: string, onto: string, during: { midway?: string } = {}) {
  const a = (await grip(page, from).boundingBox())!;
  const b = (await page.locator(`#cards .card[data-session="${onto}"]`).boundingBox())!;
  const x0 = a.x + a.width / 2, y0 = a.y + a.height / 2;
  const x1 = b.x + b.width * 0.85, y1 = b.y + b.height - 4;
  await page.mouse.move(x0, y0);
  await page.mouse.down();
  await page.mouse.move(x0, y0 + 8, { steps: 2 });
  if (during.midway) {
    // Once the drag is live: before that, the press window holds redraws.
    await page.mouse.move(x0, y0 + 16, { steps: 2 });
    await expect.poll(() => page.evaluate(() => !!(window as any).Sortable.dragged)).toBe(true);
    await page.evaluate(during.midway);
  }
  const down = y1 > y0;
  for (let t = 1; t <= 30; t++) {
    await page.mouse.move(x0 + (x1 - x0) * t / 30, y0 + (y1 - y0) * t / 30);
    await page.waitForTimeout(25);
    const order = await live(page);
    const past = down ? order.indexOf(from) > order.indexOf(onto) : order.indexOf(from) < order.indexOf(onto);
    if (past) break;
  }
  // Hold until the swap animation has finished and the placeholder is still,
  // as a hand does: a release mid-animation is not what this test is about.
  await expect.poll(() => page.evaluate(() =>
    [...document.querySelectorAll('#cards .card[data-session]')].every(c => !(c as any).animated)), { timeout: 3000 }).toBe(true);
  await page.waitForTimeout(150);
  await page.mouse.up();
}

test('under a computed sort the grip is hidden, keeps its space, and the card is not a grab target', async ({ page }) => {
  await prepare(page, { tall: true, sort: 'natural' });
  const g = grip(page, 'alpha');
  await expect(g).toHaveCSS('visibility', 'hidden');
  const box = await g.boundingBox();
  expect(box && box.width).toBeGreaterThan(10);   // its space is kept
  await expect(page.locator('#cards .card[data-session="alpha"]')).not.toHaveCSS('cursor', 'grab');
  const before = await names(page);
  await drag(page, 'charlie', 'alpha');
  expect(await names(page)).toEqual(before);
});

test('choosing Manual moves nothing, then a drag sticks across a re-render and a reload', async ({ page }, testInfo) => {
  await prepare(page, { tall: true, sort: 'natural' });
  const before = await names(page);
  await page.evaluate(() => (window as any).setSortMode('manual'));
  await expect(page.locator('#tile-sort-btn')).toHaveAttribute('title', 'Sort: Manual');
  expect(await names(page)).toEqual(before);
  await expect(grip(page, 'alpha')).toHaveCSS('visibility', 'visible');
  await grip(page, 'alpha').hover();
  await expect(grip(page, 'alpha')).toHaveCSS('cursor', 'grab');
  await expect(page.locator('#cards .card[data-session="alpha"]')).not.toHaveCSS('cursor', 'grab');
  await page.screenshot({ path: testInfo.outputPath('manual-order.png'), fullPage: true });

  await drag(page, 'alpha', 'charlie');
  const after = ['pin-b', 'pin-a', 'bravo', 'charlie', 'alpha'];
  await expect.poll(() => names(page)).toEqual(after);
  await page.evaluate(() => { eval('render()'); });
  await expect.poll(() => names(page)).toEqual(after);
  await page.reload();
  await expect.poll(() => names(page)).toEqual(after);
});

const arranged = ['pin-a', 'pin-b', 'alpha', 'bravo', 'charlie'];

// A live update (here a status change) can redraw the list while a card is
// held, and a slow arrange outlasts the 3s press window. The redraw waits for
// the drop; written at once, it replaced the held card and lost the move.
test('a live update while a card is held does not lose the drop', async ({ page }) => {
  await prepare(page, { tall: true, sort: 'manual', order: arranged });
  await drag(page, 'alpha', 'charlie', { midway:
    "sessions.find(s => s.name === 'bravo').status = 'active'; _cardsPressAt = performance.now() - 4000; render()" });
  await expect.poll(() => names(page)).toEqual(['pin-a', 'pin-b', 'bravo', 'charlie', 'alpha']);
});

test('a pinned card cannot be dropped among the unpinned ones', async ({ page }) => {
  await prepare(page, { tall: true, sort: 'manual', order: arranged });
  await drag(page, 'pin-a', 'charlie');
  const n = await names(page);
  expect(n.slice(0, 2).sort()).toEqual(['pin-a', 'pin-b']);   // at most the end of the pins
  expect(n.slice(2)).toEqual(['alpha', 'bravo', 'charlie']);
});

test('an unpinned card cannot be dropped among the pins', async ({ page }) => {
  await prepare(page, { tall: true, sort: 'manual', order: arranged });
  await drag(page, 'charlie', 'pin-a');
  expect((await names(page)).slice(0, 2)).toEqual(['pin-a', 'pin-b']);
});

test('pinned cards reorder among themselves', async ({ page }) => {
  await prepare(page, { tall: true, sort: 'manual', order: arranged });
  await drag(page, 'pin-a', 'pin-b');
  await expect.poll(() => names(page)).toEqual(['pin-b', 'pin-a', 'alpha', 'bravo', 'charlie']);
});

test('a new worker joins at the bottom and an arranged card keeps its spot', async ({ page }) => {
  const workers = [...base, { ...base[2], name: 'new-one', session_created: 99, last_activity: 9999 }];
  await prepare(page, { sort: 'manual', order: ['charlie', 'alpha', 'bravo'], workers });
  expect(await names(page)).toEqual(['pin-a', 'pin-b', 'charlie', 'alpha', 'bravo', 'new-one']);
});

test('a frozen order from before becomes the Manual order, and the freeze button is gone', async ({ page }) => {
  await prepare(page, { sort: 'natural', frozen: true, order: ['bravo', 'charlie', 'alpha'] });
  await expect(page.locator('#tile-sort-btn')).toHaveAttribute('title', 'Sort: Manual');
  expect(await names(page)).toEqual(['pin-a', 'pin-b', 'bravo', 'charlie', 'alpha']);
  await expect(page.locator('#tile-freeze-btn')).toHaveCount(0);
});

test('clicking a card body expands it under Manual, without starting a drag', async ({ page }) => {
  await prepare(page, { sort: 'manual', order: arranged });
  const card = page.locator('#cards .card[data-session="bravo"]');
  await card.scrollIntoViewIfNeeded();
  // The directory line: plain card body, not the title, grip, menu or chips.
  await card.locator('.card-dir').first().click();
  await expect(card).toHaveClass(/expanded/);
  expect(await names(page)).toEqual(['pin-a', 'pin-b', 'alpha', 'bravo', 'charlie']);
});
