// The dashboard's own dialogs (showConfirm / showPrompt / showAlert in
// app.js). Every confirm, prompt and alert goes through them now: the native
// ones are blocked in the installed PWA, and tests/dashboard_assets.rs fails
// on a native call. A spec that used `page.on('dialog', d => d.accept())`
// answers these instead; Playwright's dialog event never fires for them.
import { expect, Page } from './fixtures';

export function appDialog(page: Page) {
  return page.locator('#modal-backdrop.open');
}

/** Wait for the shared confirm and press its confirm button, or Cancel. */
export async function answerConfirm(page: Page, accept: boolean, label?: string) {
  const box = appDialog(page);
  await expect(box).toBeVisible();
  if (!accept) return box.locator('#modal-btns').getByRole('button', { name: 'Cancel', exact: true }).click();
  const btn = label
    ? box.locator('#modal-btns').getByRole('button', { name: label, exact: true })
    : box.locator('#modal-btns button').first();
  await btn.click();
}

/** Wait for the shared prompt; type `text` and press OK, or Cancel on null. */
export async function answerPrompt(page: Page, text: string | null) {
  const box = appDialog(page);
  const input = box.locator('#modal-prompt-input');
  await expect(input).toBeVisible();
  if (text === null) return box.locator('#modal-btns').getByRole('button', { name: 'Cancel', exact: true }).click();
  await input.fill(text);
  await box.locator('#modal-btns').getByRole('button', { name: 'OK', exact: true }).click();
}

/**
 * Accept every shared dialog the page opens, as `page.on('dialog', accept)`
 * did for the native ones: a confirm is confirmed, an alert dismissed with OK,
 * and a prompt answered with `prompt` (or its prefilled value when unset).
 * Covers every later navigation, and the page already loaded if there is one.
 */
export async function autoAcceptAppDialogs(page: Page, opts: { prompt?: string } = {}) {
  const install = (answer: string | null) => {
    const hook = () => {
      const bd = document.getElementById('modal-backdrop');
      if (!bd) { setTimeout(hook, 50); return; }
      new MutationObserver(() => {
        if (!bd.classList.contains('open')) return;
        setTimeout(() => {
          if (!bd.classList.contains('open')) return;
          const input = document.getElementById('modal-prompt-input') as HTMLInputElement | null;
          if (input && answer !== null) input.value = answer;
          (bd.querySelector('#modal-btns button') as HTMLElement | null)?.click();
        }, 0);
      }).observe(bd, { attributes: true, attributeFilter: ['class'] });
    };
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', hook);
    else hook();
  };
  const answer = opts.prompt ?? null;
  await page.addInitScript(install, answer);
  if (page.url() !== 'about:blank') await page.evaluate(install, answer);
}
