// IRREFUTABLE: a Scratchpad save/edit has the SAME durability as sending a
// message to a worker (Ethan, 2026-10-17). The worker-send path's guarantee is
// the offline outbox (amux_offline_queue) + replay on reconnect. This proves a
// scratchpad edit rides the IDENTICAL primitive: offline, both the note PUT and
// the worker send queue in the one outbox, and both reach their durable store
// on reconnect. Plus: a new note offline is never lost, and a save is atomic
// (a concurrent read never sees a half-written file).
import { test, expect, Page } from './fixtures';
import { boot } from './lifecycle/evidence';
import { cleanup } from './teardown';

type Win = Window & { switchView: (v: string) => void; openPeek: (n: string) => void;
  _AMUX_AUTH_TOKEN: string; online: boolean };

const token = (p: Page) => p.evaluate(() => (window as unknown as Win)._AMUX_AUTH_TOKEN);
const outbox = (p: Page) => p.evaluate(
  () => JSON.parse(localStorage.getItem('amux_offline_queue') || '[]')
    .map((o: any) => `${o.options?.method || 'GET'} ${String(o.url).split('?')[0].replace(/^https?:\/\/[^/]+/, '')}`));

async function openScratchpad(p: Page) {
  await boot(p);
  await p.evaluate(() => (window as unknown as Win).switchView('scratchpad'));
  await expect(p.locator('#scratchpad-view')).toBeVisible();
}
// Golden's nudge: an in-flight success can flip the SPA wrongly-online; its own
// refetch failure path corrects it.
async function settleOffline(p: Page) {
  const ok = await p.waitForFunction("typeof online !== 'undefined' && online === false", null, { timeout: 5_000 })
    .then(() => true).catch(() => false);
  if (!ok) { await p.evaluate(() => (window as any).fetchSessions?.() ?? (window as any).fetchBoard?.());
    await p.waitForFunction("typeof online !== 'undefined' && online === false", null, { timeout: 10_000 }); }
}

test('a scratchpad edit and a worker send share the offline outbox and both replay', async ({ page, request }, info) => {
  test.setTimeout(150_000);
  await openScratchpad(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  const worker = `sdp-${info.project.name}`.toLowerCase().replace(/[^a-z0-9-]/g, '');
  const marker = `sdp-${info.project.name}-${Date.now()}`;
  const remove = async () => { await request.delete(`/api/sessions/${worker}`, { headers: auth }); };
  try {
    const r = await request.post('/api/sessions', { headers: auth, data: { name: worker, dir: '/tmp', start: false } });
    expect(r.status(), await r.text()).toBeLessThan(300);

    // A note that already exists on disk (online), so the offline step is an EDIT.
    await page.evaluate(() => (window as any)._scratchpadNewNote());
    await expect(page.locator('#file-edit-ta')).toBeVisible();
    await page.locator('#file-edit-ta').fill(`v1 ${marker}`);
    await page.locator('#file-save-btn').click();
    await expect(page.locator('#file-save-btn')).toHaveText(/Saved/);
    // Derive the note path by its unique content, robust to same-second name
    // stepping when workers create notes concurrently in the shared Scratchpad.
    const findByMarker = async (m: string) => {
      const ls = await (await request.get('/api/ls?path=' + encodeURIComponent('~/Vault/Scratchpad') + '&hidden=0', { headers: auth })).json();
      for (const e of (ls.entries || []).filter((x: any) => String(x.name).endsWith('.md'))) {
        const path = '~/Vault/Scratchpad/' + e.name;
        const f = await request.get('/api/file?path=' + encodeURIComponent(path), { headers: auth });
        if (f.ok() && String((await f.json()).content || '').includes(m)) return path;
      }
      return null;
    };
    await expect.poll(() => findByMarker(`v1 ${marker}`), { timeout: 15_000 }).not.toBeNull();
    const notePath = await page.evaluate(() => (0, eval)('_fileData').path as string);

    // ---- GO OFFLINE ----
    await page.context().setOffline(true);
    await settleOffline(page);

    // EDIT the note offline (PUT /api/file via apiCall → outbox).
    await page.locator('#file-edit-ta').fill(`v2 ${marker}`);
    await page.locator('#file-save-btn').click();
    await expect(page.locator('#file-save-btn')).toHaveText(/Queued|Saved/);
    // Close the editor (offline save already synced _fileData.content, so this
    // does not re-queue) to clear the overlay off the peek composer.
    await page.locator('#file-overlay .btn', { hasText: '\u2715' }).first().click();
    await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
    // SEND to the worker offline, through the peek composer (the worker-send path).
    await page.evaluate((w) => (window as unknown as Win).openPeek(w), worker);
    await expect(page.locator('#peek-cmd-input')).toBeVisible();
    await page.locator('#peek-cmd-input').fill(`ping ${marker}`);
    await page.locator('.send-split-main').click();

    // THE PARITY CLAIM: both the note PUT and the worker send are in the ONE
    // outbox. Wait for BOTH to register (the send lands a beat after its click).
    await expect.poll(() => outbox(page), { timeout: 15_000 }).toEqual(
      expect.arrayContaining(['PUT /api/file']));
    await expect.poll(() => outbox(page).then(o => o.some(x => /\/api\/sessions\/.+\/(send|steer)/.test(x))),
      { timeout: 15_000 }).toBe(true);
    const q = await outbox(page);
    expect(q.some(o => o === 'PUT /api/file'), `note edit queued; outbox=${JSON.stringify(q)}`).toBe(true);
    expect(q.some(o => /\/api\/sessions\/.+\/(send|steer)/.test(o)), `worker send queued in the SAME outbox; outbox=${JSON.stringify(q)}`).toBe(true);

    // Nothing reached the server yet: disk still v1.
    expect(await findByMarker(`v2 ${marker}`), 'nothing reached the server while offline').toBeNull();

    // ---- BACK ONLINE: the outbox replays ----
    await page.context().setOffline(false);
    await page.evaluate(() => window.dispatchEvent(new Event('online')));

    // The note edit landed on disk — same durability as the message.
    await expect.poll(() => findByMarker(`v2 ${marker}`), { timeout: 20_000 }).not.toBeNull();
    // The worker received the message (cmd_history), proving the send replayed too.
    await expect.poll(async () => {
      const h = await (await request.get(`/api/history?session=${worker}&limit=20`, { headers: auth }).catch(() => null))?.json?.().catch(() => null);
      const rows = (h && (h.history || h)) || [];
      return JSON.stringify(rows).includes(`ping ${marker}`);
    }, { timeout: 20_000 }).toBe(true);
    await expect.poll(() => outbox(page), { timeout: 15_000 }).toEqual([]);
    await page.screenshot({ path: info.outputPath('parity.png') });
  } finally {
    await cleanup('remove sdp worker', remove, info);
  }
});

test('a new note created offline is queued and lands on disk after reconnect', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  await openScratchpad(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  const marker = `sdp-new-${info.project.name}-${Date.now()}`;
  await page.context().setOffline(true);
  await settleOffline(page);
  await page.evaluate(() => (window as any)._scratchpadNewNote());
  await expect(page.locator('#file-edit-ta')).toBeVisible();
  await page.locator('#file-edit-ta').fill(`offline note ${marker}`);
  // Close the editor (which saves) — a new note offline must not vanish.
  await page.locator('#file-overlay .btn', { hasText: '✕' }).first().click();
  await expect.poll(() => outbox(page), { timeout: 10_000 }).toEqual(expect.arrayContaining(['PUT /api/file']));
  await page.context().setOffline(false);
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect.poll(async () => {
    const ls = await (await request.get('/api/ls?path=' + encodeURIComponent('~/Vault/Scratchpad') + '&hidden=0', { headers: auth })).json();
    for (const e of (ls.entries || []).filter((x: any) => String(x.name).endsWith('.md'))) {
      const f = await request.get('/api/file?path=' + encodeURIComponent('~/Vault/Scratchpad/' + e.name), { headers: auth });
      if (f.ok() && String((await f.json()).content || '').includes(marker)) return true;
    }
    return false;
  }, { timeout: 25_000 }).toBe(true);
});

test('a save is atomic: a concurrent read never sees a half-written file', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  test.skip(info.project.name !== 'desktop', 'server-side atomicity holds for all clients; proven once');
  await boot(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  const path = `~/Vault/Scratchpad/atomic-${Date.now()}.md`;
  const big = (tag: string) => `{"start":"` + tag.repeat(40000) + `","end":"${tag}"}`;
  await request.put('/api/file', { headers: auth, data: { path, content: big('A') } });
  let partial = 0, reads = 0;
  const writer = (async () => { for (let i = 0; i < 25; i++) await request.put('/api/file', { headers: auth, data: { path, content: big(i % 2 ? 'A' : 'B') } }); })();
  const reader = (async () => {
    for (let i = 0; i < 400; i++) {
      const r = await request.get('/api/file?path=' + encodeURIComponent(path), { headers: auth });
      if (!r.ok()) continue;
      const c = String((await r.json()).content || '');
      reads++;
      // Every read is a WHOLE prior write: starts with a run of one tag and ends
      // with the matching tag. A truncated/interleaved write would fail this.
      const m = c.match(/^\{"start":"(A+|B+)","end":"(A|B)"\}$/);
      if (!m || m[1][0] !== m[2] || m[1].length !== 40000) partial++;
    }
  })();
  await Promise.all([writer, reader]);
  await request.delete('/api/file?path=' + encodeURIComponent(path), { headers: auth }).catch(() => {});
  expect(reads, 'the reader actually ran').toBeGreaterThan(50);
  expect(partial, `every concurrent read saw a whole file (0 partial of ${reads})`).toBe(0);
});
