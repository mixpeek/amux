// Edit a queued message before it goes out (Ethan, 2026-10-25: "make it so i
// can edit steering/queued messages"). A real server and a real queued row on
// a stopped fixture worker (so nothing is delivered): the Steering tab's edit
// dialog saves, the row and the server's queue both carry the new text, and it
// survives a re-render. A message still on this device is edited in place.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

test('a queued message is edited from the Steering tab, on the server and in the list', async ({ page, request }, info) => {
  // The first worker listing on a cold test server can take tens of seconds on a loaded host.
  test.setTimeout(150_000);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any)._steeringEdit === 'function');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const uiToken = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const name = `steer-edit-${info.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp', start: false } })).status()).toBe(201);
  try {
    const q = await request.post(`/api/sessions/${name}/steer`, { headers: auth, data: { text: 'Ship the report tonight' } });
    expect(q.status()).toBe(200);
    const id = (await q.json()).id as string;

    await page.evaluate(() => { (window as any).fetchSessions(); });
    await page.waitForFunction((n) => (globalThis as any).eval('sessions').some((s: any) => s.name === n && (s.steering || []).length), name, { timeout: 90_000, polling: 2000 });
    await page.evaluate((n) => { const g = window as any; g.openPeek(n); g._stopPeekPoll?.(); g.setPeekTab('steering'); }, name);
    const list = page.locator('#peek-steering-list');
    await expect(list).toContainText('Ship the report tonight');

    // Prime the 2s sessions cache with the pre-edit queue, so the read after
    // the edit proves the edit invalidated it rather than that it had expired.
    const queued = async () => ((await (await request.get('/api/sessions', { headers: auth })).json()) as any[])
      .find(s => s.name === name)?.steering?.find((m: any) => m.id === id)?.text;
    expect(await queued()).toBe('Ship the report tonight');
    await list.getByRole('button', { name: 'Edit message' }).click();
    const box = page.locator('#steer-edit-input');
    await expect(box).toHaveValue('Ship the report tonight');
    await page.screenshot({ path: info.outputPath('edit-dialog.png') });
    await box.fill('Ship the report tomorrow morning\nand cc the team');
    await box.press(process.platform === 'darwin' ? 'Meta+Enter' : 'Control+Enter');

    await expect(list).toContainText('Ship the report tomorrow morning');
    await expect(list).not.toContainText('tonight');
    const stored = await (await request.get(`/api/sessions/${name}/steer`, { headers: auth })).json();
    expect(stored.find((m: any) => m.id === id)?.text, 'the server queue holds the edit').toBe('Ship the report tomorrow morning\nand cc the team');
    expect(await queued(), 'the sessions list shows the edit at once, not the cached old text').toBe('Ship the report tomorrow morning\nand cc the team');

    // A poll re-renders from the server; the edit is what comes back.
    await page.evaluate(() => { (window as any).fetchSessions(); });
    await page.waitForTimeout(1500);
    await page.evaluate(() => (window as any)._steeringRender());
    await expect(list).toContainText('and cc the team');
    await page.screenshot({ path: info.outputPath('edited-row.png') });

    // Cancel leaves it alone; an empty edit is refused, not saved.
    await list.getByRole('button', { name: 'Edit message' }).click();
    await page.locator('#steer-edit-input').fill('   ');
    await page.locator('#modal-btns .btn.primary').click();
    await expect(page.locator('.toast, #toast').last()).toContainText('Empty');
    const again = await (await request.get(`/api/sessions/${name}/steer`, { headers: auth })).json();
    expect(again.find((m: any) => m.id === id)?.text).toBe('Ship the report tomorrow morning\nand cc the team');

    // Gone from the queue (delivered or removed elsewhere): the edit says so.
    await request.delete(`/api/sessions/${name}/steer`, { headers: auth, data: { id } });
    const gone = await request.patch(`/api/sessions/${name}/steer`, { headers: auth, data: { id, text: 'too late' } });
    expect(gone.status()).toBe(404);
  } finally {
    await cleanup('delete fixture worker', () => request.post(`/api/sessions/${encodeURIComponent(name)}/delete`,
      { headers: { ...auth, 'X-Amux-UI-Token': uiToken }, data: {} }));
  }
});

test('a message still on this device is edited in its saved request', async ({ page }) => {
  const W = 'zz-steer-edit-local';
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [
    { name: W, dir: '/tmp', running: true, status: 'active', flags: '', steering: [] }] }));
  await page.route(`**/api/sessions/${W}/peek?*`, r => r.fulfill({ json: { name: W, output: 'working\n', history: '' } }));
  await page.goto('/');
  await page.waitForFunction((w) => typeof (window as any)._mutateQueue === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === w), W);
  // Hold delivery: the outbox changes only when the test changes it.
  await page.evaluate(() => { (window as any).eval('online = false; _syncFlight = new Promise(() => {}); runSyncBanner = async () => {}; setOnline = () => {}'); });
  await page.evaluate(async (w) => {
    const g = window as any;
    g.openPeek(w); g._stopPeekPoll?.(); g.setPeekTab('steering');
    await g._mutateQueue((rows: any[]) => rows.push({ id: 'local-1', url: '/api/sessions/' + w + '/steer',
      options: { method: 'POST', body: JSON.stringify({ text: 'draft one', msg_id: 'm-local-1' }) },
      timestamp: Date.now(), state: 'pending', not_attempted: true }));
  }, W);
  const list = page.locator('#peek-steering-list');
  await expect(list).toContainText('draft one');
  await list.getByRole('button', { name: 'Edit message' }).click();
  await page.locator('#steer-edit-input').fill('draft two');
  await page.locator('#modal-btns .btn.primary').click();
  await expect(list).toContainText('draft two');
  const body = await page.evaluate(() => JSON.parse((globalThis as any).eval('offlineQueue')[0].options.body));
  expect(body).toMatchObject({ text: 'draft two', msg_id: 'm-local-1' });
});
