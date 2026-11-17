// The dictionary and dictation settings are one server-wide value each, so they
// live in Settings > Dictation and each worker's Dictation tab keeps only its own
// History (owner, 2026-11-17: the Dictionary and Settings sub-tabs sat inside every
// worker's tab and read as per-worker).
import { test, expect, Page } from './fixtures';

async function openWorkerDictation(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any)._dictOpenSettings === 'function');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => { await bar.locator('button').last().click(); });
  await page.evaluate(() => { (window as any).openPeek('dict-global-worker'); (window as any).setPeekTab('dictation'); });
  await expect(page.locator('#dict-open-settings')).toBeVisible();
}

test('a worker shows its History; the shared dictionary and settings open in Settings', async ({ page }, info) => {
  const word = 'Zorblax' + info.project.name;
  const added = await page.request.post('/api/dictation/dict', { data: { word, correct: '' } });
  expect(added.ok()).toBe(true);
  try {
    await openWorkerDictation(page);
    // No per-worker Dictionary or Settings tab.
    await expect(page.locator('#dict-subtab-dict')).toHaveCount(0);
    await expect(page.locator('#dict-subtab-settings')).toHaveCount(0);
    await expect(page.locator('#dict-subtab-history')).toBeVisible();
    await expect(page.locator('#dict-search')).toBeVisible();

    await page.locator('#dict-open-settings').click();
    await expect(page.locator('#settings-menu')).toHaveClass(/open/);
    await expect(page.locator('.settings-tab-btn[data-stab="dictation"]')).toHaveClass(/active/);
    await expect(page.locator('#dict-dict-body')).toContainText(word);
    await expect(page.locator('#dict-set-body #dict-mic')).toBeAttached();

    // The Edit beside "Dictionary" in the settings goes to the dictionary section.
    await page.locator('#dict-set-body button', { hasText: 'Edit' }).click();
    await expect(page.locator('#settings-dictionary-section')).toBeInViewport();

    // Deleting a word from Settings refreshes the list there.
    const row = page.locator('#dict-dict-body .dict-wordrow', { hasText: word });
    await row.locator('button[title="Delete"]').click();
    await expect(page.locator('#dict-dict-body')).not.toContainText(word);
  } finally {
    const words = await (await page.request.get('/api/dictation/dict')).json();
    for (const w of words.filter((x: any) => x.word === word)) await page.request.delete('/api/dictation/dict/' + w.id);
  }
});

test('phone: the worker dictation bar is one row and its controls are touchable', async ({ page }) => {
  const vp = page.viewportSize()!;
  test.skip(vp.width > 600, 'phone layout');
  await openWorkerDictation(page);
  const bar = (await page.locator('.dict-subtabs').boundingBox())!;
  expect(bar.height, 'one row').toBeLessThanOrEqual(60);
  for (const sel of ['#dict-subtab-history', '#dict-search', '#dict-open-settings']) {
    const b = (await page.locator(sel).boundingBox())!;
    const mid = b.y + b.height / 2;
    expect(mid, sel + ' sits in the one row').toBeGreaterThanOrEqual(bar.y);
    expect(mid, sel + ' sits in the one row').toBeLessThanOrEqual(bar.y + bar.height);
    expect(b.x + b.width, sel + ' fits the screen').toBeLessThanOrEqual(vp.width + 1);
  }
  expect((await page.locator('#dict-open-settings').boundingBox())!.height).toBeGreaterThanOrEqual(44);
  // The Settings tab strip scrolls sideways to reach Dictation rather than wrapping.
  await page.locator('#dict-open-settings').click();
  await expect(page.locator('#stab-dictation')).toBeVisible();
  const tabs = (await page.locator('#settings-tabs').boundingBox())!;
  expect(tabs.height).toBeLessThanOrEqual(60);
  await expect(page.locator('.settings-tab-btn[data-stab="dictation"]')).toBeInViewport();
});
