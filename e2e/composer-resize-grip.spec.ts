// The peek's message box can be made bigger by dragging its corner grip
// (Ethan, 2026-10-20: "drag this input box by a corner and manually make it
// bigger"). The size sticks across reloads; a double-click resets it.
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

test('drag the corner grip up to make the message box taller; it sticks, double-click resets', async ({ page, request }, info) => {
  test.setTimeout(150_000);
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
  const ui = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const name = `grip-${info.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp', start: false } })).status()).toBe(201);
  try {
    await openWorker(page, request, name);
    const before = await h(page);
    const grip = page.locator('#peek-composer-grip');
    await expect(grip).toBeVisible();
    const b = (await grip.boundingBox())!;
    const x = b.x + b.width / 2, y = b.y + b.height / 2;
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.mouse.move(x, y - 75, { steps: 5 });
    await page.mouse.move(x, y - 150, { steps: 5 });
    await page.mouse.up();
    await expect.poll(() => h(page), { message: `before ${before}` }).toBeGreaterThan(before + 120);
    // Typing does not shrink a hand-sized box.
    await page.locator('#peek-cmd-input').fill('one line');
    expect(await h(page)).toBeGreaterThan(before + 120);
    await page.locator('#peek-cmd-input').fill('');

    // Kept across a reload.
    await openWorker(page, request, name);
    await expect.poll(() => h(page)).toBeGreaterThan(before + 120);

    // Double-click resets to sizing by content.
    await page.locator('#peek-composer-grip').dblclick();
    await expect.poll(() => h(page)).toBeLessThan(before + 10);
    expect(await page.evaluate(() => localStorage.getItem('amux_peek_composer_h'))).toBeNull();
  } finally {
    await cleanup('delete fixture worker', () => request.post(`/api/sessions/${encodeURIComponent(name)}/delete`,
      { headers: { ...auth, 'X-Amux-UI-Token': ui }, data: {} }));
  }
});
