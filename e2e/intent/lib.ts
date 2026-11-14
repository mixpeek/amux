// Shared steps for the intent-first use-case specs.
import { expect, type APIRequestContext, type Page, type TestInfo } from '@playwright/test';
import * as fs from 'fs';
import * as path from 'path';

export const LIVE = !!process.env.AMUX_INTENT_URL;
// Words a non-technical person should never meet on the default path.
export const JARGON = /\b(worker|workers|session|sessions|model|provider|tmux|terminal|card|cards|board|scope|yolo)\b/i;

/** Seed the household calendar once per server (isolated runs only). */
export async function seed(request: APIRequestContext, info: TestInfo) {
  const home = String(info.project.metadata?.home || '');
  if (!home) return;
  const events = JSON.parse(fs.readFileSync(path.join(home, 'household-events.json'), 'utf8'));
  const have = await (await request.get('/api/cal-events')).json();
  const titles = new Set((Array.isArray(have) ? have : have.events || []).map((e: { title: string }) => e.title));
  for (const e of events) if (!titles.has(e.title)) await request.post('/api/cal-events', { data: e });
}

/** Sends the email log has recorded as delivered. Every spec checks this does not grow. */
export async function deliveredSends(request: APIRequestContext): Promise<number> {
  const r = await request.get('/api/email/log?days=2&limit=500');
  const v = await r.json();
  expect(r.ok(), 'the email log is readable').toBeTruthy();
  return Number(v.delivered_count ?? (v.log || []).filter((x: { delivered?: boolean }) => x.delivered).length);
}

/** A new person lands in Chat, a tab in the normal tab bar, with only Chat
 * and "Everything else" showing. Ideas and what is coming up are sections of Chat. */
export async function openHome(page: Page) {
  await page.goto('/');
  await expect(page.locator('#home-view')).toBeVisible({ timeout: 20_000 });
  await expect(page.locator('#tab-home')).toBeVisible();
  await expect(page.locator('#tab-home')).toHaveClass(/active/);
  await expect(page.locator('#tab-everything')).toBeVisible();
  await expect(page.locator('#tab-ideas, #tab-goals, #ideas-view, #goals-view'), 'one tab: Ideas and Goals are sections of Chat').toHaveCount(0);
  await expect(page.locator('#tab-sessions'), 'the worker tabs are hidden for a new person').toBeHidden();
  await expect(page.locator('#tab-board')).toBeHidden();
  await expect(page.locator('.tab-customize-wrap'), 'the tab list names every worker tab').toBeHidden();
  // "Everything else" ends the bar: the way out comes after Chat.
  const visible = await page.locator('.tab-bar > button').evaluateAll((bs) => bs.filter((b) => (b as HTMLElement).offsetParent !== null).map((b) => b.id));
  expect(visible.at(-1), `tab order: ${visible.join(', ')}`).toBe('tab-everything');
}

/** Say something in Chat and land in its thread. */
export async function ask(page: Page, text: string) {
  if (await page.locator('#home-composer').isHidden()) await page.locator('#tab-home').click();
  if (await page.locator('.home-back').isVisible()) await page.locator('.home-back').click();
  await page.locator('#home-input').fill(text);
  await page.locator('#home-send').click();
  await expect(page.locator('.home-thread-title')).toBeVisible({ timeout: LIVE ? 180_000 : 90_000 });
  await expect(page.locator('.home-receipt')).not.toBeEmpty();
  // An open conversation shows only itself: what is coming up and ideas wait on the list.
  await expect(page.locator('#home-goals'), 'coming up hides in a conversation').toBeHidden();
  await expect(page.locator('#home-ideas'), 'ideas hide in a conversation').toBeHidden();
}

/** Wait for a helper answer in the open thread that matches `re`. */
export async function reply(page: Page, re: RegExp) {
  await expect(page.locator('.cv-post[data-from="helper"] .cv-post-body').filter({ hasText: re }).first()).toBeVisible({ timeout: LIVE ? 600_000 : 60_000 });
  // The answer is scrolled to inside Chat's pane, never the page: the tabs stay on screen.
  await expect(page.locator('#tab-home'), 'the tab bar stays on screen').toBeInViewport();
}

/** The default path shows no machinery words. amux's own words are checked;
 * the family's messages and answers about them (a camp's "Session 2") are not. */
export async function noJargon(page: Page) {
  // The visible tab bar and Chat, with its sections.
  const text = await page.evaluate(() => {
    const roots = [document.querySelector('.tab-bar'), document.getElementById('home-view')]
      .filter((r): r is HTMLElement => !!r && (r as HTMLElement).offsetParent !== null);
    return roots.map((root) => {
      // Only what shows: a hidden tab's label is not on the default path.
      const hidden = Array.from(root.querySelectorAll('button')).map((b) => (b as HTMLElement).offsetParent === null);
      const c = root.cloneNode(true) as HTMLElement;
      const btns = Array.from(c.querySelectorAll('button'));
      hidden.forEach((h, i) => { if (h) btns[i]?.remove(); });
      c.querySelectorAll('.cv-post-body, .home-row-snip, .home-card-from, .home-draft-meta, .home-draft-body').forEach((n) => n.remove());
      return c.innerText;
    }).join('\n');
  });
  const hit = text.match(JARGON);
  expect(hit?.[0] ?? null, `jargon on the default path: "${hit?.[0]}" in: ${text.slice(Math.max(0, (hit?.index ?? 0) - 60), (hit?.index ?? 0) + 60)}`).toBeNull();
}

/** Chat, or one of its sections (ideas; goals, which is what is coming up),
 * shown below the requests when no conversation is open. */
export async function pane(page: Page, name: 'chat' | 'ideas' | 'goals') {
  await page.locator('#tab-home').click();
  await expect(page.locator('#home-view')).toBeVisible();
  if (name === 'chat') return;
  // Back out of a conversation (and its details, two levels): the sections
  // show only on the list. Retried, since the open thread redraws as it polls.
  await expect(async () => {
    const back = page.locator('.home-thread .home-back').first();
    if (await back.isVisible()) await back.click({ timeout: 2_000 });
    await expect(page.locator('.home-thread')).toHaveCount(0, { timeout: 1_000 });
  }).toPass({ timeout: 20_000 });
  const sec = page.locator(`#home-${name}`);
  await expect(sec).toBeVisible();
  await sec.scrollIntoViewIfNeeded();
}

/** Poll an API until `ok` holds (the ideas pass runs in the background). */
export async function until<T>(fn: () => Promise<T>, ok: (v: T) => boolean, ms = LIVE ? 600_000 : 120_000): Promise<T> {
  const end = Date.now() + ms;
  let v = await fn();
  while (!ok(v) && Date.now() < end) {
    await new Promise((r) => setTimeout(r, 1500));
    v = await fn();
  }
  return v;
}

/** The newest outcome whose title or request matches. */
export async function outcomeLike(request: APIRequestContext, re: RegExp) {
  const v = await (await request.get('/api/intent/outcomes')).json();
  return (v.outcomes || []).find((o: { title: string }) => re.test(o.title));
}
