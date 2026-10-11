// Skills tab sorts: Usage, Recently used, Recently created, Recently edited,
// Name (Ethan, 2026-10-11). Each test picks names whose alphabetical order
// is the reverse of the order the sort should produce, so a sort that silently
// fell back to Name fails.
import { test, expect, Page } from './fixtures';

declare function switchView(view: string): void;

async function boot(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._skillsSortSet === 'function');
  return page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
}

async function save(page: Page, token: string, name: string, desc: string) {
  const r = await page.request.post(`/api/skills/${name}`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { content: `---\ndescription: ${desc}\n---\nbody\n` },
  });
  expect(r.status(), await r.text()).toBe(200);
}

// The list shows 8 rows per section until "View all", so search for the
// test's own tag: only its rows render, whatever else the host has.
async function orderUnder(page: Page, sort: string, names: string[], tag: string) {
  await page.evaluate(() => switchView('skills'));
  await page.locator('#skills-search').fill(tag);
  await page.locator('#skills-sort').selectOption(sort);
  const all = await page.locator('.skill-card-name').allInnerTexts();
  return all.map((t) => t.replace(/^\//, '').trim()).filter((t) => names.includes(t));
}

test('Recently edited follows the last save; Recently created ignores edits', async ({ page }, info) => {
  const token = await boot(page);
  const tag = `${info.project.name}-${Date.now().toString(36)}-${info.repeatEachIndex}`;
  const a = `aa-sort-${tag}`, z = `zz-sort-${tag}`;
  await save(page, token, a, 'first');
  await page.waitForTimeout(1100);          // created is stored in whole seconds
  await save(page, token, z, 'second');
  await page.waitForTimeout(1100);          // coarse filesystem mtimes
  await save(page, token, z, 'second, edited');
  await page.waitForTimeout(1100);
  await save(page, token, a, 'first, edited last');
  const names = [a, z];
  await expect.poll(() => orderUnder(page, 'edited', names, tag)).toEqual([a, z]);
  // a was edited last, but z was CREATED last. A creation time read off the
  // command file would say a (temp + rename resets it on every save).
  expect(await orderUnder(page, 'created', names, tag)).toEqual([z, a]);
  await page.waitForTimeout(1100);
  await save(page, token, z, 'edited again');
  await expect.poll(() => orderUnder(page, 'edited', names, tag)).toEqual([z, a]);
  expect(await orderUnder(page, 'name', names, tag)).toEqual([a, z]);
});

test('Recently used and Recently created order by their timestamps, unknowns last', async ({ page }, info) => {
  const tag = `${info.project.name}-${Date.now().toString(36)}-${info.repeatEachIndex}`;
  const [a, m, z] = ['aa', 'mm', 'zz'].map((p) => `${p}-stub-${tag}`);
  await page.route(/\/api\/skills\/usage/, (r) => r.fulfill({ json: { measured: true, n_considered: 2, skills: {
    [z]: { n: 1, recent: 1, model: 0, user: 1, last_ts: 2_000_000_000 },
    [m]: { n: 9, recent: 9, model: 0, user: 9, last_ts: 1_900_000_000 },
  } } }));
  await page.route(/\/api\/skills(?:\?.*)?$/, (r) => r.fulfill({ json: [
    { name: a, description: 'never used', hint: '' },
    { name: m, description: 'old', hint: '', created_at: 1_700_000_000, modified_at: 1_700_000_000 },
    { name: z, description: 'new', hint: '', created_at: 1_800_000_000, modified_at: 1_800_000_000 },
  ] }));
  await boot(page);
  const names = [a, m, z];
  await expect.poll(() => orderUnder(page, 'recent', names, tag)).toEqual([z, m, a]);
  expect(await orderUnder(page, 'created', names, tag)).toEqual([z, m, a]);
  expect(await orderUnder(page, 'used', names, tag)).toEqual([m, z, a]);
  expect(await orderUnder(page, 'name', names, tag)).toEqual([a, m, z]);
  // The choice is remembered across a reload.
  await page.locator('#skills-sort').selectOption('created');
  await page.reload();
  await page.evaluate(() => switchView('skills'));
  await expect(page.locator('#skills-sort')).toHaveValue('created');
});
