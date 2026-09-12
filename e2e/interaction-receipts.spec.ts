import { test, expect, Page } from './fixtures';

async function boot(page: Page): Promise<void> {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).__amuxState?.interactions?.recent === 'function');
}

test('mutation fetch creates an inspectable interaction receipt and linked effect', async ({ page }) => {
  await boot(page);
  let interactionHeader = '';
  let commandHeader = '';
  await page.route('**/api/prefs', async route => {
    if (route.request().method() !== 'POST') return route.continue();
    const headers = route.request().headers();
    interactionHeader = headers['x-amux-interaction-id'] || '';
    commandHeader = headers['x-amux-command-kind'] || '';
    return route.fulfill({
      json: { ok: true, applied: true, rev: 123, entity_id: 'pref:receipt-smoke', version: 1 },
    });
  });

  const id = await page.evaluate(async () => {
    const response = await fetch('/api/prefs', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ key: 'receipt-smoke', value: '1' }),
    });
    await response.json();
    return (window as any).__amuxState.interactions.recent(1)[0].id;
  });

  expect(interactionHeader).toBe(id);
  expect(commandHeader).toBe('environment.post');
  await expect.poll(async () => page.evaluate((receiptId) => {
    const receipt = (window as any).__amuxState.interactions.get(receiptId);
    return {
      phase: receipt?.phase,
      command: receipt?.command?.kind,
      target: receipt?.command?.target?.primitive,
      status: receipt?.acknowledgement?.status,
      rev: receipt?.acknowledgement?.rev,
      effects: receipt?.effects?.length || 0,
      effectKind: receipt?.effects?.[0]?.kind || '',
    };
  }, id)).toEqual({
    phase: 'applied',
    command: 'environment.post',
    target: 'environment',
    status: 200,
    rev: 123,
    effects: 1,
    effectKind: 'environment.post.acknowledged',
  });
});

test('offline mutation receipt resolves to queued, not applied', async ({ page }) => {
  await boot(page);
  const result = await page.evaluate(async () => {
    (window as any).setOnline(false);
    const response = await fetch('/api/prefs', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ key: 'receipt-offline', value: '1' }),
    });
    const receipt = (window as any).__amuxState.interactions.recent(1)[0];
    return {
      responseStatus: response.status,
      outbox: response.headers.get('X-Amux-Outbox'),
      phase: receipt.phase,
      command: receipt.command.kind,
      queued: receipt.acknowledgement.queued === true,
      severity: receipt.feedback.severity,
    };
  });

  expect(result).toEqual({
    responseStatus: 202,
    outbox: 'queued',
    phase: 'queued',
    command: 'environment.post',
    queued: true,
    severity: 'info',
  });
});
