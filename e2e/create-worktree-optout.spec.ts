// The create dialog's worktree box (#280). Left unchecked in a git directory,
// the create must send worktree:false, the explicit opt-out of group-level
// workspace isolation. Outside a git directory the box is not offered and the
// request says nothing about a worktree. The first draft of this read a
// variable local to another function and threw on exactly the unchecked case,
// so every create from a repository failed; pageerror catches that class.
import { test, expect } from './fixtures';
import * as path from 'path';

declare function openCreate(): void;
declare function _checkDirGit(dir: string): Promise<void>;
declare function submitCreate(): Promise<void>;

async function createBody(page: import('@playwright/test').Page, dir: string) {
  let body: any = null;
  // The default directory's check (started by openCreate) answers LAST, as it
  // did on ios-safari in CI: it must not undo the check for the chosen dir.
  await page.route(/\/api\/git-check\?/, async (route) => {
    const asked = new URL(route.request().url()).searchParams.get('dir');
    if (asked !== dir) await new Promise((r) => setTimeout(r, 800));
    await route.continue();
  });
  await page.route(/\/api\/sessions$/, async (route) => {
    if (route.request().method() !== 'POST') return route.continue();
    body = route.request().postDataJSON();
    // Stop here: the request is what is under test, and no worker is created.
    await route.fulfill({ status: 409, json: { error: 'e2e: create intercepted' } });
  });
  await page.evaluate(async (d) => {
    openCreate();
    (document.getElementById('create-name') as HTMLInputElement).value = 'e2e-wt-optout';
    (document.getElementById('create-dir') as HTMLInputElement).value = d;
    await _checkDirGit(d);
    (document.getElementById('create-worktree-enabled') as HTMLInputElement).checked = false;
    await submitCreate();
  }, dir);
  await expect.poll(() => body).not.toBeNull();
  await page.waitForTimeout(1000);   // let the delayed default-dir answer land
  return body;
}

test('an unchecked worktree box in a repository sends worktree:false', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).submitCreate === 'function');
  const repo = path.resolve(__dirname, '..');
  const body = await createBody(page, repo);
  expect(await page.locator('#create-worktree-field').isVisible(), 'the box is offered in a git directory').toBe(true);
  expect(body.worktree, JSON.stringify(body)).toBe(false);
  expect(errors, errors.join('\n')).toEqual([]);
});

test('outside a repository the box is not offered and no worktree is sent', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).submitCreate === 'function');
  const body = await createBody(page, '/');
  expect('worktree' in body, JSON.stringify(body)).toBe(false);
  expect(errors, errors.join('\n')).toEqual([]);
});
