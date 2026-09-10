import { test, expect } from './fixtures';

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
      { kind: 'implementation', ref: '/tmp/amux-card-details/result.md', description: 'created file', state: 'created' },
      { kind: 'verification', ref: 'https://127.0.0.1:1/amux-card-details', description: 'unreachable verification URL', state: 'created' },
      { kind: 'verification', ref: 'https://localhost:8824/api/health', description: 'same amux server', state: 'created' },
      { kind: 'verification', ref: 'https://example.test/amux/commit/not-real', description: 'superseded guessed commit', state: 'invalid' },
    ]) {
      const response = await request.post(`/api/board/${encodeURIComponent(card)}/artifacts`, {
        headers: auth,
        data: artifact,
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
    await expect(page.locator('#bd-meta')).toContainText('Produced output (3)', { timeout: 15_000 });
    await expect(page.locator('#bd-meta')).toContainText('Retired artifacts (1)');
    await expect(page.locator('#bd-preview')).toContainText('Visible task context from authoritative hydration.');

    const assets = page.locator('#bd-meta .bd-card-section', { hasText: 'Produced output (3)' });
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
    const origin = await page.evaluate(() => window.location.origin);
    const sameServer = assets.locator('a[data-original-ref="https://localhost:8824/api/health"]');
    await expect(sameServer).toHaveAttribute('href', `${origin}/api/health`);
    await expect(sameServer).toHaveText(`${origin}/api/health`);
    await expect(assets).not.toContainText('https://localhost:8824');
    await expect(assets.locator('a, button.file-link')).toHaveCount(3);

    const retired = page.locator('#bd-meta .bd-card-section', { hasText: 'Retired artifacts (1)' });
    await expect(retired).toContainText('https://example.test/amux/commit/not-real');
    await expect(retired).toContainText('invalid');
    await expect(retired.locator('a, button')).toHaveCount(0);

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

  test('terminal Refresh rehydrates the durable final summary without asking the worker', async ({ page, request }) => {
    let statusRequests = 0;
    page.on('request', r => {
      if (r.url().includes('/status-request')) statusRequests += 1;
    });

    await page.goto('/');
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
    const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
    const worker = 'terminal-summary-refresh-worker';
    const created = await request.post('/api/board', {
      headers: { ...auth, 'X-Amux-Worker': worker },
      data: { title: 'durable terminal summary refresh', status: 'doing', type: 'chore', session: worker },
    });
    expect(created.ok()).toBeTruthy();
    const card = (await created.json()).id as string;
    const artifact = '/tmp/ate-84-terminal-summary.txt';
    const attached = await request.post(`/api/board/${encodeURIComponent(card)}/artifacts`, {
      headers: auth,
      data: { kind: 'verification', ref: artifact, state: 'created', description: 'focused acceptance output' },
    });
    expect(attached.ok()).toBeTruthy();
    const finished = await request.patch(`/api/board/${encodeURIComponent(card)}`, {
      headers: { ...auth, 'X-Amux-Worker': worker },
      data: {
        status: 'done',
        evidence: 'tests: focused board API and Playwright acceptance passed; deployment: https://example.test/ate-84; live acceptance: passed',
        gate_ack: true,
      },
    });
    expect(finished.ok()).toBeTruthy();

    await page.goto(`/#issue=${encodeURIComponent(card)}`);
    await expect(page.locator('#board-detail-overlay')).toHaveClass(/active/, { timeout: 30_000 });
    await expect(page.locator('#bd-status-banner')).toContainText('Final outcome: done', { timeout: 15_000 });
    await expect(page.locator('#bd-meta')).toContainText('Tests/deployment/live evidence:');
    await expect(page.locator('#bd-meta')).toContainText('Produced output');
    await expect(page.locator('#bd-meta')).toContainText(artifact);

    const refresh = page.locator('#bd-status-banner button', { hasText: `Refresh from ${worker}` });
    await expect(refresh).toHaveCount(1);
    await refresh.click();
    await expect(page.locator('#bd-status-banner')).toContainText('Final outcome: done');
    await expect(page.locator('#bd-meta')).toContainText('focused board API and Playwright acceptance passed');
    expect(statusRequests, 'terminal Refresh must read the board, not ask the provider').toBe(0);

    await request.delete(`/api/board/${encodeURIComponent(card)}`, { headers: auth });
  });

  test('authoritative status wins a stale cached detail and old Refresh never asks the provider', async ({ page, request }) => {
    let statusRequests = 0;
    let detailGets = 0;
    page.on('request', r => {
      if (r.url().includes('/status-request')) statusRequests += 1;
    });

    await page.goto('/');
    const walkthrough = page.locator('#wt-overlay.open');
    await walkthrough.waitFor({ state: 'visible', timeout: 2_000 }).catch(() => {});
    if (await walkthrough.isVisible()) await page.locator('#wt-tooltip .wt-skip').click();
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
    const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
    const worker = 'stale-detail-hydration-worker';
    const created = await request.post('/api/board', {
      headers: { ...auth, 'X-Amux-Worker': worker },
      data: { title: 'stale detail hydration race', status: 'doing', type: 'chore', session: worker },
    });
    expect(created.ok()).toBeTruthy();
    const card = (await created.json()).id as string;
    const seeded = await request.post(`/api/board/${encodeURIComponent(card)}/status-update`, {
      headers: { ...auth, 'X-Amux-Worker': worker },
      data: { text: 'provider status seeded before the terminal transition' },
    });
    expect(seeded.ok()).toBeTruthy();

    // Load the doing row into the SPA cache, then close the card behind the
    // old client's back. Opening the detail now starts from stale `doing`.
    await page.evaluate(() => (window as any).fetchBoard());
    const finished = await request.patch(`/api/board/${encodeURIComponent(card)}`, {
      headers: { ...auth, 'X-Amux-Worker': worker },
      data: {
        status: 'done',
        evidence: 'tests: stale hydration regression; deployment: https://example.test/stale-hydration; live acceptance: passed',
        gate_ack: true,
      },
    });
    expect(finished.ok()).toBeTruthy();

    // Hold the first detail GET open so the stale Refresh button is clicked
    // while the initial hydration is still in flight. The fixed client must
    // make the GET-first decision and issue zero status-request POSTs.
    await page.route(`**/api/board/${card}`, async route => {
      detailGets += 1;
      if (detailGets === 1) await new Promise(resolve => setTimeout(resolve, 750));
      await route.continue();
    });
    await page.evaluate((id) => (window as any).openBoardDetail(id), card);
    await expect(page.locator('#board-detail-overlay')).toHaveClass(/active/, { timeout: 30_000 });
    const refresh = page.locator('#bd-status-banner button', { hasText: `Refresh from ${worker}` });
    await expect(refresh).toHaveCount(1);
    await refresh.click();

    await expect(page.locator('#bd-status-banner')).toContainText('Final outcome: done', { timeout: 15_000 });
    const doneButton = page.locator('#bd-status-select');
    await expect(doneButton).toHaveValue('done');
    expect(detailGets).toBeGreaterThanOrEqual(2);
    expect(statusRequests, 'a stale old client must not route a terminal Refresh to the worker').toBe(0);

    await request.delete(`/api/board/${encodeURIComponent(card)}`, { headers: auth });
  });

  test('terminal hydration clears a persisted stale status draft and keeps late provider text out of the banner', async ({ page, request }) => {
    let statusRequests = 0;
    let detailGets = 0;
    page.on('request', r => {
      if (r.url().includes('/status-request')) statusRequests += 1;
    });

    await page.goto('/');
    const walkthrough = page.locator('#wt-overlay.open');
    await walkthrough.waitFor({ state: 'visible', timeout: 2_000 }).catch(() => {});
    if (await walkthrough.isVisible()) await page.locator('#wt-tooltip .wt-skip').click();
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
    const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
    const worker = 'persisted-terminal-draft-worker';
    const created = await request.post('/api/board', {
      headers: { ...auth, 'X-Amux-Worker': worker },
      data: {
        title: 'persisted stale terminal draft',
        desc: 'The authoritative description must survive hydration.',
        status: 'doing',
        type: 'chore',
        session: worker,
      },
    });
    expect(created.ok()).toBeTruthy();
    const card = (await created.json()).id as string;
    const seeded = await request.post(`/api/board/${encodeURIComponent(card)}/status-update`, {
      headers: { ...auth, 'X-Amux-Worker': worker },
      data: { text: 'provider status before terminal close' },
    });
    expect(seeded.ok()).toBeTruthy();
    const beforeClose = await request.get(`/api/board/${encodeURIComponent(card)}`, { headers: auth });
    expect(beforeClose.ok()).toBeTruthy();
    const staleBase = await beforeClose.json();

    // Make the old client durable state explicit, then reload so the SPA
    // boots with the fossil rather than merely holding it in a test variable.
    await page.evaluate(({ card, worker, base }) => {
      localStorage.setItem('amux_board_drafts', JSON.stringify({
        [card]: {
          title: base.title,
          desc: base.desc,
          worker,
          status: 'doing',
          due: base.due || '',
          due_time: base.due_time || '',
        },
      }));
    }, { card, worker, base: staleBase });
    await page.reload();
    await page.evaluate(() => (window as any).fetchBoard());

    const finished = await request.patch(`/api/board/${encodeURIComponent(card)}`, {
      headers: { ...auth, 'X-Amux-Worker': worker },
      data: {
        status: 'done',
        evidence: 'tests: persisted stale draft regression; deployment: https://example.test/ate-84; live acceptance: passed',
        gate_ack: true,
      },
    });
    expect(finished.ok()).toBeTruthy();
    const late = await request.post(`/api/board/${encodeURIComponent(card)}/status-update`, {
      headers: { ...auth, 'X-Amux-Worker': worker },
      data: { text: 'stale Codex provider output after terminal close' },
    });
    expect(late.ok()).toBeTruthy();
    const authoritative = await request.get(`/api/board/${encodeURIComponent(card)}`, { headers: auth });
    expect(authoritative.ok()).toBeTruthy();
    const finalCard = await authoritative.json();
    expect(finalCard.status).toBe('done');
    expect(finalCard.last_result).toContain('Final outcome: done');
    expect(finalCard.last_result).not.toContain('stale Codex provider output after terminal close');

    await page.route(`**/api/board/${card}`, async route => {
      detailGets += 1;
      if (detailGets === 1) await new Promise(resolve => setTimeout(resolve, 750));
      await route.continue();
    });
    await page.evaluate((id) => (window as any).openBoardDetail(id), card);
    await expect(page.locator('#board-detail-overlay')).toHaveClass(/active/, { timeout: 30_000 });
    const refresh = page.locator('#bd-status-banner button', { hasText: `Refresh from ${worker}` });
    await expect(refresh).toHaveCount(1);
    await refresh.click();

    await expect(page.locator('#bd-status-banner')).toContainText('Final outcome: done', { timeout: 15_000 });
    await expect(page.locator('#bd-status-banner')).not.toContainText('stale Codex provider output after terminal close');
    await expect(page.locator('#bd-status-select')).toHaveValue('done');
    await expect(page.locator('#toast')).toHaveText('Refreshed final terminal summary from the board');
    expect(detailGets).toBeGreaterThanOrEqual(2);
    expect(statusRequests, 'terminal Refresh must not ask the provider').toBe(0);

    await request.delete(`/api/board/${encodeURIComponent(card)}`, { headers: auth });
  });
});
