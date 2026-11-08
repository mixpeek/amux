// A renamed worker's old name follows to the worker it became.
//
// 2026-11-06: `workflows` was renamed to `wexus` while a phone (the AmuxApp
// WebView) had `workflows` open. Its two sends went to /api/sessions/workflows,
// the server answered "session not found", and the Messages tab said "Not sent,
// the worker did not accept it" with a Send again that retried the dead name.
// Here a worker is open with a send waiting in the outbox, the worker is renamed,
// and the open view, the waiting send and a fresh send to the old name all reach
// the renamed worker.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

test('a renamed worker: the open view, a queued send and the old name all follow', async ({ page, request }, info) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function'
    && typeof (window as any)._syncWorkerRenames === 'function');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const uiToken = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const stamp = `${info.project.name}-${Date.now()}`;
  const oldName = `rn-old-${stamp}`;
  const newName = `rn-new-${stamp}`;
  expect((await request.post('/api/sessions', { headers: auth, data: { name: oldName, dir: '/tmp', start: false } })).status()).toBe(201);
  const pending = async (name: string) => {
    const r = await request.get(`/api/sessions/${encodeURIComponent(name)}/steer`, { headers: auth });
    return r.ok() ? ((await r.json()) as any[]).map(row => row.text) : [];
  };
  try {
    await page.evaluate(name => (window as any).openPeek(name), oldName);
    await expect.poll(() => page.evaluate('peekSession')).toBe(oldName);

    // A send made while the app cannot reach the server waits in the outbox,
    // addressed to the name the app has open.
    const queuedText = `queued before the rename ${stamp}`;
    await page.evaluate('online = false');
    await page.evaluate(([name, text]) => (window as any).apiCall(`/api/sessions/${encodeURIComponent(name)}/steer`,
      { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ text, record_history: true }) }),
      [oldName, queuedText]);
    const queueUrls = () => page.evaluate(() => JSON.parse(localStorage.getItem('amux_offline_queue') || '[]').map((q: any) => q.url));
    await expect.poll(async () => (await queueUrls()).some((u: string) => u.includes(`/${oldName}/`))).toBe(true);

    // Renamed elsewhere (another device, a worker, the CLI).
    const renamed = await request.patch(`/api/sessions/${encodeURIComponent(oldName)}/config`,
      { headers: { ...auth, 'X-Amux-UI-Token': uiToken }, data: { rename: newName } });
    expect(renamed.status(), await renamed.text()).toBe(200);
    const map = await (await request.get('/api/session-renames', { headers: auth })).json();
    expect(map.renames[oldName]).toBe(newName);

    // The waiting send is rewritten to the new name once the app learns of the
    // rename (here from a request the open view makes to the old name).
    await page.evaluate(name => fetch(`/api/sessions/${encodeURIComponent(name)}/config`), oldName);
    await expect.poll(async () => (await queueUrls()).every((u: string) => !u.includes(`/${oldName}/`)), { timeout: 10_000 }).toBe(true);
    await expect.poll(() => page.evaluate('peekSession'), { timeout: 10_000 }).toBe(newName);
    await page.evaluate('online = true');
    await expect.poll(async () => {
      await page.evaluate(() => (window as any).runSyncBanner?.(true));
      return pending(newName);
    }, { timeout: 20_000 }).toContain(queuedText);

    // A relaunch (the phone app evicted and reopened) restores the renamed worker.
    await page.reload();
    await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
    await expect.poll(() => page.evaluate('peekSession'), { timeout: 20_000 }).toBe(newName);
    await expect.poll(() => page.evaluate(() => location.hash)).toContain(encodeURIComponent(newName));

    // Anything still addressing the old name lands on the renamed worker.
    const lateText = `sent to the old name ${stamp}`;
    const late = await request.post(`/api/sessions/${encodeURIComponent(oldName)}/steer`,
      { headers: auth, data: { text: lateText, record_history: true } });
    expect(late.ok(), await late.text()).toBe(true);
    expect(late.headers()['x-amux-renamed-to']).toBe(newName);
    expect(await pending(newName)).toContain(lateText);

    // Changing the worker by its old name is refused and names the new one.
    const stale = await request.patch(`/api/sessions/${encodeURIComponent(oldName)}/config`,
      { headers: { ...auth, 'X-Amux-UI-Token': uiToken }, data: { desc: 'stale' } });
    expect(stale.status()).toBe(404);
    expect((await stale.json()).renamed_to).toBe(newName);
    await page.screenshot({ path: info.outputPath('renamed-view.png'), fullPage: true });
  } finally {
    await cleanup('back online', () => page.evaluate('online = true'));
    for (const name of [newName, oldName]) {
      await cleanup(`delete ${name}`, () => request.post(`/api/sessions/${encodeURIComponent(name)}/delete`,
        { headers: { ...auth, 'X-Amux-UI-Token': uiToken }, data: {} }));
    }
  }
});
