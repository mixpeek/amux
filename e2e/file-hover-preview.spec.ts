// Hovering a file link shows the head of the file (owner, 2026-11-18: "file
// path is clickable and on mouseover shows a preview"; 2026-11-19: "file
// preview mouseover not working"). The card opened and was closed by the next
// scroll anywhere on the page, which a live terminal or list fires all the
// time; and a path in worker output (.file-link) never had one.
import { test, expect, allowUnusedRoute } from './fixtures';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

test.beforeEach(({}, info) => { test.skip(info.project.name !== 'desktop', 'a hover preview needs a pointer that hovers'); });

function files() {
  const dir = mkdtempSync(join(tmpdir(), 'amux-e2e-hover-'));
  const other = join(dir, 'other.md');
  writeFileSync(other, '# Other file\n\nThe body of the other file.\n');
  const main = join(dir, 'main.md');
  writeFileSync(main, `# Main\n\nSee [the other file](other.md).\n`);
  return { main, other };
}
async function boot(page) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [] }));
  allowUnusedRoute(page, /\/api\/sessions(?:\?.*)?$/);
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
}
const card = (page) => page.locator('#worker-hovercard');

test('a markdown file link shows the file, and a scroll elsewhere does not close it', async ({ page }) => {
  const { main } = files();
  await boot(page);
  await page.evaluate(p => (window as any).openFilePreview(p), main);
  const link = page.locator('#file-body .md-file-link').first();
  await link.hover();
  await expect(card(page)).toBeVisible();
  await expect(card(page)).toContainText('The body of the other file');
  // Something else on the page scrolls (a live terminal does this constantly).
  await page.evaluate(() => {
    const far = document.createElement('div'); far.id = 'zz-far'; far.style.cssText = 'position:fixed;left:0;bottom:0;width:50px;height:50px;overflow:auto';
    far.innerHTML = '<div style="height:400px"></div>'; document.body.appendChild(far); far.scrollTop = 100;
    far.dispatchEvent(new Event('scroll'));
  });
  await page.waitForTimeout(300);
  await expect(card(page), 'a scroll that does not move the link leaves the card open').toBeVisible();
  // The link's own container scrolls: the link moved, so the card goes.
  await page.evaluate(() => document.querySelector('#file-body')!.dispatchEvent(new Event('scroll')));
  await expect(card(page)).toBeHidden();
});

test('a path in worker output (.file-link) shows the file too', async ({ page }) => {
  const { other } = files();
  await boot(page);
  await page.evaluate(p => {
    const a = document.createElement('a'); a.className = 'file-link'; a.dataset.path = p; a.textContent = p; a.href = '#';
    a.style.cssText = 'position:fixed;top:200px;left:200px;z-index:5000;background:#fff'; a.id = 'zz-out-link';
    document.body.appendChild(a);
  }, other);
  await page.locator('#zz-out-link').hover();
  await expect(card(page)).toBeVisible();
  await expect(card(page)).toContainText('The body of the other file');
});
