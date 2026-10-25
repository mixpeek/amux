import { test, expect } from './fixtures';

/**
 * Ethan, 2026-10-11: "messages list should also show WHO sent it not just that
 * its human there should be filters for who".
 *
 * Against the REAL server (no route stubs): two messages from two different
 * senders land in cmd_history, the server derives `sender` from each row's
 * stamped origin (history.rs sender_sql), the row shows "sender -> worker", and
 * the From menu, built from the server's senders facet, narrows the list
 * server-side.
 */
test('each message row names its sender and the From filter narrows the list', async ({ page }) => {
  const nonce = Date.now().toString(36);
  const target = 'e2e-sender-target';
  const lane = 'e2e-sender-lane-' + nonce;
  const ownerText = 'owner message ' + nonce;
  const laneText = 'lane message ' + nonce;
  await page.goto('/');
  // Seeded through the page's own authenticated fetch (the bare request
  // context has no session and gets 401). Owner: a human row with no member
  // or gateway identity. Lane: a session row stamped with its origin.
  const seeded = await page.evaluate(async (bodies) => {
    const out: number[] = [];
    for (const b of bodies) {
      const r = await fetch('/api/history', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(b) });
      out.push(r.status);
    }
    return out;
  }, [
    { text: ownerText, type: 'user', session: target },
    { text: laneText, type: 'session', session: target, origin: lane },
  ]);
  expect(seeded.every(s => s >= 200 && s < 300), `seed statuses ${seeded}`).toBe(true);
  await page.waitForFunction(() => typeof (window as any).switchView === 'function', { timeout: 20000 });
  await page.evaluate(() => (window as any).switchView('messages'));
  await page.click('#cv-all-item');
  await page.waitForSelector('#msgs-kind-filter .msg-kind-chip', { timeout: 15000 });
  // All kinds, so both the human and the lane message are in the list.
  await page.locator('#msgs-kind-filter .msg-kind-chip', { hasText: /^All / }).click();

  const ownerRow = page.locator('#msgs-list [data-msg-key]', { hasText: ownerText });
  const laneRow = page.locator('#msgs-list [data-msg-key]', { hasText: laneText });
  await expect(ownerRow).toHaveCount(1, { timeout: 10000 });
  await expect(laneRow).toHaveCount(1);
  await expect(ownerRow.locator('.msg-sender')).toHaveText('owner');
  await expect(laneRow.locator('.msg-sender')).toHaveText(lane);
  // "sender -> worker": the arrow and the target follow the sender.
  await expect(ownerRow.locator('.msg-sender-arrow')).toHaveCount(1);
  await expect(laneRow).toContainText(target);

  // The From menu lists both senders, from the server's facet.
  const from = page.locator('#msgs-sender-filter');
  await expect(from.locator('option', { hasText: lane })).toHaveCount(1);
  await expect(from.locator('option', { hasText: /^owner \(/ })).toHaveCount(1);

  // Picking the lane narrows the list to its message, at the SERVER.
  const fetched = page.waitForRequest(r => r.url().includes('/api/history?') && r.url().includes('sender=' + encodeURIComponent(lane)));
  await from.selectOption(lane);
  await fetched;
  await expect(laneRow).toHaveCount(1);
  await expect(ownerRow).toHaveCount(0);

  // And the filter composes with the kind chips: lane under Human is empty.
  await page.locator('#msgs-kind-filter .msg-kind-chip', { hasText: /^Human / }).click();
  await expect(laneRow).toHaveCount(0);
  await expect(page.locator('#msgs-list')).toContainText('from ' + lane);

  // Clearing it brings the owner's message back.
  await page.locator('#msgs-kind-filter .msg-kind-chip', { hasText: /^All / }).click();
  await from.selectOption('');
  await expect(ownerRow).toHaveCount(1);
  await expect(laneRow).toHaveCount(1);
});

// At 375px the controls above the list are one row that scrolls sideways
// (Ethan, 2026-10-21, e2e/messages-mobile-one-row.spec.ts), so the From filter
// may start off screen; it must still be readable, tappable and reachable.
test('at 375px the From filter is readable, tappable and reachable beside the worker filter', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 740 });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function', { timeout: 20000 });
  await page.evaluate(() => (window as any).switchView('messages'));
  await page.click('#cv-all-item');
  const from = page.locator('#msgs-sender-filter');
  await expect(from).toBeVisible();
  await from.scrollIntoViewIfNeeded();
  const box = await from.boundingBox();
  expect(box!.height).toBeGreaterThanOrEqual(44);
  expect(box!.x).toBeGreaterThanOrEqual(0);
  expect(box!.x + box!.width).toBeLessThanOrEqual(375);
  // Wide enough to read its label, and on the same row as the worker filter.
  expect(box!.width).toBeGreaterThanOrEqual(140);
  const worker = await page.locator('#msgs-session-filter').boundingBox();
  expect(Math.abs(worker!.y - box!.y)).toBeLessThan(4);
});
