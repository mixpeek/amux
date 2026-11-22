// Owner, 2026-11-21: "if I click a response that has a link in it (to open
// that link), it collapses the response text". A reply's first line is the
// header of its collapsible .ptc block, so a link on that line bubbled its
// click to the header toggle and folded the reply. Real render pipeline
// (_peekHtml) and a real click on the rendered link.
import { test, expect } from './fixtures';

const RAW = '\x1b[37m⏺\x1b[39m Done. Opened https://example.com/pr/42 for review\n'
  + '  and the tests pass.\n';

test('clicking a link in a reply header opens it without folding the reply', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekHtml === 'function');
  await page.evaluate((raw) => {
    const body = document.createElement('div');
    body.id = 'ptc-link-probe';
    body.innerHTML = (window as any)._peekHtml(raw);
    document.body.prepend(body);
  }, RAW);
  const block = page.locator('#ptc-link-probe .ptc').first();
  const link = block.locator('.ptc-head a[href*="example.com"]');
  await expect(link, 'the URL on the reply line must render as a link').toHaveCount(1);
  await expect(block).not.toHaveClass(/collapsed/);
  // Keep the page from navigating away; the click itself is what we test.
  await link.evaluate((a: HTMLAnchorElement) => a.addEventListener('click', e => e.preventDefault()));
  await link.click();
  await page.waitForTimeout(400); // past the header's 250 ms single-click delay
  await expect(block, 'a link click must not fold the reply').not.toHaveClass(/collapsed/);
  // The header text itself still toggles, so the guard did not disable it.
  await block.locator('.ptc-head').click({ position: { x: 4, y: 4 } });
  await expect(block).toHaveClass(/collapsed/, { timeout: 2000 });
});
