// Owner, 2026-10-29: "make it so i can tag workers in md files and i can link
// the worker it should only be clickable when in preview mode (not in edit
// mode)". In a markdown file's Preview a worker's @name is a link that opens
// it; an email, an unknown @word and code stay text. Edit and Raw show the text
// as typed, and in Edit typing @ offers the workers, as every input does.
import { test, expect, Page, allowUnusedRoute } from './fixtures';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const W = 'md-tag-w';
const DIR = mkdtempSync(join(tmpdir(), 'amux-e2e-mdw-'));
const FILE = join(DIR, 'plan.md');
writeFileSync(FILE, '# Plan\n\nAsk @' + W + ' about the launch.\n\nMail ethan@example.com, not @nobody-here.\n\nIn code: `@' + W + '` stays.\n');

async function boot(page: Page) {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [{ name: W, running: true, status: 'idle', dir: DIR }] }));
  await page.route(/\/api\/sessions\/md-tag-w\/peek\?/, r => r.fulfill({ json: { name: W, live: 'ready', history: '' } }));
  await page.route(/\/api\/sessions\/md-tag-w\/subagents$/, r => r.fulfill({ json: { session: W, subagents: [] } }));
  // Only the test that opens the worker reads these.
  allowUnusedRoute(page, /\/api\/sessions\/md-tag-w\/peek\?/);
  allowUnusedRoute(page, /\/api\/sessions\/md-tag-w\/subagents$/);
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(w => typeof (window as any).openFilePreview === 'function'
    && (globalThis as any).eval('sessions').some((s: any) => s.name === w), W);
  await page.evaluate(p => (window as any).openFilePreview(p), FILE);
  await expect(page.locator('#file-body')).toContainText('Ask');
}

test('a worker tagged in a markdown file is a link in Preview, and opens the worker', async ({ page }) => {
  await boot(page);
  const links = page.locator('#file-body .md-worker-link');
  await expect(links).toHaveCount(1);
  await expect(links.first()).toHaveText('@' + W);
  // Not links: an email, an @word that is no worker, a tag inside code.
  await expect(page.locator('#file-body')).toContainText('ethan@example.com');
  await expect(page.locator('#file-body')).toContainText('@nobody-here');
  await expect(page.locator('#file-body code')).toHaveText('@' + W);
  await links.first().click();
  await expect(page.locator('#peek-overlay')).toHaveAttribute('data-session', W);
});

test('Edit and Raw show the tag as text, and Edit offers workers after @', async ({ page }) => {
  await boot(page);
  await page.evaluate(() => (window as any).setFileViewMode('raw'));
  await expect(page.locator('#file-body .md-worker-link')).toHaveCount(0);
  await expect(page.locator('#file-body')).toContainText('Ask @' + W);
  await page.evaluate(() => (window as any).setFileViewMode('edit'));
  const ta = page.locator('#file-edit-ta');
  await expect(ta).toBeVisible();
  await expect(page.locator('#file-edit-wrap a.md-worker-link')).toHaveCount(0);
  await expect(ta).toHaveValue(new RegExp('Ask @' + W));
  // Type a new tag at the end: the worker list opens and Enter takes it.
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(el.value.length, el.value.length); });
  await page.keyboard.type('\nThen @md-t');
  const list = page.locator('#file-edit-wrap .ac-list.open');
  await expect(list).toBeVisible();
  await expect(list).toContainText(W);
  await page.keyboard.press('Enter');
  await expect(ta).toHaveValue(new RegExp('Then @' + W + ' $'));
  // Back in Preview the new tag is a link too.
  await page.evaluate(() => {
    const w = window as any;
    w.eval('_fileData').content = (document.getElementById('file-edit-ta') as HTMLTextAreaElement).value;
    w.setFileViewMode('preview');
  });
  await expect(page.locator('#file-body .md-worker-link')).toHaveCount(2);
});
