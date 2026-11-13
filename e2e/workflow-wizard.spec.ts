// The workflow wizard (#457). A non-technical person answers one plain question
// per screen; the answers become a brief that the New Worker dialog starts a
// worker on. Checked on what the person sees and on what the create request
// carries: the outcome cannot be skipped, a skipped question lands under "Open
// questions", picked chips and typed answers both reach the brief, a half-done
// draft survives a reload, and the phone layout fits 375px.
import { test, expect } from './fixtures';

declare function openWorkflowWizard(): void;
declare function openCreate(): void;
declare function submitCreate(): Promise<void>;

async function boot(page: import('@playwright/test').Page) {
  const errors: string[] = [];
  // WebKit reports a fetch the reload cancelled as an access-control error;
  // that is the reload, not the wizard.
  page.on('pageerror', (e) => { const m = String(e); if (!/Fetch API cannot load .* due to access control checks/.test(m)) errors.push(m); });
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openWorkflowWizard === 'function');
  await page.evaluate(() => localStorage.removeItem('amux-workflow-wizard'));
  return errors;
}

const next = (page: import('@playwright/test').Page) => page.locator('#wfw-next').click();
const skip = (page: import('@playwright/test').Page) => page.locator('#wfw-skip').click();

test('answers become a brief and start the New Worker dialog with it', async ({ page }) => {
  const errors = await boot(page);
  let body: any = null;
  await page.route(/\/api\/sessions$/, async (route) => {
    if (route.request().method() !== 'POST') return route.continue();
    body = route.request().postDataJSON();
    await route.fulfill({ status: 409, json: { error: 'e2e: create intercepted' } });
  });

  // Opened from the New Worker dialog, the way someone stuck on the prompt finds it.
  await page.evaluate(() => openCreate());
  await page.locator('#create-open-wizard').click();
  await expect(page.locator('#wfw-overlay')).toHaveClass(/active/);
  await expect(page.locator('#create-overlay')).not.toHaveClass(/active/);
  await expect(page.locator('#wfw-progress')).toHaveText('Step 1 of 13');

  // The outcome is the one question that cannot be skipped.
  await expect(page.locator('#wfw-skip')).toHaveCount(0);
  await next(page);
  await expect(page.locator('#wfw-error')).toBeVisible();
  await expect(page.locator('#wfw-progress')).toHaveText('Step 1 of 13');
  await page.locator('#wfw-answer').fill('Match last week\'s payments to invoices and list the ones that do not match');
  await next(page);

  // 2 success: skipped, so it must come back as an open question.
  await expect(page.locator('#wfw-q')).toHaveText('How will you know it worked?');
  await skip(page);
  // 3 when
  await page.locator('#wfw-answer').fill('Every Monday, about 200 payments');
  await next(page);
  // 4 systems: a chip plus typed detail.
  await page.locator('#wfw-chips button', { hasText: 'Accounting software' }).click();
  await expect(page.locator('#wfw-chips button', { hasText: 'Accounting software' })).toHaveAttribute('aria-pressed', 'true');
  await page.locator('#wfw-answer').fill('NetSuite, read only');
  await next(page);
  // 5 to 9: rules typed, the rest skipped.
  await skip(page);                       // inputs
  await skip(page);                       // examples
  await page.locator('#wfw-answer').fill('A payment marked "prior virtual payment" went to court');
  await next(page);                       // rules
  await skip(page);                       // exceptions
  await skip(page);                       // tools
  // 10 approval: chip only.
  await page.locator('#wfw-chips button', { hasText: 'Contacting customers' }).click();
  await next(page);
  await skip(page);                       // sensitive
  await skip(page);                       // output
  await skip(page);                       // stuck

  await expect(page.locator('#wfw-progress')).toHaveText('Review');
  const brief = await page.locator('#wfw-brief').innerText();
  expect(brief).toContain('## Outcome\nMatch last week\'s payments');
  expect(brief).toContain('## Systems it needs\n- Accounting software\nNetSuite, read only');
  expect(brief).toContain('## Rules\nA payment marked "prior virtual payment" went to court');
  expect(brief).toContain('## Needs your OK first\n- Contacting customers');
  const open = brief.split('## Open questions')[1] || '';
  expect(open).toContain('How will you know it worked?');
  expect(open).toContain('If it gets stuck, what should it do?');
  expect(open, 'an answered question is not listed as open').not.toContain('What outcome');
  expect(brief).not.toContain('## How you will know it worked');
  await expect(page.locator('#wfw-name')).toHaveValue('match-last-week-s');

  // Hand-off: the existing dialog, prefilled; the create request carries the brief.
  await page.locator('#wfw-name').fill('wexus-payments');
  await page.locator('#wfw-create').click();
  await expect(page.locator('#wfw-overlay')).not.toHaveClass(/active/);
  await expect(page.locator('#create-overlay')).toHaveClass(/active/);
  await expect(page.locator('#create-name')).toHaveValue('wexus-payments');
  await expect(page.locator('#create-prompt')).toHaveValue(/Save this brief as WORKFLOW\.md/);
  await page.evaluate(async () => {
    (document.getElementById('create-dir') as HTMLInputElement).value = '/';
    await submitCreate();
  });
  await expect.poll(() => body).not.toBeNull();
  expect(body.name).toBe('wexus-payments');
  expect(body.prompt).toBe(brief.trim());
  expect(errors, errors.join('\n')).toEqual([]);
});

test('a half-finished draft survives a reload, and Start over clears it', async ({ page }) => {
  const errors = await boot(page);
  await page.evaluate(() => openWorkflowWizard());
  await page.locator('#wfw-answer').fill('Draft outcome');
  await next(page);
  await page.locator('#wfw-answer').fill('Typed but not yet submitted');
  await page.locator('#wfw-overlay').click({ position: { x: 2, y: 2 } });   // close by clicking outside
  await expect(page.locator('#wfw-overlay')).not.toHaveClass(/active/);

  await page.reload();
  await page.waitForFunction(() => typeof (window as any).openWorkflowWizard === 'function');
  await page.evaluate(() => openWorkflowWizard());
  await expect(page.locator('#wfw-progress')).toHaveText('Step 2 of 13');
  await expect(page.locator('#wfw-answer')).toHaveValue('Typed but not yet submitted');
  await page.locator('#wfw-body button', { hasText: 'Back' }).click();
  await expect(page.locator('#wfw-answer')).toHaveValue('Draft outcome');

  // Through to the review, then start over through the shared confirm dialog.
  await next(page);
  for (let i = 0; i < 12; i++) await skip(page);
  await expect(page.locator('#wfw-progress')).toHaveText('Review');
  await page.locator('#wfw-body button', { hasText: 'Start over' }).click();
  await page.locator('#modal-btns button', { hasText: 'Start over' }).click();
  await expect(page.locator('#wfw-progress')).toHaveText('Step 1 of 13');
  await expect(page.locator('#wfw-answer')).toHaveValue('');
  expect(errors, errors.join('\n')).toEqual([]);
});

test('fits a 375px phone with 44px touch targets', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 740 });
  const errors = await boot(page);
  await page.evaluate(() => openWorkflowWizard());
  await page.locator('#wfw-answer').fill('Outcome');
  await next(page);
  await next(page);
  await next(page);   // systems: the step with chips
  await expect(page.locator('#wfw-chips')).toBeVisible();
  const m = await page.evaluate(() => {
    const box = document.getElementById('wfw-box')!.getBoundingClientRect();
    const btns = [...document.querySelectorAll('#wfw-body button')].map(b => (b as HTMLElement).getBoundingClientRect());
    return { left: box.left, right: box.right, vw: innerWidth, minH: Math.min(...btns.map(b => b.height)),
      maxRight: Math.max(...btns.map(b => b.right)), n: btns.length,
      clipped: [...document.querySelectorAll('#wfw-chips button')].filter(b => b.scrollWidth > b.clientWidth + 1).map(b => b.textContent) };
  });
  expect(m.n).toBeGreaterThan(5);
  expect(m.left).toBeGreaterThanOrEqual(0);
  expect(m.right).toBeLessThanOrEqual(m.vw);
  expect(m.maxRight, 'no button runs off the screen').toBeLessThanOrEqual(m.vw);
  expect(m.clipped, 'no option label is cut off').toEqual([]);
  expect(m.minH, 'every button is a 44px touch target').toBeGreaterThanOrEqual(44);
  await page.screenshot({ path: test.info().outputPath('wizard-375.png') });
  expect(errors, errors.join('\n')).toEqual([]);
});
