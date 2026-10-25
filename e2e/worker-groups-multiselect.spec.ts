// A worker's groups are a multiselect in the edit modal: each group is a chip
// that can be removed, existing groups are offered to add, a new name can be
// typed, and Save updates the worker at once (Ethan, 2026-10-20: "i can remove
// groups and it'll update. also make groups multiselect not just comma
// separated").
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';

async function serverTags(request: any, auth: any, name: string) {
  const d = await (await request.get('/api/sessions', { headers: auth })).json();
  return ((Array.isArray(d) ? d : d.sessions) || []).find((x: any) => x.name === name)?.tags;
}
const chips = (page: Page) => page.locator('#eg-tag-wrap .be-tag-chip');

test('groups: remove a chip, add an existing group and a new one, Save updates the worker', async ({ page, request }, info) => {
  test.setTimeout(150_000);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).editField === 'function' && typeof (window as any).fetchSessions === 'function');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
  const ui = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const stamp = `${info.project.name}-${Date.now()}`;
  const name = `grp-${stamp}`, other = `grp-other-${stamp}`;
  const [a, b, c] = [`alpha${Date.now()}`, `beta${Date.now()}`, `gamma${Date.now()}`];
  for (const [n, tags] of [[name, [a, b]], [other, [c]]] as const) {
    expect((await request.post('/api/sessions', { headers: auth, data: { name: n, dir: '/tmp', start: false, tags } })).status()).toBe(201);
  }
  try {
    await expect.poll(() => page.evaluate(async (n) => {
      await (window as any).fetchSessions();
      return (0, eval)('sessions').some((s: any) => s.name === n);
    }, name), { timeout: 90_000 }).toBe(true);

    const open = () => page.evaluate((n) => {
      const s = (0, eval)('sessions').find((x: any) => x.name === n);
      (window as any).editField(n, 'tags', (s.tags || []).join(', '));
    }, name);
    await open();
    await expect(page.locator('#edit-overlay')).toHaveClass(/active/);
    await expect(page.locator('#edit-input-wrap')).toBeHidden();
    await expect(chips(page)).toHaveText([`${a}×`, `${b}×`]);

    // Remove beta with its x; gamma (another worker's group) is offered.
    await chips(page).filter({ hasText: b }).locator('.be-tag-chip-remove').click();
    await expect(chips(page)).toHaveText([`${a}×`]);
    const gammaBtn = page.locator('#eg-tag-suggestions .be-tag-suggestion', { hasText: c });
    await expect(gammaBtn).toBeVisible({ timeout: 15_000 });
    await gammaBtn.click();
    await expect(chips(page)).toHaveText([`${a}×`, `${c}×`]);

    // A brand-new group by name, then Backspace takes it back off.
    await page.locator('#eg-tag-input').fill('brandnew');
    await expect(page.locator('#eg-tag-suggestions .be-tag-suggestion', { hasText: 'new group' })).toBeVisible();
    await page.locator('#eg-tag-input').press('Enter');
    await expect(chips(page)).toHaveText([`${a}×`, `${c}×`, 'brandnew×']);
    await page.locator('#eg-tag-input').press('Backspace');
    await expect(chips(page)).toHaveText([`${a}×`, `${c}×`]);

    await page.locator('#edit-overlay button', { hasText: /^Save$/ }).click();
    await expect(page.locator('#edit-overlay')).not.toHaveClass(/active/);
    // The page shows it at once, and the server stored exactly this list.
    await expect.poll(() => page.evaluate((n) => (0, eval)('sessions').find((x: any) => x.name === n).tags, name), { timeout: 15_000 }).toEqual([a, c]);
    await expect(page.locator('#toast')).toContainText('groups:');
    await expect.poll(() => serverTags(request, auth, name), { timeout: 15_000 }).toEqual([a, c]);

    // Reopening starts from the saved list; removing every group clears them.
    await open();
    await expect(chips(page)).toHaveText([`${a}×`, `${c}×`]);
    await chips(page).first().locator('.be-tag-chip-remove').click();
    await chips(page).first().locator('.be-tag-chip-remove').click();
    await expect(chips(page)).toHaveCount(0);
    await page.locator('#eg-tag-input').press('Enter');   // Enter on an empty field saves
    await expect(page.locator('#edit-overlay')).not.toHaveClass(/active/);
    await expect.poll(() => serverTags(request, auth, name), { timeout: 15_000 }).toEqual([]);
  } finally {
    for (const n of [name, other]) await cleanup('delete fixture worker', () => request.post(`/api/sessions/${encodeURIComponent(n)}/delete`,
      { headers: { ...auth, 'X-Amux-UI-Token': ui }, data: {} }));
  }
});
