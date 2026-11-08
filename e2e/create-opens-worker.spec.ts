// Creating a worker opens it (Ethan, 2026-10-31: "i just tried to make a
// worker nothing happened"). The create had succeeded and the worker was
// running, but the dialog only closed and scrolled to the new card when the
// card was on screen, so from a worker view, a filtered list or another group
// nothing visibly happened. The create is answered here and the worker is a
// fake in the session list: nothing is launched.
import { test, expect } from './fixtures';

declare function openCreate(): void;
declare function submitCreate(): Promise<void>;

const NAME = 'zz-create-opens';

test('a created worker opens, even when you were looking at another worker', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  let created = false;
  const other = { name: 'zz-create-other', dir: '/tmp/zz-other', running: true, status: 'idle', flags: '', steering: [] };
  await page.route(/\/api\/sessions(?:\?.*)?$/, async (route) => {
    if (route.request().method() === 'POST') {
      created = true;
      return route.fulfill({ status: 201, json: { ok: true, name: NAME } });
    }
    return route.fulfill({ json: created
      ? [other, { name: NAME, dir: '/tmp/zz-create', running: true, status: 'idle', flags: '', steering: [] }]
      : [other] });
  });
  await page.route(/\/api\/sessions\/zz-create-[a-z]+\/(peek|config)(?:\?.*)?$/, (route) =>
    route.fulfill({ json: route.request().url().includes('/peek') ? { output: 'idle\n', history: '' } : { ok: true } }));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).submitCreate === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === 'zz-create-other'));
  // Inside another worker's view, where the new card is not on screen.
  await page.evaluate(() => (window as any).openPeek('zz-create-other'));
  await page.evaluate(async (n) => {
    openCreate();
    (document.getElementById('create-name') as HTMLInputElement).value = n;
    (document.getElementById('create-dir') as HTMLInputElement).value = '/tmp/zz-create';
    await submitCreate();
  }, NAME);
  expect(created, 'the create reached the server').toBe(true);
  await expect.poll(() => page.evaluate(() => (globalThis as any).eval('peekSession'))).toBe(NAME);
  await expect(page.locator('#peek-overlay')).toHaveClass(/active/);
  expect(errors, errors.join('\n')).toEqual([]);
});
