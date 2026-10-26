// Global Messages: pick several workers and groups at once, with search
// (Ethan, 2026-10-25: "i want to have multiselect/search for workers + groups
// here in global messages tab"). Against the real server: three workers, two
// in one group, a message each. Picking the group shows its two workers'
// messages; adding the third worker shows all three; search narrows the
// picker; "All workers" clears it; the button always says what is applied.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

test('the Messages worker filter picks several workers and groups, searchable', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  const n = Date.now().toString(36);
  const group = 'e2e-scope-' + n;
  const [w1, w2, w3] = ['a', 'b', 'c'].map(x => `e2e-scope-${x}-${n}`);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function' && typeof (window as any)._msgsScopeOpen === 'function', { timeout: 20000 });
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const uiToken = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  for (const [w, tags] of [[w1, [group]], [w2, [group]], [w3, []]] as const) {
    expect((await request.post('/api/sessions', { headers: auth, data: { name: w, dir: '/tmp', start: false, tags } })).status()).toBe(201);
  }
  try {
    const seeded = await page.evaluate(async (ws) => {
      const out: number[] = [];
      for (const w of ws) {
        const r = await fetch('/api/history', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ text: 'scope note for ' + w, type: 'user', session: w }) });
        out.push(r.status);
      }
      return out;
    }, [w1, w2, w3]);
    expect(seeded.every(s => s >= 200 && s < 300)).toBe(true);

    await page.evaluate(() => (window as any).switchView('messages'));
    await page.click('#cv-all-item');
    await page.waitForSelector('#msgs-kind-filter .msg-kind-chip', { timeout: 15000 });
    await page.locator('#msgs-kind-filter .msg-kind-chip', { hasText: /^All / }).click();
    const btn = page.locator('#msgs-session-filter');
    await expect(btn).toHaveText('All workers');
    const list = page.locator('#msgs-list');
    const shows = async (w: string) => (await list.textContent() || '').includes('scope note for ' + w);

    // The group: its two workers, not the third.
    await btn.click();
    const pop = page.locator('#msgs-scope-pop');
    await expect(pop).toBeVisible();
    await pop.locator('#msgs-scope-q').fill(group);
    await expect(pop.locator(`.vault-check[data-kind="groups"][data-name="${group}"]`)).toBeVisible();
    await expect(pop.locator(`.vault-check[data-kind="workers"][data-name="${w3}"]`)).toBeHidden();
    await page.screenshot({ path: info.outputPath('picker-search.png') });
    await pop.locator('#msgs-scope-q').press('Enter');
    await expect(btn).toHaveText('group: ' + group);
    await expect.poll(async () => [await shows(w1), await shows(w2), await shows(w3)], { timeout: 15000 }).toEqual([true, true, false]);

    // Plus the third worker: all three, and the button names both picks.
    await pop.locator('#msgs-scope-q').fill(w3);
    await pop.locator(`.vault-check[data-kind="workers"][data-name="${w3}"] input`).check();
    await expect(btn).toHaveText('1 group · 1 worker');
    await expect(pop.locator('#msgs-scope-chips .vault-ms-chip')).toHaveCount(2);
    await expect.poll(async () => [await shows(w1), await shows(w2), await shows(w3)], { timeout: 15000 }).toEqual([true, true, true]);
    await page.screenshot({ path: info.outputPath('picker-two-picks.png') });

    // Removing the group chip leaves only the third worker.
    await pop.locator('#msgs-scope-chips .vault-ms-chip', { hasText: group }).locator('.vault-ms-x').click();
    await expect(btn).toHaveText(w3);
    await expect.poll(async () => [await shows(w1), await shows(w2), await shows(w3)], { timeout: 15000 }).toEqual([false, false, true]);

    // All workers clears it; a click outside closes the picker.
    await pop.getByRole('button', { name: 'All workers' }).click();
    await expect(btn).toHaveText('All workers');
    await page.mouse.click(5, 5);
    await expect(pop).toHaveCount(0);
  } finally {
    for (const w of [w1, w2, w3]) {
      await cleanup('delete ' + w, () => request.post(`/api/sessions/${encodeURIComponent(w)}/delete`, { headers: { ...auth, 'X-Amux-UI-Token': uiToken }, data: {} }));
    }
  }
});
