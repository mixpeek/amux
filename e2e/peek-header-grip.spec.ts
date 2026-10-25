// Ethan, 2026-10-22: "i want to be able to drag this up and down and it
// reorientates/shrinks/collapses/moves the contents as its expanded/
// contracted", and "make sure everything we do per tab/window/etc. is saved in
// the state". In a short window the worker page's header (name, ask, task,
// toolbar, tabs, folder) left the terminal one line. The grip under it folds the
// header in stages as it is dragged up, live, and the stage is remembered per
// worker with its tab and split.
import { test, expect, Page } from './fixtures';

const A = 'e2e-grip-a', B = 'e2e-grip-b';
const worker = (name: string) => ({ name, dir: '/tmp/', provider: 'claude', status: 'waiting', running: true, tags: ['e2e'],
  desc: 'grip', task_name: 'Testing Pillow 12 with HEIF compatibility', pinned: false, isolated: false });

async function boot(page: Page) {
  await page.setViewportSize({ width: 1440, height: 560 });
  await page.route('**/api/sessions', r => r.fulfill({ json: [worker(A), worker(B)] }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
}
async function open(page: Page, name: string) {
  await page.evaluate((n) => (window as any).openPeek(n), name);
  await expect(page.locator('#peek-overlay')).toHaveAttribute('data-session', name);
}
const level = (page: Page) => page.locator('#peek-overlay').getAttribute('data-head').then(v => Number(v || 0));
const room = (page: Page) => page.locator('#peek-split-wrap').evaluate(e => e.getBoundingClientRect().height);
async function drag(page: Page, dy: number) {
  const b = (await page.locator('#peek-head-grip').boundingBox())!;
  await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
  await page.mouse.down();
  await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2 + dy, { steps: 5 });
  await page.mouse.up();
}

test.describe('header grip', () => {
  test.skip(({ viewport }) => !!viewport && viewport.width <= 600, 'desktop drag; the phone keeps focus mode');

  test('dragging up folds the header stage by stage and gives the content the room', async ({ page }) => {
    await boot(page);
    await open(page, A);
    // A worker that is asking: the ask row is filled in, as the live one is.
    await page.evaluate(() => {
      const r = document.getElementById('peek-needs-input-row')!; r.hidden = false;
      r.innerHTML = '<div>NEEDS INPUT · MS-1608</div><div>May I dispatch zb-web-pipeline with resources=mxp-app and allow_build=true to ship Next.js 15.5.28 to mxp.co?</div>';
    });
    expect(await level(page)).toBe(0);
    const rooms = [await room(page)];
    for (let i = 1; i <= 3; i++) {
      await drag(page, -40);
      expect(await level(page)).toBe(i);
      rooms.push(await room(page));
    }
    // Every stage gives the content more room than the one before.
    for (let i = 1; i < rooms.length; i++) expect(rooms[i], JSON.stringify(rooms)).toBeGreaterThan(rooms[i - 1]);
    // Stage 3 is one slim bar that says which worker this is.
    await expect(page.locator('.peek-tabs')).toBeHidden();
    await expect(page.locator('#peek-focus-bar')).toContainText(A);
    // Dragging down brings it back; one long drag crosses several stages.
    await drag(page, 130);
    expect(await level(page)).toBe(0);
    await expect(page.locator('.peek-tabs')).toBeVisible();
  });

  test('stage 1 keeps the ask on one line and a hidden row hidden', async ({ page }) => {
    await boot(page);
    await open(page, A);
    await page.evaluate(() => { const r = document.getElementById('peek-needs-input-row')!; r.hidden = true; });
    await drag(page, -40);
    expect(await level(page)).toBe(1);
    await expect(page.locator('#peek-needs-input-row')).toBeHidden();
    await page.evaluate(() => {
      const r = document.getElementById('peek-needs-input-row')!; r.hidden = false;
      r.innerHTML = '<div>NEEDS INPUT · MS-1608</div><div>' + 'May I dispatch the pipeline build to ship it? '.repeat(6) + '</div>';
    });
    const h = await page.locator('#peek-needs-input-row').evaluate(e => e.getBoundingClientRect().height);
    expect(h, 'one line').toBeLessThan(40);
  });

  test('double-click and arrow keys fold and unfold', async ({ page }) => {
    await boot(page);
    await open(page, A);
    await page.locator('#peek-head-grip').dblclick();
    expect(await level(page)).toBeGreaterThan(0);
    await page.locator('#peek-head-grip').dblclick();
    expect(await level(page)).toBe(0);
    await page.locator('#peek-head-grip').focus();
    await page.keyboard.press('ArrowUp');
    await page.keyboard.press('ArrowUp');
    expect(await level(page)).toBe(2);
    await page.keyboard.press('ArrowDown');
    expect(await level(page)).toBe(1);
    // Expand on the slim bar goes back to full.
    await page.keyboard.press('ArrowUp'); await page.keyboard.press('ArrowUp');
    expect(await level(page)).toBe(3);
    await page.locator('#peek-focus-bar button', { hasText: 'Expand' }).click();
    expect(await level(page)).toBe(0);
  });

  test('each worker keeps its stage, across switching and a reload', async ({ page }) => {
    await boot(page);
    await open(page, A);
    await drag(page, -80);
    expect(await level(page)).toBe(2);
    await open(page, B);
    // B was never folded, but it opens at the last stage chosen until it has its own.
    await page.locator('#peek-head-grip').dblclick();   // B: unfold to full
    expect(await level(page)).toBe(0);
    await open(page, A);
    expect(await level(page)).toBe(2);
    await page.reload();
    await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
    await expect(page.locator('#peek-overlay')).toHaveAttribute('data-session', A, { timeout: 10_000 });
    expect(await level(page)).toBe(2);
    const views = await page.evaluate(() => JSON.parse(localStorage.getItem('amux_peek_views') || '{}'));
    expect(views[A]).toMatchObject({ head: 2 });
    expect(views[B]).toMatchObject({ head: 0 });
  });
});
