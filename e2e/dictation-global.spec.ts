// The dictionary and dictation settings are one server-wide value each, so they
// have their own Dictation tab beside Workers and each worker's Dictation tab keeps
// only its own History (owner, 2026-11-17: the Dictionary and Settings sub-tabs sat
// inside every worker's tab and read as per-worker; 2026-11-18: "it shouldnt be in
// settings it should be its own tab on the global workers page").
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';

async function openWorkerDictation(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && typeof (window as any)._dictOpenSettings === 'function');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => { await bar.locator('button').last().click(); });
  await page.evaluate(() => { (window as any).openPeek('dict-global-worker'); (window as any).setPeekTab('dictation'); });
  await expect(page.locator('#dict-open-settings')).toBeVisible();
}

test('a worker shows its History; the shared dictionary and settings open in the Dictation tab', async ({ page }, info) => {
  const word = 'Zorblax' + info.project.name;
  const api = (method: string, path: string, body?: any) => page.evaluate(async ([m, p, b]) => {
    const r = await fetch(p as string, { method: m as string, headers: (window as any)._authHeaders({ 'Content-Type': 'application/json' }),
      body: b ? JSON.stringify(b) : undefined });
    return { ok: r.ok, json: await r.json().catch(() => null) };
  }, [method, path, body]);
  const removeWord = async () => {
    const words = (await api('GET', '/api/dictation/dict')).json || [];
    for (const w of words.filter((x: any) => x.word === word)) await api('DELETE', '/api/dictation/dict/' + w.id);
  };
  await openWorkerDictation(page);
  expect((await api('POST', '/api/dictation/dict', { word, correct: '' })).ok).toBe(true);
  try {
    // A fresh open of the worker's tab after the word was added.
    await page.evaluate(() => { (window as any).setPeekTab('dictation'); });
    // No per-worker Dictionary or Settings tab.
    await expect(page.locator('#dict-subtab-dict')).toHaveCount(0);
    await expect(page.locator('#dict-subtab-settings')).toHaveCount(0);
    await expect(page.locator('#dict-subtab-history')).toBeVisible();
    await expect(page.locator('#dict-search')).toBeVisible();

    await page.locator('#dict-open-settings').click();
    await expect(page.locator('#peek-overlay')).not.toHaveClass(/active/);
    await expect(page.locator('#dictation-view')).toBeVisible();
    await expect(page.locator('#tab-dictation')).toHaveClass(/active/);
    await expect(page.locator('#settings-menu')).not.toHaveClass(/open/);
    await expect(page.locator('#settings-menu .settings-tab-btn[data-stab="dictation"]')).toHaveCount(0);
    await expect(page.locator('#dict-dict-body')).toContainText(word);
    await expect(page.locator('#dict-set-body #dict-mic')).toBeAttached();

    // The Edit beside "Dictionary" in the settings goes to the dictionary section.
    await page.locator('#dict-set-body button', { hasText: 'Edit' }).click();
    await expect(page.locator('#settings-dictionary-section')).toBeInViewport();

    // Deleting a word from the Dictation tab refreshes the list there.
    const row = page.locator('#dict-dict-body .dict-wordrow', { hasText: word });
    await row.locator('button[title="Delete"]').click();
    await expect(page.locator('#dict-dict-body')).not.toContainText(word);
  } finally {
    await cleanup('remove the test word from the shared dictionary', removeWord, info);
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
  // The Dictation tab fits the phone: nothing wider than the screen.
  await page.locator('#dict-open-settings').click();
  await expect(page.locator('#dictation-view')).toBeVisible();
  await expect(page.locator('#dict-set-body #dict-mic')).toBeAttached();
  const wide = await page.evaluate(() => [...document.querySelectorAll('#dictation-view *')]
    .filter(e => (e as HTMLElement).offsetParent && e.getBoundingClientRect().right > innerWidth + 1).map(e => e.id || e.className).slice(0, 5));
  expect(wide).toEqual([]);
});

test('the Dictation tab opens from the main tab strip and from its address', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/#view=dictation');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => { await bar.locator('button').last().click(); });
  await expect(page.locator('#dictation-view')).toBeVisible();
  await expect(page.locator('#dict-dict-body .dict-dictbar')).toBeVisible();
  await expect(page.locator('#dict-set-body #dict-mic')).toBeAttached();
  await page.evaluate(() => (window as any).switchView('sessions'));
  await expect(page.locator('#dictation-view')).toBeHidden();
  await page.evaluate(() => document.getElementById('tab-dictation')!.click());
  await expect(page.locator('#dictation-view')).toBeVisible();
  await expect(page.locator('#tab-dictation')).toHaveClass(/active/);
});

// The page says which engine actually transcribes, from GET /api/transcription
// (owner, 2026-11-18: "Make sure the dictation tab global tab page is accurate").
test('the Dictation tab names the engine that runs and what it falls back to', async ({ page }) => {
  const status = (dictate: any, engine = 'auto') => ({ choices: ['auto', 'local', 'gemini'], engine,
    gemini: { configured: true, model: 'gemini-2.5-flash', source: 'server' }, dictate,
    recordings: { local: { available: true, model: 'ggml-base.en.bin' }, uses: { available: true, engine: 'whisper.cpp' } } });
  let reply: any = status({ local: { available: true, model: 'mlx-community/whisper-large-v3-turbo (MLX)' }, uses: { available: true, engine: 'mlx-whisper' } });
  await page.route(/\/api\/transcription$/, r => r.fulfill({ json: reply }));
  await openWorkerDictation(page);
  await page.locator('#dict-open-settings').click();
  await expect(page.locator('#dict-engine-now')).toHaveText('Whisper large-v3-turbo on this Mac (MLX, GPU)');
  await expect(page.locator('#dict-engine-fallback')).toHaveText("Gemini (gemini-2.5-flash, this server's key) when the local engine cannot answer");

  // No local engine: Gemini is what runs, and the page says why local is not.
  reply = status({ local: { available: false, why: "no Whisper 'base' model in ~/.amux/models/whisper" }, uses: { available: true, engine: 'gemini' } });
  await page.evaluate(() => (globalThis as any).eval('_dictGlobalLoad')());
  await expect(page.locator('#dict-engine-now')).toHaveText("Gemini (gemini-2.5-flash, this server's key)");
  await expect(page.locator('#dict-engine-fallback')).toContainText('Local engine unavailable');
});
