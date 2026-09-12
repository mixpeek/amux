import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHash } from 'node:crypto';
import { test, expect } from './offline-fixtures';
import { boot, auth, checkpoint } from './evidence';

// Unlike route-fault tests, this exercises the real service worker, cached
// shell, browser network switch, API writes and durable server queue together.
test.use({ serviceWorkers: 'allow' });
test('LC-OFFLINE-ROUNDTRIP: cold offline reload preserves edits and messages; reconnect checks every real acknowledgement', async ({ page, request, context, offlineTransport }, info) => {
  test.setTimeout(150_000);
  page.setDefaultTimeout(15_000);
  await page.addInitScript(() => {
    (window as any).__syncTransitions = [];
    addEventListener('DOMContentLoaded', () => {
      let previous = '';
      new MutationObserver(() => {
        const rows = Array.from(document.querySelectorAll('#sync-items .sync-item')).map(el => ({
          key:el.getAttribute('data-sync-id'), label: (el.textContent || '').slice(2).trim(), status: el.className,
        }));
        const key = JSON.stringify(rows);
        if (rows.length && key !== previous) {
          previous = key;
          (window as any).__syncTransitions.push({ rows, at: Date.now() });
        }
      }).observe(document.documentElement, { subtree: true, childList: true, attributes: true });
    });
  });
  await boot(page);
  await expect.poll(() => page.evaluate(() => Boolean(navigator.serviceWorker.controller)), {
    timeout: 20_000, message: 'real service worker must control the shell before testing an offline reload',
  }).toBe(true);
  const headers = await auth(page);
  const uiToken = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN);
  const name = `lc-offline-${info.project.name}-${Date.now()}`;
  const cards: any[] = [];
  const directory = await mkdtemp(join(tmpdir(), 'lc-offline-files-'));
  const files = [
    {name:'large-offline-output.bin', mimeType:'application/octet-stream', buffer:Buffer.alloc(32 * 1024 * 1024 + 17, 0x5a)},
    {name:'offline-évidence.txt', mimeType:'text/plain', buffer:Buffer.from('Evidence retained through a cold offline reload.\n')},
  ];
  // Distinct chunk contents make swapped/duplicated chunks fail the hash check.
  for (let i = 0; i < files[0].buffer.length; i++) files[0].buffer[i] = i % 251;
  const sourceDirectory = await mkdtemp(join(tmpdir(), 'lc-offline-source-'));
  for (const file of files) await writeFile(join(sourceDirectory, file.name), file.buffer);
  const digest = (bytes:Buffer) => createHash('sha256').update(bytes).digest('hex');
  const uploadQueue = () => page.evaluate(() => (window as any)._upqList());
  const priorDirectory = await (await request.get('/api/prefs?key=files_cwd', {headers})).json();
  expect((await request.post('/api/prefs', {headers, data:{key:'files_cwd', value:directory}})).ok()).toBe(true);
  const messages = [`Offline owner assignment ${name}`, `Offline follow-up ${name}`];
  const queued = () => page.evaluate(() => JSON.parse(localStorage.getItem('amux_offline_queue') || '[]'));
  try {
    expect((await request.post('/api/sessions', { headers, data: { name, dir: '/tmp' } })).status()).toBe(201);
    for (let i = 0; i < 3; i++) {
      const made = await request.post('/api/board', { headers,
        data: { title: `Offline original ${i} ${name}`, type: 'chore', session: name } });
      expect(made.status()).toBe(201); cards.push(await made.json());
    }
    await page.reload();
    await expect(page.locator(`.card[data-session="${name}"]`).locator('visible=true').first()).toBeVisible();
    // Warm every detail cache before losing the network.
    for (const card of cards) {
      await page.goto(`/#issue=${card.id}`);
      await expect(page.locator('#bd-key')).toHaveText(card.id);
      await page.waitForFunction(id => (window as any).eval('_bdHydrated && _bdLoadedIdentity?.id') === id, card.id, {timeout:10_000});
      await page.locator('#board-detail-overlay.active > .overlay-header').getByRole('button', { name: /Back/ }).click();
    }
    await page.locator('#tab-files').click();
    await expect(page.locator('#files-body')).toBeVisible();
    offlineTransport.setConnected(false);
    await context.setOffline(true);
    await expect.poll(() => page.evaluate(() => navigator.onLine)).toBe(false);
    for (const card of cards) {
      await page.goto(`/#issue=${card.id}`);
      await expect(page.locator('#bd-key')).toHaveText(card.id);
      await page.locator('#bd-tab-edit').click();
      await page.locator('#bd-title').fill(`Offline edited ${card.id}`);
      await page.locator('#bd-edit-footer button[onclick="boardDetailSave()"]').click();
      await expect(page.locator('#bd-save-status')).not.toHaveText('Saved');
      await page.locator('#board-detail-overlay.active > .overlay-header').getByRole('button', { name: /Back/ }).click();
    }
    await page.locator('#tab-files').click();
    // WebKit's network emulation also disables File.text/arrayBuffer, even on
    // about:blank. Keep the real upstream disconnected while allowing local I/O.
    if (info.project.name === 'ios-safari') await context.setOffline(false);
    const chooser = page.waitForEvent('filechooser');
    const uploadButton = page.getByTitle('Upload files into this folder', {exact:true});
    if (await uploadButton.isVisible()) await uploadButton.click();
    else {
      await page.locator('#files-overflow-btn').click();
      await page.locator('#files-overflow-menu').getByRole('button', {name:/Upload files/}).click();
    }
    await (await chooser).setFiles(files.map(file => join(sourceDirectory, file.name)));
    await expect.poll(async () => (await uploadQueue()).length, {timeout:30_000}).toBe(2);
    await context.setOffline(true);
    const dismiss = page.locator('#sync-banner.active').getByRole('button', {name:'Dismiss', exact:true});
    if (await dismiss.isVisible()) await dismiss.click();
    const savedUploads = (await uploadQueue()).map((f:any) => ({id:f.id, name:f.name, size:f.size, totalChunks:f.totalChunks}));
    expect(savedUploads.find((f:any) => f.name === files[0].name).totalChunks).toBeGreaterThan(1);
    await page.goto('/#view=sessions');
    const worker = page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
    await worker.locator('.card-menu-btn').click();
    await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
    const send = page.locator('#peek-overlay .send-split-main');
    if ((await send.innerText()).trim() !== 'Queue') await page.locator('#peek-overlay .send-split-arrow').click();
    await expect(send).toHaveText('Queue');
    for (const message of messages) {
      await page.locator('#peek-cmd-input').fill(message);
      await send.click();
      await expect(page.locator('#peek-cmd-input')).toHaveValue('');
    }
    await expect.poll(async () => (await queued()).length).toBe(5);
    const durable = await queued();
    expect(new Set(durable.map((q: any) => q.id)).size).toBe(5);
    const messageIds = durable.filter((q: any) => q.url.endsWith('/steer')).map((q: any) => JSON.parse(q.options.body).msg_id);
    expect(messageIds).toHaveLength(2);
    expect(messageIds.every(Boolean)).toBe(true);
    await checkpoint(page, info, 'offline-seven-operations-retained');
    // WebKit's emulator also rejects cached document loads. The disconnected
    // proxy proves network loss while its local cache remains readable.
    if (info.project.name === 'ios-safari') await context.setOffline(false);
    await page.reload();
    // Reapply the emulator flag to the new document; the upstream socket has
    // remained disconnected throughout, including the cached shell load.
    await context.setOffline(true);
    await page.waitForFunction(() => typeof (window as any).runSyncBanner === 'function');
    // navigator.onLine can reset across a Chromium navigation. Prove the
    // transport is still disconnected instead of trusting the indicator.
    expect(await page.evaluate(async () => {
      try { await (window as any)._origFetch('/health?offline-proof', {signal:AbortSignal.timeout(5000)}); return false; }
      catch { return true; }
    })).toBe(true);
    expect((await queued()).map((q: any) => ({ id: q.id, body: q.options.body }))).toEqual(
      durable.map((q: any) => ({ id: q.id, body: q.options.body })));
    expect((await uploadQueue()).map((f:any) => ({id:f.id, name:f.name, size:f.size, totalChunks:f.totalChunks}))).toEqual(savedUploads);
    await checkpoint(page, info, 'cold-offline-reload');
    // Reconnect through the browser's actual online event; do not call the
    // replay function or click Retry to rescue an automatic pickup failure.
    offlineTransport.setConnected(true);
    await context.setOffline(false);
    await expect.poll(async () => (await queued()).length, { timeout: 30_000 }).toBe(0);
    await expect.poll(async () => (await uploadQueue()).length, {timeout:60_000}).toBe(0);
    const transitions = await page.evaluate(() => (window as any).__syncTransitions);
    await info.attach('per-operation-sync-transitions', { body: JSON.stringify(transitions), contentType: 'application/json' });
    const expectedKeys = [...durable.map((q:any) => 'queue:' + q.id), ...savedUploads.map((f:any) => 'upload:' + f.id)];
    for (const key of expectedKeys) {
      expect(transitions.some((s:any) => s.rows.some((r:any) => r.key === key && r.status.includes('running'))), key + ' ran').toBe(true);
      expect(transitions.some((s:any) => s.rows.some((r:any) => r.key === key && r.status.includes('done'))), key + ' acknowledged').toBe(true);
    }
    for (const file of files) expect(digest(await readFile(join(directory, file.name)))).toBe(digest(file.buffer));
    await info.attach('offline-file-byte-proof', {body:JSON.stringify(files.map(file => ({name:file.name, bytes:file.buffer.length, sha256:digest(file.buffer)}))), contentType:'application/json'});
    for (const card of cards) {
      expect(transitions.some((s: any) => s.rows.some((r: any) => r.label.includes(card.id) && r.status.includes('running')))).toBe(true);
      expect(transitions.some((s: any) => s.rows.some((r: any) => r.label.includes(card.id) && r.status.includes('done')))).toBe(true);
      const stored = await request.get(`/api/board/${card.id}`, { headers });
      expect(stored.ok()).toBe(true);
      expect((await stored.json()).title).toBe(`Offline edited ${card.id}`);
    }
    await expect(page.locator('#sync-title-text')).toHaveText('7 synced');
    await expect(page.locator('#sync-items .done')).toHaveCount(7);
    await expect(page.locator('#sync-items .done').filter({ hasText: '✔' })).toHaveCount(7);
    const serverQueue = await request.get(`/api/sessions/${name}/steer`, { headers });
    expect(serverQueue.ok()).toBe(true);
    const rows = await serverQueue.json();
    expect(rows).toHaveLength(2);
    expect(rows.map((r: any) => r.text)).toEqual(durable.filter((q:any) => q.url.endsWith('/steer')).map((q:any) => JSON.parse(q.options.body).text));
    expect(rows.every((r: any) => !r.delivered_at)).toBe(true); // stopped worker: synced does not mean consumed
    await expect(page.locator('#toast')).not.toHaveClass(/visible/);
    await expect(page.locator('#sync-banner')).toHaveClass(/active/);
    await checkpoint(page, info, 'all-seven-acknowledgements');
    // A second online event/reload must not resurrect the settled outbox.
    await page.reload();
    await page.waitForFunction(() => typeof (window as any).runSyncBanner === 'function');
    expect(await queued()).toEqual([]);
    const afterReload = await (await request.get(`/api/sessions/${name}/steer`, { headers })).json();
    const stableRows = (items:any[]) => items.map(({age_s, ...row}) => row);
    expect(stableRows(afterReload)).toEqual(stableRows(rows));
  } catch (error) {
    if (!page.isClosed()) {
      await info.attach('failure-state', {body:JSON.stringify(await page.evaluate(() => ({
        online:navigator.onLine, outbox:JSON.parse(localStorage.getItem('amux_offline_queue') || '[]'),
        sync:document.getElementById('sync-banner')?.textContent,
      })).catch(() => ({}))), contentType:'application/json'});
      await page.screenshot({path:info.outputPath('before-cleanup.png')}).catch(() => {});
    }
    throw error;
  } finally {
    offlineTransport.setConnected(true);
    await context.setOffline(false).catch(() => {});
    if (!page.isClosed()) {
      await page.evaluate(async ({ ids, name }) => (window as any)._mutateQueue((q: any[]) => {
        for (let i = q.length - 1; i >= 0; i--) if (ids.some(id => q[i].url === '/api/board/' + id) || q[i].url.includes('/' + name + '/')) q.splice(i, 1);
      }), { ids: cards.map(c => c.id), name }).catch(() => {});
      await page.evaluate(async dir => {for (const row of await (window as any)._upqList()) if (row.dir === dir) await (window as any)._upqRemove(row.id);}, directory).catch(() => {});
    }
    const removed = await request.post('/api/sessions/' + name + '/delete', {headers:{...headers, 'X-Amux-UI-Token':uiToken}});
    expect(removed.ok(), 'run-owned worker cleanup').toBe(true);
    await request.post('/api/prefs', {headers, data:{key:'files_cwd', value:priorDirectory.value || ''}});
    await rm(directory, {recursive:true, force:true});
    await rm(sourceDirectory, {recursive:true, force:true});
    for (const card of cards) await request.delete(`/api/board/${card.id}`, { headers });
  }
});
