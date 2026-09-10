import { test, expect } from '../fixtures';
import { boot, checkpoint } from './evidence';

// Fixture transport: exercises the shipped renderer and controls without
// starting an external download. A populated row catches errors empty lists hide.
test('LC-TORRENT: populated progress, pause, resume and removal render at each viewport', async ({ page }, info) => {
  let rows = [{ gid: 'lc-torrent', name: 'Lifecycle sample archive.zip', status: 'active',
    total: 4096, completed: 1024, speed: 128, files: [] }];
  const actions: string[] = [];
  const errors: string[] = [];
  page.on('console', message => { if (message.type() === 'error' && message.text().includes('torrent load')) errors.push(message.text()); });
  await page.route(/\/api\/torrents(?:\/lc-torrent(?:\/(?:pause|resume|remove))?)?$/, route => {
    const request = route.request(), url = new URL(request.url());
    if (request.method() === 'GET') return route.fulfill({ json: rows });
    actions.push(`${request.method()} ${url.pathname}`);
    if (url.pathname.endsWith('/pause')) rows[0].status = 'paused';
    else if (url.pathname.endsWith('/resume')) rows[0].status = 'active';
    else rows = [];
    return route.fulfill({ json: { ok: true } });
  });
  await boot(page);
  const tab = page.locator('#tab-torrents');
  if (!await tab.isVisible()) {
    await page.locator('.tab-customize-wrap > .tab-customize-btn').click();
    await page.locator('#tab-customizer-menu [data-tab-id="torrents"] input[type="checkbox"]').check();
    await page.locator('.tab-customize-wrap > .tab-customize-btn').click();
  }
  await tab.click();
  const list = page.locator('#torrent-list');
  await expect(list).toContainText('Lifecycle sample archive.zip');
  await expect(list).toContainText('25%');
  await expect(list).toContainText('1.0 KB / 4.0 KB');
  const targets = await list.getByRole('button').evaluateAll(buttons => buttons.map(button => {
    const { width, height } = button.getBoundingClientRect();
    return { name: button.getAttribute('aria-label'), width, height };
  }));
  await info.attach('torrent-touch-targets', { body: JSON.stringify(targets), contentType: 'application/json' });
  for (const target of targets) {
    expect(target.width, `${target.name}: touch target width`).toBeGreaterThanOrEqual(44);
    expect(target.height, `${target.name}: touch target height`).toBeGreaterThanOrEqual(44);
  }
  await checkpoint(page, info, 'torrent-active-fixture');
  await list.getByRole('button', { name: 'Pause', exact: true }).click();
  await expect(list).toContainText('paused');
  await expect(list.getByRole('button', { name: 'Resume', exact: true })).toBeVisible();
  await checkpoint(page, info, 'torrent-paused-fixture');
  await list.getByRole('button', { name: 'Resume', exact: true }).click();
  await expect(list).toContainText('active');
  await list.getByRole('button', { name: 'Stop & remove', exact: true }).click();
  await expect(list).toBeEmpty();
  await expect(page.locator('#torrent-empty')).toBeVisible();
  expect(actions).toEqual(['POST /api/torrents/lc-torrent/pause', 'POST /api/torrents/lc-torrent/resume', 'POST /api/torrents/lc-torrent/remove']);
  expect(errors, 'populated torrent rendering must not throw').toEqual([]);
});
