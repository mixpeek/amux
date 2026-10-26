// About > Pull from remote on a server where local amux work goes upstream as a pull
// request before it is replaced (AMUX_PULL_SENDS_LOCAL_AS_PR, every amux Cloud server).
// Ethan, 2026-10-25: local changes "need to be a PR complete and PR in the remote branch"
// before the pull overrides them.
//
// Mocked: POST /api/pull (the real one runs `amux contribute` against GitHub) and the
// `amux github login` terminal (recorded gh output). Real: the About window, the
// pull result, the Connect GitHub button and the sign-in window it opens.
import fs from 'node:fs';
import path from 'node:path';
import { test, expect, Page } from './fixtures';

const GH_LOGIN = fs.readFileSync(path.join(__dirname, 'fixtures/cli-login/gh-auth-login-2.81.0.txt'), 'utf8');
const PR = 'https://github.com/mixpeek/amux/pull/7';

async function pull(page: Page, reply: object) {
  await page.route('**/api/pull', r => r.request().method() === 'POST'
    ? r.fulfill({ json: reply }) : r.fallback());
  await page.goto('/');
  await page.evaluate(() => (window as any).openAbout());
  await page.locator('#pull-btn').click();
}

test('a pull that sent local work links the pull request', async ({ page }) => {
  await pull(page, { ok: true, output: 'Fetched origin/main.', contribute: { verdict: 'pr_opened', pr_url: PR } });
  const status = page.locator('#pull-status');
  await expect(status).toContainText('Your local changes were sent as a pull request:');
  await expect(status.locator('a')).toHaveAttribute('href', PR);
  await expect(status.locator('.pull-connect-github')).toHaveCount(0);
});

test('a pull refused for want of GitHub offers the GitHub sign-in, which shows its code', async ({ page }) => {
  const st = { created: 0, cmd: '', sent: false };
  await page.route('**/api/terminal/create', r => { st.created++; r.fulfill({ json: { id: 'e2e-gh' } }); });
  await page.route('**/api/terminal/e2e-gh/input', r => {
    st.cmd += Buffer.from(JSON.parse(r.request().postData() || '{}').data || '', 'base64').toString();
    r.fulfill({ json: { ok: true } });
  });
  await page.route('**/api/terminal/e2e-gh/output**', async r => {
    await new Promise(res => setTimeout(res, 300));
    // The output endpoint returns only bytes not yet read: the recording once, then nothing.
    const data = st.sent ? '' : GH_LOGIN; st.sent = true;
    r.fulfill({ json: { data: Buffer.from(data).toString('base64'), alive: true } });
  });
  await pull(page, { ok: false, needs_github: true, contribute: { verdict: 'github_not_connected' },
    output: '  GitHub is not connected, so the local work cannot be sent yet. Nothing was changed.' });
  const status = page.locator('#pull-status');
  await expect(status).toContainText('Nothing was changed.');
  await expect(status.locator('a')).toHaveCount(0);
  await status.locator('.pull-connect-github').click();
  await expect(page.locator('#apikey-setup-modal')).toBeVisible();
  await expect(page.locator('#model-login-code')).toHaveText('AB12-CD34', { timeout: 15000 });
  await expect(page.locator('#model-login-link')).toHaveAttribute('href', 'https://github.com/login/device');
  await expect(page.locator('#model-login-status')).toHaveText('Open the page, sign in, and enter the code below.');
  expect(st.created).toBe(1);
  expect(st.cmd).toContain('exec amux github login');
});
