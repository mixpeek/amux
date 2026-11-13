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

export async function openHome(page: Page) {
  await page.goto('/');
  await expect(page.locator('#home-view')).toBeVisible({ timeout: 20_000 });
  await expect(page.locator('.tab-bar-outer')).toBeHidden();
}

/** Say something in Chat and land in its thread. */
export async function ask(page: Page, text: string) {
  if (await page.locator('#home-composer').isHidden()) await page.locator('#home-nav-chat').click();
  if (await page.locator('.home-back').isVisible()) await page.locator('.home-back').click();
  await page.locator('#home-input').fill(text);
  await page.locator('#home-send').click();
  await expect(page.locator('.home-thread-title')).toBeVisible({ timeout: LIVE ? 180_000 : 90_000 });
  await expect(page.locator('.home-receipt')).not.toBeEmpty();
}

/** Wait for a helper answer in the open thread that matches `re`. */
export async function reply(page: Page, re: RegExp) {
  await expect(page.locator('.home-msg.theirs .home-bubble').filter({ hasText: re }).first()).toBeVisible({ timeout: LIVE ? 600_000 : 60_000 });
}

/** The default path shows no machinery words. amux's own words are checked;
 * the family's messages and answers about them (a camp's "Session 2") are not. */
export async function noJargon(page: Page) {
  const text = await page.locator('#home-view').evaluate((root) => {
    const c = root.cloneNode(true) as HTMLElement;
    c.querySelectorAll('.home-bubble, .home-row-snip, .home-card-from').forEach((n) => n.remove());
    return c.innerText;
  });
  const hit = text.match(JARGON);
  expect(hit?.[0] ?? null, `jargon on the default path: "${hit?.[0]}" in: ${text.slice(Math.max(0, (hit?.index ?? 0) - 60), (hit?.index ?? 0) + 60)}`).toBeNull();
}

export async function pane(page: Page, name: 'chat' | 'ideas' | 'goals') {
  await page.locator('#home-nav-' + name).click();
  await expect(page.locator('#home-' + name)).toBeVisible();
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
