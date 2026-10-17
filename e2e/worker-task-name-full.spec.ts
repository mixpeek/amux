// A worker's full task name is visible on its card and details page, and its
// task-name changes are recorded and listed (Ethan, 2026-10-16: "make sure i
// can see the full task name of a worker on the card and details page"; "make
// sure we're storing the list of task changes per worker").
import { test, expect } from './fixtures';
import { boot } from './lifecycle/evidence';
import { cleanup } from './teardown';

test('full task name on the card and details page, and its changes are recorded', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  await boot(page);
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)}` };
  const name = `tnf-${info.project.name}`.toLowerCase().replace(/[^a-z0-9-]/g, '');
  const first = 'Rebuild the deploy gate so a launcher that cannot measure CI installs its own tested fix, then verify it in production end to end';
  const second = 'Ship the Maps stopwatch for run time trials with laps, GPS distance, pace and saved trials';
  const removeWorker = async () => { await request.delete(`/api/sessions/${name}`, { headers: auth }); };
  try {
    const r = await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp', desc: first, start: false } });
    expect(r.status(), await r.text()).toBeLessThan(300);
    await page.reload(); await boot(page);
    const line = page.locator(`.card[data-session="${name}"] .card-task-full`).first();
    await expect(line).toContainText(first);
    const clip = await line.evaluate(e => ({ sw: e.scrollWidth, cw: e.clientWidth, ws: getComputedStyle(e).whiteSpace }));
    expect(clip.ws, 'the task line wraps').toBe('normal');
    expect(clip.sw, 'nothing is cut off sideways').toBeLessThanOrEqual(clip.cw + 1);

    const history = async () => (await (await request.get(`/api/sessions/${name}/task-history`, { headers: auth })).json()).changes || [];
    await expect.poll(async () => (await history()).map((c: any) => c.to), { timeout: 20_000 }).toContain(first);
    const p = await request.patch(`/api/sessions/${name}/config`, { headers: auth, data: { desc: second } });
    expect(p.status(), await p.text()).toBeLessThan(300);
    await expect.poll(async () => (await history())[0]?.to, { timeout: 20_000 }).toBe(second);
    expect((await history())[0].from, 'a change records what it replaced').toBe(first);

    // The page's worker list refreshes on its own poll; ask for it now.
    await expect.poll(async () => { await page.evaluate(() => (0, eval)('_fetchSessionsOnce')()); return page.evaluate((n) => ((0, eval)('sessions') as any[]).find(x => x.name === n)?.task_name, name); }, { timeout: 20_000 }).toBe(second);
    await page.evaluate((n) => (window as any).openPeek(n), name);
    const label = page.locator('#peek-task-label');
    await expect(label).toContainText(second);
    await expect(label, 'the task line is on screen, phones included').toBeVisible();
    expect(await label.evaluate(e => getComputedStyle(e).whiteSpace)).toBe('normal');
    // History opens the standard dialog with the list (Ethan: "it should open a modal which is a list").
    await page.locator('#peek-task-history-btn').click();
    const dlg = page.locator('#task-history-modal [role="dialog"]');
    await expect(dlg).toBeVisible();
    await expect(dlg.locator('li')).toHaveCount(2, { timeout: 15_000 });
    await expect(dlg.locator('li').first()).toContainText(second);
    const box = await dlg.boundingBox();
    expect(box!.x + box!.width, 'the dialog fits the screen').toBeLessThanOrEqual(page.viewportSize()!.width + 1);
    await page.screenshot({ path: info.outputPath('task-history-modal.png') });
    await page.keyboard.press('Escape');
    await expect(page.locator('#task-history-modal')).toHaveCount(0);
    await page.screenshot({ path: info.outputPath('task-name-full.png') });
  } finally {
    await cleanup('remove task-name worker', removeWorker, info);
  }
});
