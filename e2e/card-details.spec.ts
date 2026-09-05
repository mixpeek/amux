import { test, expect } from '@playwright/test';

/**
 * The card is the work record. Pin the compact contract requested in the live
 * audit against a real server and its real slim -> detail hydration path:
 * useful task context, multiple clickable outputs, worker actions, and editing.
 *
 * `:lineage` is deliberately used as the deep-link suffix. Old links may still
 * exist in messages, but the retired database-oriented tab must fall back to
 * Details without treating the suffix as part of the card id or fetching the
 * obsolete panel.
 */
test.describe('board card details', () => {
  test('old lineage links open the useful card record and fit every viewport', async ({ page, request }) => {
    const errors: string[] = [];
    let whyRequests = 0;
    page.on('pageerror', e => errors.push(`pageerror: ${e.message}`));
    page.on('request', r => {
      if (r.url().includes('/api/why/task/')) whyRequests += 1;
    });

    await page.goto('/');
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
    const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };

    const created = await request.post('/api/board', {
      headers: auth,
      data: {
        title: 'card details e2e subject',
        desc: 'Visible task context from authoritative hydration.',
        status: 'todo',
        type: 'chore',
      },
    });
    expect(created.ok(), 'must create the card under test').toBeTruthy();
    const card = (await created.json()).id as string;

    for (const artifact of [
      { kind: 'implementation', ref: '/tmp/amux-card-details/result.md', description: 'created file' },
      { kind: 'verification', ref: 'https://127.0.0.1:1/amux-card-details', description: 'unreachable verification URL' },
    ]) {
      const response = await request.post(`/api/board/${encodeURIComponent(card)}/artifacts`, {
        headers: auth,
        data: { ...artifact, state: 'created' },
      });
      expect(response.ok(), `must attach ${artifact.ref}`).toBeTruthy();
    }

    await page.goto(`/#issue=${encodeURIComponent(card)}:lineage`);
    await expect(page.locator('#board-detail-overlay')).toHaveClass(/active/, { timeout: 30_000 });
    await expect(page.locator('#bd-key')).toHaveText(card);

    const details = page.locator('#bd-tab-preview');
    await expect(details).toHaveClass(/active/);
    await expect(details).toHaveText('Details');
    await expect(page.locator('#bd-tab-lineage')).toHaveCount(0);
    await expect(page.locator('#bd-lineage')).toHaveCount(0);
    await expect(page.locator('#bd-meta')).toContainText('Produced assets (2)', { timeout: 15_000 });
    await expect(page.locator('#bd-preview')).toContainText('Visible task context from authoritative hydration.');

    const assets = page.locator('#bd-meta .bd-card-section', { hasText: 'Produced assets (2)' });
    const file = assets.locator('button.file-link', { hasText: '/tmp/amux-card-details/result.md' });
    await expect(file).toHaveCount(1);
    await expect(file).toHaveAttribute('type', 'button');
    await expect(file).toHaveAttribute('onclick', /openFilePreview/);
    await expect(assets).toContainText('missing');
    const url = assets.locator('a[href="https://127.0.0.1:1/amux-card-details"]');
    await expect(url).toHaveCount(1);
    await expect(url).toHaveAttribute('target', '_blank');
    await expect(url).toHaveAttribute('rel', /noopener/);
    await expect(assets).toContainText('reachability not checked');

    await page.locator('#bd-tab-history').click();
    await expect(page.locator('#bd-tab-history')).toHaveClass(/active/);
    await expect(page.locator('#bd-log')).toBeVisible();
    await expect(page.locator('#bd-log')).toContainText('result.md');
    await expect(page.locator('#bd-meta')).toBeHidden();

    await page.locator('#bd-tab-edit').click();
    await expect(page.locator('#bd-edit-fields')).toBeVisible();
    await expect(page.locator('#bd-edit-footer')).toBeVisible();
    await expect(page.locator('#bd-delete')).toBeVisible();
    await expect(page.locator('#bd-title')).not.toHaveAttribute('readonly', '');

    await details.click();
    await expect(page.locator('#bd-edit-fields')).toBeHidden();
    await expect(page.locator('#bd-delete')).toBeHidden();
    await expect(page.locator('#bd-title')).toHaveAttribute('readonly', '');
    expect(whyRequests, 'retired Lineage UI must not make hidden lineage requests').toBe(0);

    const width = page.viewportSize()!.width;
    const overflow = await page.evaluate(() => {
      const body = document.querySelector('#board-detail-overlay .board-detail-body');
      if (!body) return ['detail body missing'];
      return [...body.querySelectorAll('*')]
        .filter(n => n.getBoundingClientRect().right > window.innerWidth + 1)
        .slice(0, 5)
        .map(n => `${(n as HTMLElement).className} right=${Math.round(n.getBoundingClientRect().right)}`);
    });
    expect(overflow, `card details must fit a ${width}px viewport`).toEqual([]);

    if (width <= 600) {
      for (const tab of ['#bd-tab-preview', '#bd-tab-history', '#bd-tab-edit']) {
        const box = await page.locator(tab).boundingBox();
        expect(box?.height, `${tab} must be a 44px mobile target`).toBeGreaterThanOrEqual(44);
      }
    }
    expect(errors, 'opening and switching card views must not throw').toEqual([]);

    await request.delete(`/api/board/${encodeURIComponent(card)}`, { headers: auth });
  });
});
