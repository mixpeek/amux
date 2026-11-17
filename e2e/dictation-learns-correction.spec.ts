// A corrected dictation offers its fix to the dictionary (Ethan, 2026-11-17:
// "if I have to change or modify or edit or retype a word, it should prompt
// me to ask if that change should be added to the dictionary so that it
// knows moving forward"). The dictionary API is stubbed so the test reads
// exactly what the page would save.
import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';

async function setup(page: Page, stub = true) {
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_dict_declined'); });
  const posted: any[] = [];
  if (stub) await page.route('**/api/dictation/dict', async (route) => {
    if (route.request().method() === 'POST') { posted.push(route.request().postDataJSON()); await route.fulfill({ json: { ok: true, id: 1 } }); }
    else await route.fulfill({ json: [] });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._dictWatchField === 'function');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => { await bar.locator('button').last().click(); });
  return posted;
}

// Dictated text lands in a field, then the person edits it as typing would.
async function dictateThenEdit(page: Page, dictated: string, edited: string) {
  await page.evaluate(([d, e]) => {
    document.getElementById('dict-fix-box')?.remove();
    const t = document.createElement('textarea');
    t.id = 'dict-fix-box';
    t.style.cssText = 'position:fixed;top:120px;left:40px;width:300px;height:80px;z-index:99999';
    document.body.appendChild(t);
    t.value = d; (window as any)._dictWatchField(t, d);
    t.value = e; t.dispatchEvent(new Event('input', { bubbles: true }));
  }, [dictated, edited]);
}

test('the word pairs a correction yields', async ({ page }) => {
  await setup(page, false);  // pure function: no dictionary request to stub
  const c = (d: string, b: string, a: string) => page.evaluate(([d, b, a]) => (window as any)._dictCorrections(d, b, a), [d, b, a]);
  expect(await c('ping mix peak about pricing', 'ping mix peak about pricing', 'ping Mixpeek about pricing')).toEqual([{ from: 'mix peak', to: 'Mixpeek' }]);
  expect(await c('call ethan', 'Hi, call ethan.', 'Hi, call Ethan.')).toEqual([{ from: 'ethan', to: 'Ethan' }]);
  expect(await c('send it now', 'send it now', 'send now'), 'a deletion is not a misheard word').toEqual([]);
  expect(await c('send it', 'send it', 'send it today'), 'an addition is not a misheard word').toEqual([]);
  expect(await c('hello there', 'typed words hello there', 'changed words hello there'), 'words the person typed are not offered').toEqual([]);
  expect(await c('send it please', 'send it please', 'typed words'), 'replacing the whole text is not a fix').toEqual([]);
  expect(await c('ethan', 'ethan', 'Ethan'), 'a one-word dictation can be fixed').toEqual([{ from: 'ethan', to: 'Ethan' }]);
});

test('the offer comes as soon as the corrected word ends', async ({ page }) => {
  const posted = await setup(page);
  await page.evaluate(() => {
    const t = document.createElement('textarea');
    t.id = 'dict-fix-box';
    t.style.cssText = 'position:fixed;top:120px;left:40px;width:300px;height:80px;z-index:99999';
    document.body.appendChild(t);
    t.value = 'ping mix peak about pricing'; (window as any)._dictWatchField(t, 'ping mix peak about pricing', '');
    t.value = 'ping Mixpeek  about pricing';
    t.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: ' ' }));
  });
  await expect(page.locator('#modal-msg')).toContainText('mix peak', { timeout: 1500 });
  await page.locator('#modal-btns .btn.primary').click();
  await expect.poll(() => posted.length).toBe(1);
});

test('editing a dictated word offers it, and Add saves heard -> typed', async ({ page }) => {
  const posted = await setup(page);
  await dictateThenEdit(page, 'ping mix peak about pricing', 'ping Mixpeek about pricing');
  await expect(page.locator('#modal-msg')).toContainText('mix peak', { timeout: 6000 });
  await expect(page.locator('#modal-msg')).toContainText('Mixpeek');
  await page.locator('#modal-btns .btn.primary').click();
  await expect.poll(() => posted.length).toBe(1);
  expect(posted[0]).toEqual({ word: 'mix peak', correct: 'Mixpeek' });
});

test('a declined fix is not offered again, and a plain deletion offers nothing', async ({ page }) => {
  const posted = await setup(page);
  await dictateThenEdit(page, 'meet with hours team', 'meet with ours team');
  await expect(page.locator('#modal-msg')).toContainText('hours', { timeout: 6000 });
  await page.locator('#modal-btns .btn').last().click();
  await dictateThenEdit(page, 'meet with hours team', 'meet with ours team');
  await page.waitForTimeout(3500);
  await expect(page.locator('#modal-msg')).not.toBeVisible();
  await dictateThenEdit(page, 'send it now please', 'send it please');
  await page.waitForTimeout(3500);
  await expect(page.locator('#modal-msg')).not.toBeVisible();
  expect(posted).toEqual([]);
});

test('a word from an earlier dictation into the same field is still offered', async ({ page }) => {
  const posted = await setup(page);
  await page.evaluate(() => {
    const t = document.createElement('textarea');
    t.id = 'dict-fix-box';
    t.style.cssText = 'position:fixed;top:120px;left:40px;width:300px;height:80px;z-index:99999';
    document.body.appendChild(t);
    t.value = 'ping mix peak'; (window as any)._dictWatchField(t, 'ping mix peak');
    t.value = 'ping mix peak about pricing'; (window as any)._dictWatchField(t, 'about pricing');
    t.value = 'ping Mixpeek about pricing'; t.dispatchEvent(new Event('input', { bubbles: true }));
  });
  await expect(page.locator('#modal-msg')).toContainText('mix peak', { timeout: 6000 });
  await page.locator('#modal-btns .btn.primary').click();
  await expect.poll(() => posted.length).toBe(1);
  expect(posted[0]).toEqual({ word: 'mix peak', correct: 'Mixpeek' });
});

test('a correction made just before a second dictation is still offered', async ({ page }) => {
  const posted = await setup(page);
  await page.evaluate(() => {
    const t = document.createElement('textarea');
    t.id = 'dict-fix-box';
    t.style.cssText = 'position:fixed;top:120px;left:40px;width:300px;height:80px;z-index:99999';
    document.body.appendChild(t);
    t.value = 'ping mix peak'; (window as any)._dictWatchField(t, 'ping mix peak', '');
    t.value = 'ping Mixpeek'; t.dispatchEvent(new Event('input', { bubbles: true }));
    // The second dictation lands well inside the 2.5 s pause.
    const before = t.value; t.value = before + ' about pricing'; (window as any)._dictWatchField(t, 'about pricing', before);
  });
  await expect(page.locator('#modal-msg')).toContainText('mix peak', { timeout: 6000 });
  await page.locator('#modal-btns .btn.primary').click();
  await expect.poll(() => posted.length).toBe(1);
  expect(posted[0]).toEqual({ word: 'mix peak', correct: 'Mixpeek' });
});
