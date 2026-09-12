import { test, expect } from '@playwright/test';
import { mkdir, readFile } from 'node:fs/promises';
import path from 'node:path';
import { auth, boot, checkpoint, getSessionsResilient } from './evidence';
import { lifecyclePrefix, selectLifecycleProvider, createLifecycleWorker,
  expectLifecycleTerminal, expectLifecycleWorker } from './provider';

// Real provider execution is opt-in and always uses a new raw worker. No fake
// receipt, manual Enter, or operator-written output may stand in for pickup.
test('LC-ISOLATED-NATIVE: isolated worker consumes an owner queued file and produces real output', async ({ page, request }, info) => {
  test.setTimeout(900_000);
  expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
  expect(process.env.AMUX_LIFECYCLE_LAB_WORKSPACE).toBeTruthy();
  const health = await (await request.get('/health')).json();
  await info.attach('isolated-prerequisites', { body: JSON.stringify(health), contentType: 'application/json' });
  expect(health.admission, 'worker admission denied; native isolated pickup remains unverified').not.toBe('deny');
  const name = `${lifecyclePrefix}isolated-${Date.now()}`;
  const dir = path.join(process.env.AMUX_LIFECYCLE_LAB_WORKSPACE!, name);
  await mkdir(dir, { recursive: true });
  await boot(page);
  const headers = await auth(page);
  let created = false;
  try {
    await page.locator('#tab-sessions').click();
    await page.locator('[onclick*="toggleAddMenu"]').click();
    await page.locator('.card-menu-item', { hasText: 'New worker' }).click();
    await page.locator('#create-name').fill(name);
    await page.locator('#create-dir').fill(dir);
    await selectLifecycleProvider(page);
    await page.locator('#create-prompt').fill('');
    await page.locator('#create-isolated').check();
    await expect(page.locator('#create-isolated-info')).toBeVisible();
    await checkpoint(page, info, 'isolated-create');
    created = true;
    await createLifecycleWorker(page);
    const roster = await getSessionsResilient(request, headers);
    expect(roster.ok()).toBe(true);
    const worker = (await roster.json()).find((r: any) => r.name === name);
    expectLifecycleWorker(worker);
    expect(worker.isolated).toBe(true);
    await page.locator(`.card[data-session="${name}"]`).locator('visible=true').first().locator('.card-menu-btn').click();
    await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
    await expectLifecycleTerminal(page);
    await page.locator('#peek-composer-more-btn').click();
    const chooser = page.waitForEvent('filechooser');
    await page.locator('#peek-more-menu').getByRole('button', { name: 'Attach file', exact: false }).click();
    await (await chooser).setFiles({ name: 'isolated-input.json', mimeType: 'application/json',
      buffer: Buffer.from('{"values":[7,11,13]}') });
    await expect(page.locator('#peek-attach-bar')).toContainText('isolated-input.json');
    await expect(page.locator('#peek-attach-bar .uploading')).toHaveCount(0);
    await expect(page.locator('#peek-attach-bar .failed')).toHaveCount(0);
    const main = page.locator('#peek-overlay .send-split-main');
    if ((await main.innerText()).trim() !== 'Queue') await page.locator('#peek-overlay .send-split-arrow').click();
    await expect(main).toHaveText('Queue');
    const message = `Authorized isolated lifecycle assignment ${name}. Work only in ${dir}. Read the exact uploaded JSON file attached here and sum its values with a real command. Write isolated-receipt.json with worker set to "${name}", total, input_path, and harness_env containing only the values (or null when absent) of AMUX_SESSION, AMUX_WORKER, and AMUX_URL from the command environment. Do not inspect other environment variables. Print isolated-receipt.json and finish without requesting another acknowledgement or messaging peers.`;
    await page.locator('#peek-cmd-input').fill(message);
    const response = page.waitForResponse(r => r.url().endsWith(`/${name}/steer`) && r.request().method() === 'POST');
    await main.click();
    const queued = await response;
    expect(queued.ok()).toBe(true);
    const accepted = await queued.json();
    expect(typeof accepted.id).toBe('string');
    await expect(page.locator('#peek-cmd-input')).toHaveValue('');
    await page.locator('#peek-tab-steering').click();
    await checkpoint(page, info, 'isolated-owner-queued');
    let history: any[] = [];
    await expect.poll(async () => {
      const response = await request.get(`/api/sessions/${name}/steer?history=1`, { headers });
      expect(response.ok()).toBe(true);
      history = await response.json();
      return history.some(r => r.id === accepted.id && r.delivered_at > 0 && ['confirmed', 'retried'].includes(r.submit_verdict));
    }, { timeout: 240_000, intervals: [1000, 5000] }).toBe(true);
    expect(history.filter(r => r.id === accepted.id)).toHaveLength(1);
    let receipt: any;
    await expect.poll(async () => {
      try { receipt = JSON.parse(await readFile(path.join(dir, 'isolated-receipt.json'), 'utf8')); return receipt.total; }
      catch { return null; }
    }, { timeout: 360_000, intervals: [5000] }).toBe(31);
    expect(receipt.worker).toBe(name);
    expect(receipt.input_path).toContain('isolated-input');
    expect(receipt.harness_env).toEqual({ AMUX_SESSION: null, AMUX_WORKER: null, AMUX_URL: null });
    await page.locator('#peek-tab-terminal').click();
    await expect(page.locator('#peek-body')).toContainText('isolated-receipt.json');
    await checkpoint(page, info, 'isolated-native-output');
    expect((await (await request.get('/health')).json()).build).toBe(health.build);
    await info.attach('isolated-native-proof', { body: JSON.stringify({ name, accepted, history, receipt }), contentType: 'application/json' });
  } finally {
    // Preserve evidence; stop only the fresh run-owned worker.
    if (created) expect((await request.post(`/api/sessions/${name}/stop`, { headers })).ok()).toBe(true);
  }
});
