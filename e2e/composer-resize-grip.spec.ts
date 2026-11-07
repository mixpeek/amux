// The peek's message box can be made bigger by dragging its corner grip
// (Ethan, 2026-10-20: "drag this input box by a corner and manually make it
// bigger"). It grows as you type; a dragged size, up or down, holds for that
// draft (Ethan, 2026-11-07: "expand vertically as i type but i can also
// collapse it if i drag"). An emptied box or a double-click sizes by content.
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';

async function openWorker(page: Page, request: any, name: string) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any).fetchSessions === 'function');
  await expect.poll(() => page.evaluate(async (n) => { await (window as any).fetchSessions(); return (0, eval)('sessions').some((s: any) => s.name === n); }, name), { timeout: 90_000 }).toBe(true);
  await page.evaluate((n) => (window as any).openPeek(n), name);
  await expect(page.locator('#peek-overlay')).toHaveClass(/active/);
  await expect(page.locator('#peek-cmd-input')).toBeVisible();
  // Wait for the opening transition: the grip must have stopped moving.
  let prev = '';
  await expect.poll(async () => { const b = JSON.stringify(await page.locator('#peek-composer-grip').boundingBox()); const same = b === prev; prev = b; return same; }, { intervals: [150] }).toBe(true);
}
const h = (page: Page) => page.locator('#peek-cmd-input').evaluate(el => el.getBoundingClientRect().height);

test('the message box grows as you type, a drag sizes it for the draft, an empty box grows again', async ({ page, request }, info) => {
  test.setTimeout(150_000);
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
  const ui = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const name = `grip-${info.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp', start: false } })).status()).toBe(201);
  const box = page.locator('#peek-cmd-input');
  const drag = async (dy: number) => {
    const g = (await page.locator('#peek-composer-grip').boundingBox())!;
    const x = g.x + g.width / 2, y = g.y + g.height / 2;
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.mouse.move(x, y + dy / 2, { steps: 5 });
    await page.mouse.move(x, y + dy, { steps: 5 });
    await page.mouse.up();
  };
  const lines = (n: number) => Array.from({ length: n }, (_, i) => 'line ' + i).join('\n');
  try {
    await openWorker(page, request, name);
    const before = await h(page);
    await expect(page.locator('#peek-composer-grip')).toBeVisible();

    // Typing grows the box.
    await box.fill(lines(6));
    await expect.poll(() => h(page)).toBeGreaterThan(before + 60);
    await box.fill('');
    await expect.poll(() => h(page)).toBeLessThan(before + 10);

    // Dragged up, the box stays tall while the draft is short.
    await drag(-150);
    await expect.poll(() => h(page), { message: `before ${before}` }).toBeGreaterThan(before + 120);
    await box.pressSequentially('one line');
    expect(await h(page)).toBeGreaterThan(before + 120);

    // A box full of text drags DOWN and stays collapsed as you keep typing
    // (2026-11-06: "drag this down even if the text is full"); the text scrolls.
    await box.fill(lines(80));
    const full = await h(page);
    await drag(120);
    await expect.poll(() => h(page), { message: `full ${full}` }).toBeLessThan(full - 100);
    const collapsed = await h(page);
    await box.press('End');
    await box.pressSequentially(' more');
    expect(Math.abs((await h(page)) - collapsed)).toBeLessThan(2);
    expect(await box.evaluate(el => el.scrollHeight > el.clientHeight + 20), 'the text scrolls inside the smaller box').toBe(true);

    // Emptying the box (a send clears it the same way) sizes it by content again,
    // so the next draft grows as you type.
    await box.fill('');
    await expect.poll(() => h(page)).toBeLessThan(before + 10);
    await box.fill(lines(6));
    await expect.poll(() => h(page)).toBeGreaterThan(before + 60);

    // A double-click on the grip resets a hand-set size too, and no size is kept on the device.
    await drag(-150);
    await expect.poll(() => h(page)).toBeGreaterThan(before + 120);
    await page.locator('#peek-composer-grip').dblclick();
    await box.fill('');
    await expect.poll(() => h(page)).toBeLessThan(before + 10);
    expect(await page.evaluate(() => localStorage.getItem('amux_peek_composer_h'))).toBeNull();
  } finally {
    await cleanup('delete fixture worker', () => request.post(`/api/sessions/${encodeURIComponent(name)}/delete`,
      { headers: { ...auth, 'X-Amux-UI-Token': ui }, data: {} }));
  }
});
