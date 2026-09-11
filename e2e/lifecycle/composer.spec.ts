import { test, expect, allowUnusedRoute } from '../fixtures';
import { boot, auth, checkpoint, deleteOwnedWorkers } from './evidence';

for (const outcome of ['refused', 'accepted', 'queued', 'unconfirmed'] as const) {
  test(`LC-COMPOSER: ${outcome} delivery clears only after local persistence, before server response`, async ({ page, request }, info) => {
    test.setTimeout(60_000);
    await boot(page);
    const headers = await auth(page);
    const name = `lc-compose-${info.project.name}-${Date.now()}`;
    expect((await request.post('/api/sessions', { headers, data: { name, dir: '/tmp' } })).status()).toBe(201);
    let release!: () => void;
    const pending = new Promise<void>(resolve => { release = resolve; });
    const payloads: any[] = [];
    await page.route(`**/api/sessions/${name}/send`, async route => {
      payloads.push(route.request().postDataJSON());
      await pending;
      await route.fulfill({ status: outcome === 'refused' ? 422 : outcome === 'queued' ? 503 : 200,
        json: ['accepted', 'unconfirmed'].includes(outcome) ? { ok: true, submitted: outcome === 'accepted' } : { error: 'lifecycle controlled refusal' } });
    });
    const entries = () => page.evaluate(name => JSON.parse(localStorage.getItem('amux_offline_queue') || '[]').filter((q: any) => q.url.endsWith(`/${name}/send`)), name);
    try {
      await page.reload();
      const card = page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
      await card.locator('.card-menu-btn').click();
      await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
      await page.locator('#peek-composer-more-btn').click();
      const chooser = page.waitForEvent('filechooser');
      await page.locator('#peek-more-menu').getByRole('button', { name: 'Attach file', exact: false }).click();
      await (await chooser).setFiles({ name: 'retained-draft.txt', mimeType: 'text/plain', buffer: Buffer.from('Keep this upload with its queued message.') });
      await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(1);
      await expect(page.locator('#peek-attach-bar .uploading')).toHaveCount(0);
      await expect(page.locator('#peek-attach-bar .failed')).toHaveCount(0);
      const input = page.locator('#peek-cmd-input');
      const send = page.locator('#peek-overlay .send-split-main');
      const message = `Preserve this ${outcome} message ${name}`;
      await input.fill(message);
      await send.click();
      await expect(input, 'durable local acceptance clears before the server answers').toHaveValue('');
      await expect(send).toBeEnabled();
      await expect(send).toHaveText('Send');
      await expect(page.locator('#sync-banner')).not.toHaveClass(/active/);
      await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(0);
      const saved = await entries();
      expect(saved).toHaveLength(1);
      const intent = JSON.parse(saved[0].options.body);
      expect(intent.text).toContain(message);
      expect(intent.text).toContain('@');
      expect(intent.msg_id).toBeTruthy();
      await expect.poll(() => payloads.length).toBe(1);
      expect(payloads[0]).toEqual(intent);
      await input.fill('A newer draft must survive the old delivery receipt');
      await checkpoint(page, info, 'locally-queued-server-still-pending');
      release();
      if (outcome === 'accepted') await expect.poll(async () => (await entries()).length).toBe(0);
      else {
        await expect.poll(async () => (await entries())[0]?.attempts).toBe(1);
        expect((await entries())[0].state).toBe(outcome === 'queued' ? 'pending' : 'blocked');
      }
      await expect(input).toHaveValue('A newer draft must survive the old delivery receipt');
      await page.reload();
      const persisted = await entries();
      if (outcome !== 'accepted') {
        expect(persisted).toHaveLength(1);
        expect(JSON.parse(persisted[0].options.body)).toEqual(intent);
      }
    } finally {
      release();
      await page.evaluate(name => eval('_mutateQueue')((queue: any[]) => { for (let i = queue.length - 1; i >= 0; i--) if (queue[i].url.endsWith(`/${name}/send`)) queue.splice(i, 1); }), name);
      await deleteOwnedWorkers(page, request, headers, [name]);
    }
  });
}

test('LC-COMPOSER: focused and replacement inputs clear exactly the accepted draft', async ({ page }, info) => {
  await boot(page);
  const result = await page.evaluate(() => {
    const name = 'lc-focus-regression';
    const input = document.createElement('textarea');
    input.id = 'input-' + name; document.body.appendChild(input);
    input.value = 'accepted message'; input.focus();
    eval('_draftSave')(name, input.value);
    eval('_composerAcceptLocal')(name, input.value);
    const focusedCleared = input.value === '';
    const replacement = input.cloneNode() as HTMLTextAreaElement;
    replacement.value = 'replacement accepted'; input.replaceWith(replacement); replacement.focus();
    eval('_draftSave')(name, replacement.value);
    eval('_composerAcceptLocal')(name, 'replacement accepted');
    const replacementCleared = replacement.value === '';
    replacement.value = 'newer edit'; eval('_draftSave')(name, 'older accepted');
    eval('_composerAcceptLocal')(name, 'older accepted');
    const newerPreserved = replacement.value === 'newer edit' && eval('_draftGet')(name) === 'newer edit';
    replacement.remove(); eval('_draftClear')(name);
    return { focusedCleared, replacementCleared, newerPreserved };
  });
  expect(result).toEqual({ focusedCleared:true, replacementCleared:true, newerPreserved:true });
});

test('LC-COMPOSER: failed local persistence retains draft and sends nothing', async ({ page, request }) => {
  await boot(page);
  const headers = await auth(page);
  const name = `lc-storage-${Date.now()}`;
  expect((await request.post('/api/sessions', {headers, data:{name, dir:'/tmp'}})).status()).toBe(201);
  let sends = 0;
  // Zero requests is the required outcome when durable local storage fails.
  allowUnusedRoute(page, `**/api/sessions/${name}/send`);
  await page.route(`**/api/sessions/${name}/send`, route => { sends++; return route.fulfill({json:{ok:true, submitted:true}}); });
  try {
    await page.reload();
    await page.locator(`.card[data-session="${name}"]`).locator('visible=true').first().locator('.card-menu-btn').click();
    await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
    await page.locator('#peek-cmd-input').fill('Storage failure must keep this draft');
    await page.evaluate(() => {
      const original = Storage.prototype.setItem;
      (window as any).__restoreStorage = () => { Storage.prototype.setItem = original; };
      Storage.prototype.setItem = function(key, value) {
        if (key === 'amux_offline_queue') throw new DOMException('Lifecycle controlled quota', 'QuotaExceededError');
        return original.call(this, key, value);
      };
    });
    await page.locator('#peek-overlay .send-split-main').click();
    await expect(page.locator('#peek-cmd-input')).toHaveValue('Storage failure must keep this draft');
    await expect(page.locator('#peek-overlay .send-split-main')).toBeEnabled();
    expect(sends).toBe(0);
  } finally {
    await page.evaluate(() => (window as any).__restoreStorage?.());
    await deleteOwnedWorkers(page, request, headers, [name]);
  }
});
