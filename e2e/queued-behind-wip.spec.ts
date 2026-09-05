import { test, expect } from './fixtures';

async function renderFrontier(page: import('@playwright/test').Page, holding: string[]) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._stalledChip === 'function');
  await page.evaluate((held) => {
    const frontier = {
      ready: 1,
      readyCards: [{ id: 'TUBES-2419', title: 'Query-flag equivalence' }],
      claimable: 0,
      holding: held,
      measured: true,
      ts: Date.now(),
    };
    eval(`_workFrontier['tubescience'] = ${JSON.stringify(frontier)}`);
    const host = document.createElement('div');
    host.id = 'wip-frontier-specimen';
    host.innerHTML = (window as any)._stalledChip({
      name: 'tubescience', running: true, status: 'idle',
    });
    document.body.appendChild(host);
  }, holding);
}

test('ready work behind WIP names both cards and links the holding card', async ({ page }, info) => {
  await renderFrontier(page, ['TUBES-2418']);
  const chip = page.locator('#wip-frontier-specimen .work-queued-chip');
  await expect(chip).toHaveAttribute(
    'aria-label',
    'TUBES-2419 queued behind current work TUBES-2418',
  );
  await expect(chip).not.toContainText('stalled');

  const narrow = info.project.name !== 'desktop';
  const visible = chip.locator(narrow ? '.work-queued-short' : '.work-queued-wide');
  await expect(visible).toBeVisible();
  await expect(visible).toContainText('TUBES-2419');
  await expect(visible).toContainText('behind TUBES-2418');
  await expect(chip.locator(narrow ? '.work-queued-wide' : '.work-queued-short')).toBeHidden();

  await chip.click();
  await expect(page).toHaveURL(/#issue=TUBES-2418$/);
});

test('ready but unclaimable work with no holding card still says stalled', async ({ page }) => {
  await renderFrontier(page, []);
  const chip = page.locator('#wip-frontier-specimen .status-badge');
  await expect(chip).toContainText('stalled · 1 ready');
  await expect(page.locator('#wip-frontier-specimen .work-queued-chip')).toHaveCount(0);
});
