// Delay a queued message by N hours (Ethan, 2026-10-15: "test when I sent a
// queued message and delay it by x hours"). The dialog shows the clock time it
// will go, the row says "Delivers <time>" as soon as it is set and after a
// re-render, and the tag clears the delay. The server half (the drain holds
// the row until then) is in session_verbs.rs tests; the live journey is in the
// card's evidence. No real worker is messaged.
import { test, expect } from './fixtures';

const W = 'delay-lane';

test('the delay dialog shows when it goes, and the row says so at once and after a re-render', async ({ page }, info) => {
  const row = { id: 'steer-1', text: 'Ship the report', queued_at: Date.now() / 1000 - 30, guard: '',
    system: false, transport_id: 't-1', blocked_reason: null, deliver_at: null as number | null };
  const patches: any[] = [];
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: W, dir: '/tmp/delay', running: true, status: 'active', flags: '', steering: [row] }] }));
  await page.route(`**/api/sessions/${W}/peek?*`, r => r.fulfill({ json: { name: W, output: 'working\n', history: '' } }));
  await page.route(`**/api/sessions/${W}/steer*`, async r => {
    if (r.request().method() !== 'PATCH') return r.fulfill({ json: [] });
    const body = r.request().postDataJSON();
    patches.push(body);
    row.deliver_at = body.delay_s > 0 ? Date.now() / 1000 + body.delay_s : null;
    return r.fulfill({ json: { ok: true, id: body.id, deliver_at: row.deliver_at } });
  });
  await page.goto('/');
  await page.waitForFunction((w) => typeof (window as any).openPeek === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === w), W);
  await page.evaluate((w) => { (window as any).openPeek(w); (window as any)._stopPeekPoll?.(); (window as any).setPeekTab('steering'); }, W);

  const list = page.locator('#peek-steering-list');
  await list.getByRole('button', { name: 'Delay delivery' }).click();
  const dlg = page.getByRole('dialog', { name: 'Delay delivery' });
  await expect(dlg).toBeVisible();
  await dlg.locator('#steer-dp-val').fill('3');
  await dlg.locator('#steer-dp-unit').selectOption('3600');
  const expected = await page.evaluate(() => (window as any)._steerDeliverLabel(Date.now() / 1000 + 3 * 3600));
  await expect(dlg.locator('#steer-dp-when')).toContainText(expected);
  await page.screenshot({ path: info.outputPath('delay-dialog.png') });
  await dlg.getByRole('button', { name: 'Set delay' }).click();

  expect(patches).toHaveLength(1);
  expect(patches[0]).toMatchObject({ id: 'steer-1', delay_s: 10800 });
  const tag = list.locator('.steer-delay-tag');
  await expect(tag, 'the row says when it goes as soon as the delay is set').toContainText(expected);
  await expect(list.getByRole('button', { name: 'Delay delivery' })).toHaveCount(0);
  if (info.project.name === 'mobile') {
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - innerWidth);
    expect(overflow).toBeLessThanOrEqual(0);
    expect((await tag.boundingBox())!.height, 'the tag is also the clear button').toBeGreaterThanOrEqual(44);
  }
  await page.screenshot({ path: info.outputPath('delayed-row.png') });

  // A session poll re-renders from the server row; the tag must survive it.
  await page.evaluate(() => (window as any).fetchSessions());
  await page.waitForTimeout(300);
  await page.evaluate(() => (window as any)._steeringRender());
  await expect(tag).toContainText(expected);

  await tag.click();
  expect(patches.at(-1)).toMatchObject({ id: 'steer-1', delay_s: 0 });
  await expect(tag).toHaveCount(0);
  await expect(list.getByRole('button', { name: 'Delay delivery' })).toBeVisible();
});
