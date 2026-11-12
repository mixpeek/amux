// Opening a conversation keeps your place, and the place holds while images
// load (Ethan, 2026-11-08: "When I open chats it should scroll to wherever I
// last left it off, currently it just scrolls to the bottom and cuts off some
// shit"). The newest post was an image whose preview got its height after the
// list had been scrolled to the end, so it sat half hidden behind the
// composer. The API is stubbed, like messages-where-left-off.spec.ts.
import { test, expect, Page } from './fixtures';

const ME = { email: 'ethan@x.com', key: 'person:ethan@x.com', kind: 'person', name: 'ethan', scope_level: 'global', scope_name: '' };
const A = 'g:general', B = 'g:other';
const LIST = { measured: true, n_considered: 2, me: ME,
  channels: [{ conv: A, name: 'general', unread: 0, mentions: 0, last_ts: 0 }, { conv: B, name: 'other', unread: 0, mentions: 0, last_ts: 0 }],
  people: [], workers: [] };
const now = Date.now();
const SHOT = '/tmp/scroll-settles-shot.png';
// A tall image, served late: it lands after the list was placed.
const SVG = '<svg xmlns="http://www.w3.org/2000/svg" width="220" height="160"><rect width="220" height="160" fill="#888"/></svg>';

function posts(conv: string, n: number, imageLast: boolean, extra: number) {
  const base = conv === A ? 0 : 1000;
  const out = Array.from({ length: n + extra }, (_, i) => ({
    id: base + i + 1, conv, author_key: 'person:doron@z.com', author_name: 'doron',
    body: 'message ' + (i + 1) + ' in ' + conv + '\n\nwith a second paragraph so the list scrolls',
    mentions: [], deliveries: [], ts: now - (n + extra - i) * 60000 }));
  if (imageLast) out[n - 1].body = 'the screenshot ' + SHOT;
  return out;
}

async function open(page: Page, opts: { imageLast?: boolean } = {}) {
  const state = { extra: 0 };
  await page.route(/\/api\/conversations(\?.*)?$/, r => r.fulfill({ json: LIST }));
  await page.route(/\/api\/conversations\/[^/]+\/posts(\?.*)?$/, r => {
    const conv = decodeURIComponent(r.request().url().split('/api/conversations/')[1].split('/')[0]);
    const after = Number(new URL(r.request().url()).searchParams.get('after') || 0);
    const all = posts(conv, 60, !!opts.imageLast && conv === A, conv === A ? state.extra : 0).filter(p => p.id > after);
    return r.fulfill({ json: { measured: true, n_considered: all.length, posts: all, last_read: conv === A ? 60 + state.extra : 1060,
      people: [], workers: [], mentionable_workers: [], typing: [] } });
  });
  await page.route(/\/api\/conversations\/[^/]+\/(read|typing)$/, r => r.fulfill({ json: { ok: true } }));
  if (opts.imageLast) await page.route(/\/api\/file\/raw\?/, async r => {
    await new Promise(res => setTimeout(res, 1200));
    return r.fulfill({ status: 200, contentType: 'image/svg+xml', body: SVG });
  });
  // Cleared on the first load only, so a reload keeps the saved places.
  await page.addInitScript(() => {
    if (sessionStorage.getItem('scroll-settles-seeded')) return;
    sessionStorage.setItem('scroll-settles-seeded', '1');
    localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amux_cv_sel'); localStorage.removeItem('amux_cv_pos');
  });
  await page.goto('/');
  await enter(page);
  return state;
}
async function enter(page: Page) {
  await page.waitForFunction(() => typeof (window as any)._cvRestoreScroll === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
}
const select = (page: Page, c: string) => page.evaluate(c => (0, eval)('_cvSelect(' + JSON.stringify(c) + ')'), c);
const inView = (page: Page, id: number) => page.evaluate(i => {
  const box = document.getElementById('cv-posts'), el = box && box.querySelector('.cv-post[data-id="' + i + '"]');
  if (!box || !el) return false;
  const b = box.getBoundingClientRect(), e = el.getBoundingClientRect();
  return e.top >= b.top - 2 && e.top < b.bottom;
}, id);
// Read up to a post the way a person does: the view moves, then scroll fires.
const readTo = (page: Page, id: number) => page.evaluate(i => {
  const box = document.getElementById('cv-posts')!, el = box.querySelector('.cv-post[data-id="' + i + '"]')!;
  box.scrollTop += el.getBoundingClientRect().top - box.getBoundingClientRect().top;
  box.dispatchEvent(new Event('scroll'));
}, id);

test('the newest post, an image that loads late, ends inside the view above the composer', async ({ page }) => {
  await open(page, { imageLast: true });
  await select(page, A);
  const img = page.locator('#cv-posts .cv-post[data-id="60"] img.cv-thumb');
  await expect(img).toBeAttached();
  await expect.poll(() => img.evaluate((i: HTMLImageElement) => i.complete && i.naturalHeight > 0), { timeout: 10000 }).toBe(true);
  await expect.poll(() => page.evaluate(() => {
    const box = document.getElementById('cv-posts')!, last = box.querySelector('.cv-post[data-id="60"]')!;
    const compose = document.getElementById('cv-compose')!.getBoundingClientRect();
    const b = box.getBoundingClientRect(), e = last.getBoundingClientRect();
    return e.bottom <= b.bottom + 1 && e.bottom <= compose.top + 1 && box.scrollHeight - box.scrollTop - box.clientHeight < 4;
  })).toBe(true);
});

test('a place left without waiting is kept, and survives a reload', async ({ page }) => {
  await open(page);
  await select(page, A);
  await expect(page.locator('#cv-posts .cv-post[data-id="60"]')).toBeAttached();
  await readTo(page, 12);
  // Straight to the other conversation: the place is written on the way out,
  // not by a timer that used to file it under the conversation you opened.
  await select(page, B);
  await expect(page.locator('#cv-posts .cv-post[data-id="1060"]')).toBeAttached();
  await select(page, A);
  await expect(page.locator('#cv-posts .cv-post[data-id="12"]'), 'conversation A rendered again').toBeAttached();
  await expect.poll(() => inView(page, 12)).toBe(true);
  await page.reload();
  await enter(page);
  await select(page, A);
  await expect(page.locator('#cv-posts .cv-post[data-id="12"]')).toBeAttached();
  await expect.poll(() => inView(page, 12)).toBe(true);
});

test('a new post from someone else does not pull you off the post you are reading', async ({ page }) => {
  const state = await open(page);
  await select(page, A);
  await expect(page.locator('#cv-posts .cv-post[data-id="60"]')).toBeAttached();
  await readTo(page, 12);
  await expect.poll(() => inView(page, 12)).toBe(true);
  state.extra = 1;
  await page.evaluate(() => (0, eval)('_cvPoll(false)'));
  await expect(page.locator('#cv-posts .cv-post[data-id="61"]')).toBeAttached();
  await expect.poll(() => inView(page, 12)).toBe(true);
});
