// Expanding a skill in the Skills tab shows example invocations inferred from
// its markdown (Ethan, 2026-10-10). Written examples (lines in the file that
// already invoke the skill) come from the real server; inferred ones come from
// the meta-task model, which CI does not have, so those cases stub only the
// examples endpoint and keep everything else real.
import { test, expect, Page } from './fixtures';

declare function switchView(view: string): void;

async function createSkill(page: Page, name: string, content: string) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._skillLoadExamples === 'function');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const r = await page.request.post(`/api/skills/${name}`, { headers: { Authorization: `Bearer ${token}` }, data: { content } });
  expect(r.status(), await r.text()).toBe(200);
}

async function expand(page: Page, name: string) {
  await page.evaluate(() => switchView('skills'));
  const card = page.locator('.skill-card', { hasText: `/${name}` }).first();
  await expect(card).toBeVisible();
  await card.locator('.btn[title="Expand"]').click();
  return card.locator('.skill-examples');
}

test('a line in the skill file that invokes it is shown as an example', async ({ page }, info) => {
  const name = `ex-written-${info.project.name}-${Date.now().toString(36)}`;
  await createSkill(page, name, `---\ndescription: Summarize a file\nargument-hint: [path]\n---\nRun it like:\n- \`/${name} README.md\`\n`);
  const box = await expand(page, name);
  await expect(box.locator('.skill-example-inv').first()).toHaveText(`/${name} README.md`, { timeout: 60_000 });
  await expect(box).toContainText('Written in the skill file');
});

test('inferred examples render with their purpose and copy', async ({ page }, info) => {
  const name = `ex-inferred-${info.project.name}-${Date.now().toString(36)}`;
  await page.route(/\/api\/skills\/[^/]+\/examples/, (r) => r.fulfill({ json: {
    name, written: [], cached: false, measured: true, n_considered: 1,
    inferred: [
      { invocation: `/${name} 264`, purpose: 'Merge one PR by number' },
      { invocation: `/${name} all PRs`, purpose: 'Work through every open PR' },
    ],
  } }));
  await createSkill(page, name, '---\ndescription: Merge PRs\n---\nbody\n');
  const box = await expand(page, name);
  await expect(box.locator('.skill-example-inv')).toHaveText([`/${name} 264`, `/${name} all PRs`]);
  await expect(box).toContainText('Work through every open PR');
  await box.locator('.skill-example').nth(1).getByRole('button', { name: 'Copy' }).click();
  await expect(page.locator('body')).toContainText(`Copied /${name} all PRs`);
});

test('when nothing can be inferred the reason is shown', async ({ page }, info) => {
  const name = `ex-none-${info.project.name}-${Date.now().toString(36)}`;
  await page.route(/\/api\/skills\/[^/]+\/examples/, (r) => r.fulfill({ json: {
    name, written: [], inferred: [], cached: false, measured: false, n_considered: 1,
    why_unmeasured: 'meta-task model call failed: no key',
  } }));
  await createSkill(page, name, '---\ndescription: x\n---\nbody\n');
  const box = await expand(page, name);
  await expect(box).toContainText('No examples yet: meta-task model call failed: no key');
});
