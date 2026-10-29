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
  // The worker shows: the file closed rather than covering it (owner,
  // 2026-10-29: "when i @ a worker and click it it should open that worker").
  await expect(page.locator('#file-overlay')).not.toHaveClass(/active/);
  await expect(page.locator('#peek-overlay')).toBeVisible();
  // And Back returns to the file.
  await expect.poll(() => page.evaluate(() => (globalThis as any).eval('_nav.list[_nav.pos] && _nav.list[_nav.pos].k'))).toBe('peek');
  await page.evaluate(() => (window as any).navBack());
  await expect(page.locator('#file-overlay')).toHaveClass(/active/);
  await expect(page.locator('#file-body')).toContainText('Ask');
});

test('Edit and Raw show the tag as text, and Edit offers workers after @', async ({ page }) => {
  await boot(page);
  // Raw is the editor's source pane: the text as typed, no preview showing.
  await page.evaluate(() => (window as any).setFileViewMode('raw'));
  await expect(page.locator('#mdx .mdx-view')).toBeHidden();
  await expect(page.locator('#file-edit-ta')).toHaveValue(new RegExp('Ask @' + W));
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

// Owner, 2026-10-29 (MSG-100765): "this should autocomplete dropdown the
// worker". Typed partway down the editor, the list opened under the whole
// box, behind the status bar. It opens at the caret, on top, and Enter takes it.
test('the worker list opens at the caret in the editor, on top, and Enter takes it', async ({ page }) => {
  await boot(page);
  await page.evaluate(() => (window as any).setFileViewMode('edit'));
  const ta = page.locator('#file-edit-ta');
  await ta.evaluate((el: HTMLTextAreaElement) => { el.focus(); el.setSelectionRange(8, 8); });
  await page.keyboard.type(' contract, comments @md-t');
  const list = page.locator('#file-edit-wrap .ac-list.open');
  await expect(list).toBeVisible();
  const r = await page.evaluate(() => {
    const l = document.querySelector('#file-edit-wrap .ac-list.open') as HTMLElement;
    const b = l.getBoundingClientRect(), t = document.getElementById('file-edit-ta')!.getBoundingClientRect();
    const hit = document.elementFromPoint(b.left + 10, b.top + 10);
    return { onTop: !!hit && l.contains(hit), inside: b.top >= t.top && b.bottom <= t.bottom + 1, nearTop: b.top - t.top < 120 };
  });
  expect(r.onTop, 'nothing covers the list').toBe(true);
  expect(r.inside, 'inside the editor').toBe(true);
  expect(r.nearTop, 'by the caret (line 3), not at the bottom').toBe(true);
  await page.screenshot({ path: test.info().outputPath('md-at-caret.png') });
  await page.keyboard.press('Enter');
  await expect(ta).toHaveValue(new RegExp('comments @' + W + ' '));
});

// Owner, 2026-10-29: "when i refresh an open md file it goes to the worker
// view". A file opened over a worker takes the address, so a reload reopens
// the file; closing it gives the worker's address back.
test('a file opened over a worker reopens on reload, and closing it gives the worker back its address', async ({ page }) => {
  await boot(page);
  await page.evaluate(() => (window as any).closeFilePreview());
  await page.evaluate(w => (window as any).openPeek(w), W);
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('peek=');
  await page.evaluate(p => (window as any).openFilePreview(p), FILE);
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('#path=');
  await page.reload();
  await expect(page.locator('#file-overlay')).toHaveClass(/active/, { timeout: 15000 });
  await expect(page.locator('#file-body')).toContainText('Ask');
  // Closing a file opened over a worker puts the worker's address back.
  await page.evaluate(() => (window as any).closeFilePreview());
  await page.evaluate(w => (window as any).openPeek(w), W);
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('peek=');
  await page.evaluate(p => (window as any).openFilePreview(p), FILE);
  await page.evaluate(() => (window as any).closeFilePreview());
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('peek=');
});
