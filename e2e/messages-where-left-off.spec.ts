// Opening a conversation goes back to where you left off (Ethan, 2026-10-29:
// "when i open messages it should scroll to where i left off"): the first
// message someone else sent since your last read, under a "New" line, else the
// post you were reading when you left. The API is stubbed, like the other
// Messages specs.
import { test, expect, Page } from './fixtures';

const ME = { email: 'ethan@x.com', key: 'person:ethan@x.com', kind: 'person', name: 'ethan', scope_level: 'global', scope_name: '' };
const A = 'g:general', B = 'g:other';
const LIST = { measured: true, n_considered: 2, me: ME,
  channels: [{ conv: A, name: 'general', unread: 20, mentions: 0, last_ts: 0 }, { conv: B, name: 'other', unread: 0, mentions: 0, last_ts: 0 }],
  people: [], workers: [] };
const now = Date.now();
const posts = (conv: string, n: number) => Array.from({ length: n }, (_, i) => ({
  id: (conv === A ? 0 : 1000) + i + 1, conv, author_key: 'person:doron@z.com', author_name: 'doron',
  body: 'message ' + (i + 1) + ' in ' + conv + '\n\nwith a second paragraph so the list scrolls',
  mentions: [], deliveries: [], ts: now - (n - i) * 60000 }));

async function open(page: Page, lastRead: number) {
  await page.route(/\/api\/conversations(\?.*)?$/, r => r.fulfill({ json: LIST }));
  await page.route(/\/api\/conversations\/[^/]+\/posts(\?.*)?$/, r => {
    const conv = decodeURIComponent(r.request().url().split('/api/conversations/')[1].split('/')[0]);
    const after = Number(new URL(r.request().url()).searchParams.get('after') || 0);
    const all = posts(conv, 60).filter(p => p.id > after);
    return r.fulfill({ json: { measured: true, n_considered: all.length, posts: all, last_read: conv === A ? lastRead : 1060,
      people: [], workers: [], mentionable_workers: [], typing: [] } });
  });
  await page.route(/\/api\/conversations\/[^/]+\/(read|typing)$/, r => r.fulfill({ json: { ok: true } }));
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_cv_sel'); localStorage.removeItem('amux_cv_pos'); });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvRestoreScroll === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
}
const inView = (page: Page, id: number) => page.evaluate(i => {
  const box = document.getElementById('cv-posts')!, el = box.querySelector('.cv-post[data-id="' + i + '"]')!;
  const b = box.getBoundingClientRect(), e = el.getBoundingClientRect();
  return e.top >= b.top - 2 && e.top < b.bottom;
}, id);

test('a conversation with unread messages opens at the first one, under a New line', async ({ page }) => {
  await open(page, 40);
  await page.evaluate(c => (0, eval)('_cvSelect(' + JSON.stringify(c) + ')'), A);
  await expect(page.locator('#cv-posts .cv-unread-line')).toHaveCount(1);
  await expect(page.locator('#cv-posts .cv-unread-line + .cv-post')).toHaveAttribute('data-id', '41');
  await expect.poll(() => inView(page, 41)).toBe(true);
});

test('a conversation you scrolled up in reopens at the post you were reading', async ({ page }) => {
  await open(page, 60);
  await page.evaluate(c => (0, eval)('_cvSelect(' + JSON.stringify(c) + ')'), A);
  await expect(page.locator('#cv-posts .cv-post').first()).toBeVisible();
  // Read up to post 12, then leave for another conversation and come back.
  await page.evaluate(() => {
    const box = document.getElementById('cv-posts')!, el = box.querySelector('.cv-post[data-id="12"]')!;
    box.scrollTop += el.getBoundingClientRect().top - box.getBoundingClientRect().top;
    box.dispatchEvent(new Event('scroll'));
  });
  await page.waitForTimeout(500);
  await page.evaluate(c => (0, eval)('_cvSelect(' + JSON.stringify(c) + ')'), B);
  await expect(page.locator('#cv-posts .cv-post[data-id="1060"]')).toBeVisible();
  await page.evaluate(c => (0, eval)('_cvSelect(' + JSON.stringify(c) + ')'), A);
  await expect(page.locator('#cv-posts .cv-unread-line')).toHaveCount(0);
  await expect.poll(() => inView(page, 12)).toBe(true);
});
