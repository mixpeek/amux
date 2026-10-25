import {test, expect} from './fixtures';

// The Mac app reopens each window on the view it was showing, as a deep link
// (#view=files, #view=board, ...). Every window shares one localStorage, so the
// "reopen the last peek" restore holds whichever worker ANY window looked at
// last. On WebKit #view=files opened the Files view synchronously, loadFiles
// rewrote the hash, and the restore then saw no deep link and opened that
// other window's worker over the Files view (2026-10-21, 15-window restore).
test('a #view= deep link is not replaced by the last peek another window saved', async ({page}) => {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [
    {name: 'otherwin', running: true, status: 'idle', dir: '/tmp'},
  ]}));
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.setItem('amux_peek_state', JSON.stringify({session: 'otherwin', ts: Date.now()}));
  });
  for (const view of ['files', 'board', 'sessions']) {
    await page.goto('about:blank');
    await page.goto('/#view=' + view);
    await expect.poll(() => page.evaluate('activeView')).toBe(view);
    // The restore fires 200 ms after boot; give it well past that.
    await page.waitForTimeout(1200);
    expect(await page.evaluate('peekSession'), `#view=${view} kept its view`).toBeNull();
    await expect(page.locator('#peek-overlay')).not.toHaveClass(/active/);
  }

  // Without a deep link the restore still reopens the saved peek.
  await page.goto('about:blank');
  await page.goto('/');
  await expect.poll(() => page.evaluate('peekSession'), {timeout: 8000}).toBe('otherwin');
});
