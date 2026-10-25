// Settings > Account > usage: a provider that is not connected offers its sign-in
// right there (MSG-91014, 2026-10-21). Before this the row said "Connect account"
// with no way to connect.
//
// Mocked: the Claude CLI's terminal and Claude's part of /api/identity (the e2e host
// has no Claude account), and the usage report (the real one needs a live
// subscription). Real: the button, the sign-in window, clearing the old token in
// server.env, and the usage list refreshing when the sign-in completes.
import fs from 'node:fs';
import path from 'node:path';
import { test, expect, Page, allowUnusedRoute } from './fixtures';

test.describe.configure({ mode: 'serial' });

const STALE_TOKEN = 'sk-ant-oat01-' + 'e2eStaleSetupToken_0123456789';
// Recorded `claude auth login` output on an amux Cloud server (code_challenge and state redacted).
const AUTH_LOGIN = fs.readFileSync(path.join(__dirname, 'fixtures/cli-login/claude-auth-login-2.1.291.txt'), 'utf8');
const AUTH_URL = /https:\/\/claude\.com\/cai\/oauth\/authorize\?code=true&client_id=[^&]+&response_type=code&.*&state=redacted$/;

async function token(page: Page) {
  await page.waitForFunction(() => typeof (window as any)._AMUX_AUTH_TOKEN === 'string');
  const t = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  return t ? { Authorization: `Bearer ${t}` } : {};
}

function usage(connected: boolean) {
  return {
    providers: [
      connected
        ? { id: 'claude', label: 'Claude', available: true, metered: true,
            windows: [{ label: '5-hour session', used_percent: 12, remaining_percent: 88 }] }
        : { id: 'claude', label: 'Claude', available: false, cause: 'no_token',
            reason: 'No Claude subscription token on this host' },
      { id: 'codex', label: 'Codex', available: false, cause: 'no_token', reason: 'Not signed in' },
      { id: 'ollama', label: 'Ollama', available: true, metered: false },
      { id: 'muse', label: 'Muse Code', available: false, cause: 'account_quota_not_reported', reason: 'Not reported' },
    ],
  };
}

// The Claude CLI in a server terminal: the recorded link and prompt until a code is typed,
// then its answer to the code (`outcome`), and the process exits `exitAfterMs` later.
// Each new terminal is a new CLI waiting for a code.
type Outcome = 'success' | 'refused' | 'silent';
async function mockCli(page: Page, exitAfterMs = 0, outcome: Outcome = 'success') {
  const st = { codeTyped: 0, created: 0, deleted: 0, inputs: [] as string[] };
  const answer = { success: '\r\nLogin successful.\r\n', refused: '\r\nInvalid code. Please make sure the full code was copied.\r\n', silent: '' };
  await page.route('**/api/terminal/create', r => { st.created++; st.codeTyped = 0; r.fulfill({ json: { id: 'e2e-login' } }); });
  await page.route('**/api/terminal/e2e-login/input', async r => {
    const data = Buffer.from(JSON.parse(r.request().postData() || '{}').data || '', 'base64').toString();
    st.inputs.push(data);
    if (data.startsWith('e2e-code')) st.codeTyped = Date.now();
    r.fulfill({ json: { ok: true } });
  });
  await page.route('**/api/terminal/e2e-login/output**', async r => {
    await new Promise(res => setTimeout(res, 300));
    const answered = st.codeTyped > 0;
    const out = AUTH_LOGIN + (answered ? answer[outcome] : '');
    r.fulfill({ json: { data: Buffer.from(out).toString('base64'), alive: !(answered && Date.now() - st.codeTyped >= exitAfterMs) } });
  });
  await page.route('**/api/terminal/e2e-login', r => { if (r.request().method() === 'DELETE') st.deleted++; r.fulfill({ json: { ok: true } }); });
  return st;
}

// The real /api/identity, with Claude's login as the test says (the e2e host has no Claude account).
async function claudeIdentity(page: Page, oauth: () => boolean) {
  await page.route('**/api/identity', async r => {
    const res = await r.fetch(); const j = await res.json();
    j.model_auth = { ...(j.model_auth || {}), claude: { api_key: false, subscription_token: false, oauth: oauth() } };
    r.fulfill({ response: res, json: j });
  });
}

async function openUsage(page: Page) {
  await page.evaluate(() => { (window as any).toggleSettings(); (window as any)._settingsTab('account'); });
  await expect(page.locator('.usage-provider[data-provider="claude"]')).toBeVisible({ timeout: 15000 });
}

// Ethan, 2026-10-23: the cloud should have "the same usage login stuff as local". The row's
// sign-in is now Claude Code's own /login (`claude auth login`), which leaves a refreshable
// login with the usage scope, where `claude setup-token` left an inference-only token.
test('Claude: Sign in from the usage row runs /login, clears an old setup-token and the row shows usage', async ({ page }) => {
  let connected = false;
  await page.route('**/api/usage', r => r.fulfill({ json: usage(connected) }));
  const cli = await mockCli(page);
  await claudeIdentity(page, () => cli.codeTyped > 0);
  await page.goto('/');
  const h = await token(page);
  // A server that signed in the old way: a setup-token in server.env.
  expect((await page.request.patch('/api/settings/env', { headers: h, data: { CLAUDE_CODE_OAUTH_TOKEN: STALE_TOKEN } })).ok()).toBeTruthy();

  await openUsage(page);
  const row = page.locator('.usage-provider[data-provider="claude"]');
  await expect(row.locator('.usage-provider-status')).toHaveText('Connect account');
  await row.locator('summary').click();
  await expect(row).toContainText('No Claude subscription token on this host');
  const btn = row.locator('.usage-connect');
  await expect(btn).toHaveText('Sign in with Claude');
  // Only providers with a CLI sign-in get a button; one without says so.
  await page.locator('.usage-provider[data-provider="muse"] summary').click();
  await expect(page.locator('.usage-provider[data-provider="muse"] .usage-connect')).toHaveCount(0);
  await expect(page.locator('.usage-provider[data-provider="muse"] .usage-no-login')).toContainText('No dashboard sign-in for Muse Code yet');
  await expect(page.locator('.usage-provider[data-provider="ollama"] .usage-connect')).toHaveCount(0);

  await btn.click();
  await expect(page.locator('#apikey-setup-modal')).toBeVisible();
  await expect(page.locator('#model-login-status')).toHaveText('Open the page, sign in, then paste the code it shows you.', { timeout: 15000 });
  await expect(page.locator('#model-login-link')).toHaveAttribute('href', AUTH_URL);
  expect(cli.created).toBe(1);
  expect(cli.inputs[0]).toBe('exec env BROWSER=false claude auth login --claudeai\r');

  await page.locator('#model-login-paste').fill('e2e-code#e2e-state');
  connected = true;   // what the usage endpoint answers once the login is on the server
  await page.locator('#model-login-paste-wrap button').first().click();
  // Code and Enter are two writes (Claude Code reads one write as a paste).
  await expect.poll(() => cli.inputs.slice(1)).toEqual(['e2e-code#e2e-state', '\r']);

  // The CLI exits, the window closes, the old setup-token is gone (so workers use the
  // refreshable login), and the usage row refreshes without a reload.
  await expect(page.locator('#apikey-setup-modal')).toBeHidden({ timeout: 30000 });
  await expect(page.getByText('Claude is connected. Your workers can use it now.')).toBeVisible();
  await expect.poll(async () => (await (await page.request.get('/api/settings/env', { headers: h })).json()).CLAUDE_CODE_OAUTH_TOKEN || '').toBe('');
  await expect(row.locator('.usage-provider-status')).toHaveText('88% left', { timeout: 15000 });
  // Connected, it still offers the same sign-in, as a switch (MSG-92525).
  await expect(row.locator('.usage-connect')).toHaveText('Switch Claude account');
});

test('Claude: Switch account waits for the new login instead of closing at once', async ({ page }) => {
  await page.route('**/api/usage', r => r.fulfill({ json: usage(true) }));
  const cli = await mockCli(page, 4000);
  await claudeIdentity(page, () => true);     // signed in before, and after
  await page.goto('/');
  await openUsage(page);
  const row = page.locator('.usage-provider[data-provider="claude"]');
  await expect(row.locator('.usage-provider-status')).toHaveText('88% left');
  if (!(await row.locator('.usage-connect').isVisible())) await row.locator('summary').click();
  await row.locator('.usage-connect').click();
  await expect(page.locator('#model-login-status')).toHaveText('Open the page, sign in, then paste the code it shows you.', { timeout: 15000 });
  // Already signed in: the identity check alone must not end the switch.
  await page.waitForTimeout(4000);
  await expect(page.locator('#apikey-setup-modal')).toBeVisible();
  expect(cli.deleted).toBe(0);
  await page.locator('#model-login-paste').fill('e2e-code#e2e-state');
  await page.locator('#model-login-paste-wrap button').first().click();
  await expect(page.locator('#model-login-status')).toHaveText('Checking…');
  // It closes only after the CLI has exited with the new login written.
  await expect(page.locator('#apikey-setup-modal')).toBeHidden({ timeout: 30000 });
  expect(Date.now() - cli.codeTyped).toBeGreaterThanOrEqual(4000);
  await expect(page.getByText('Claude is connected. Your workers can use it now.')).toBeVisible();
});

// Sandbox, 2026-10-24: Claude refused a pasted code ("Invalid code") and stopped reading;
// the window sat at "Checking…" for minutes, then said "connected" because the old login
// was still there.
async function startSwitch(page: Page) {
  await page.route('**/api/usage', r => r.fulfill({ json: usage(true) }));
  await claudeIdentity(page, () => true);     // an older login stays in place throughout
  await page.goto('/');
  await openUsage(page);
  const row = page.locator('.usage-provider[data-provider="claude"]');
  if (!(await row.locator('.usage-connect').isVisible())) await row.locator('summary').click();
  await row.locator('.usage-connect').click();
  await expect(page.locator('#model-login-status')).toHaveText('Open the page, sign in, then paste the code it shows you.', { timeout: 15000 });
  await page.locator('#model-login-paste').fill('e2e-code#e2e-state');
  await page.locator('#model-login-paste-wrap button').first().click();
}

test('Claude: a refused code starts a new sign-in and says why', async ({ page }) => {
  const cli = await mockCli(page, 60000, 'refused');
  await startSwitch(page);
  await expect.poll(() => cli.created, { timeout: 15000 }).toBe(2);
  await expect(page.locator('#model-login-err')).toHaveText('Claude did not accept that code, so this is a new sign-in link. Copy the whole code from the page.');
  await expect(page.locator('#model-login-status')).toHaveText('Open the page, sign in, then paste the code it shows you.');
  await expect(page.locator('#model-login-link')).toHaveAttribute('href', AUTH_URL);
  expect(cli.deleted).toBeGreaterThanOrEqual(1);    // the refused CLI's terminal is closed
  await expect(page.getByText('Claude is connected. Your workers can use it now.')).toHaveCount(0);
});

test('Claude: a switch whose CLI exits without signing in is not reported as connected', async ({ page }) => {
  const cli = await mockCli(page, 0, 'silent');
  allowUnusedRoute(page, '**/api/terminal/e2e-login');   // the CLI has exited: no terminal is left to delete
  await startSwitch(page);
  await expect(page.locator('#model-login-status')).toHaveText('Claude sign-in stopped before finishing.', { timeout: 30000 });
  await expect(page.locator('#apikey-setup-modal')).toBeVisible();
  await expect(page.getByText('Claude is connected. Your workers can use it now.')).toHaveCount(0);
  expect(cli.created).toBe(1);
});

test('Codex: the usage row starts the ChatGPT device sign-in', async ({ page }) => {
  await page.route('**/api/usage', r => r.fulfill({ json: usage(false) }));
  const inputs: string[] = [];
  await page.route('**/api/terminal/create', r => r.fulfill({ json: { id: 'e2e-codex' } }));
  await page.route('**/api/terminal/e2e-codex/input', r => {
    inputs.push(Buffer.from(JSON.parse(r.request().postData() || '{}').data || '', 'base64').toString());
    r.fulfill({ json: { ok: true } });
  });
  await page.route('**/api/terminal/e2e-codex/output**', async r => {
    await new Promise(res => setTimeout(res, 300));
    // The recorded output of the codex version the cloud image pins.
    const out = fs.readFileSync(path.join(__dirname, 'fixtures/cli-login/codex-0.162.1.txt'));
    r.fulfill({ json: { data: out.toString('base64'), alive: true } });
  });
  await page.goto('/');
  await openUsage(page);
  const row = page.locator('.usage-provider[data-provider="codex"]');
  await row.locator('summary').click();
  await row.locator('.usage-connect').click();
  await expect(page.locator('#apikey-setup-modal')).toBeVisible();
  await expect(page.locator('#model-login-code')).toHaveText('ABCD-EFGH1', { timeout: 15000 });
  await expect(page.locator('#model-login-link')).toHaveAttribute('href', 'https://auth.openai.com/codex/device');
  expect(inputs[0]).toContain('codex login --device-auth');
});

// Ethan, 2026-10-21 (MSG-92525): "the same cli login flow for each". Gemini on an
// API key reads "Not reported" and had no button; a signed-in Codex whose usage
// reply was empty read "Connect account".
test('every provider with a CLI sign-in offers it, whatever its usage state', async ({ page }) => {
  await page.route('**/api/usage', r => r.fulfill({ json: { providers: [
    { id: 'claude', label: 'Claude', available: false, cause: 'account_quota_not_reported', signed_in: true, reason: 'Signed in with a Claude subscription token.' },
    { id: 'codex', label: 'Codex', available: false, cause: 'unexpected_shape', reason: 'Codex account usage returned no rate-limit snapshot.' },
    { id: 'gemini', label: 'Gemini', available: false, cause: 'account_quota_not_reported', reason: 'This Gemini authentication mode does not report an account-wide remaining subscription quota' },
    { id: 'ollama', label: 'Ollama', available: true, metered: false },
    { id: 'grok', label: 'Grok', available: true, metered: true, usage_unknown: true },
  ] } }));
  let created = 0;
  await page.route('**/api/terminal/create', r => { created++; r.fulfill({ json: { id: 'e2e-gemini' } }); });
  const typed: string[] = [];
  await page.route('**/api/terminal/e2e-gemini/input', r => { typed.push(Buffer.from(JSON.parse(r.request().postData() || '{}').data || '', 'base64').toString()); r.fulfill({ json: { ok: true } }); });
  await page.route('**/api/terminal/e2e-gemini/output**', async r => { await new Promise(res => setTimeout(res, 300)); r.fulfill({ json: { data: '', alive: true } }); });
  await page.route('**/api/terminal/e2e-gemini', r => r.fulfill({ json: { ok: true } }));
  allowUnusedRoute(page, '**/api/terminal/e2e-gemini');   // the window stays open
  await page.goto('/');
  await openUsage(page);
  const label = (id: string) => page.locator(`.usage-provider[data-provider="${id}"] .usage-connect`);
  // A subscription-token sign-in reports no usage and is still an account (live, usage-a759).
  await expect(page.locator('.usage-provider[data-provider="claude"] .usage-provider-status')).toHaveText('Not reported');
  await expect(label('claude')).toHaveText('Switch Claude account');
  await expect(page.locator('.usage-provider[data-provider="codex"] .usage-provider-status')).toHaveText('Usage not reported');
  await expect(label('codex')).toHaveText('Switch ChatGPT account');
  await expect(page.locator('.usage-provider[data-provider="gemini"] .usage-provider-status')).toHaveText('Not reported');
  await expect(label('gemini')).toHaveText('Sign in with Google (Gemini)');
  await expect(label('ollama')).toHaveCount(0);
  await expect(page.locator('.usage-provider[data-provider="ollama"] .usage-no-login')).toHaveCount(0);
  await expect(page.locator('.usage-provider[data-provider="grok"] .usage-no-login')).toContainText('No dashboard sign-in for Grok yet');
  await page.locator('.usage-provider[data-provider="gemini"] summary').click();
  await label('gemini').click();
  await expect(page.locator('#apikey-setup-modal')).toBeVisible();
  await expect.poll(() => created).toBe(1);
  await expect.poll(() => typed.join('')).toContain('gemini');
  await expect(page.locator('#model-login-status')).toHaveText(/Google \(Gemini\)/, { timeout: 15000 });
});
