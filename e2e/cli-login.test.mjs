// T-CLI-5: each CLI login adapter against recorded output of the CLI version the
// cloud image pins. A CLI update that changes its prompts or link fails here.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { CLI_LOGINS, parseLogin, stripAnsi } from '../crates/amux-dashboard/static/state/cli-login.mjs';

const fx = (name) => readFileSync(new URL(`./fixtures/cli-login/${name}`, import.meta.url), 'utf8');

test('claude 2.1.291 auth login: whole link from the OSC 8 hyperlink, waits for a pasted code', () => {
  // Recorded on an amux Cloud server (2026-10-23); code_challenge and state redacted.
  const r = parseLogin('claude', fx('claude-auth-login-2.1.291.txt'));
  assert.match(r.url, /^https:\/\/claude\.com\/cai\/oauth\/authorize\?code=true&client_id=/);
  assert.match(r.url, /redirect_uri=https%3A%2F%2Fplatform\.claude\.com%2Foauth%2Fcode%2Fcallback/);
  // The /login scopes: user:profile is what the plan-usage endpoint needs.
  assert.match(r.url, /scope=[^&]*user%3Aprofile/);
  assert.match(r.url, /&state=redacted$/, 'the link must be complete, not cut at a wrap');
  assert.equal(r.needsCode, true);
  assert.equal(r.code, null);
});

test('claude 2.1.291 auth login: its answer to a pasted code, success or refusal', () => {
  // The two lines the CLI printed on the sandbox (2026-10-24) after the code went in.
  const base = fx('claude-auth-login-2.1.291.txt');
  const waiting = parseLogin('claude', base);
  assert.equal(waiting.succeeded, false);
  assert.equal(waiting.rejected, false);
  const ok = parseLogin('claude', base + 'Login successful.\r\n');
  assert.deepEqual([ok.succeeded, ok.rejected], [true, false]);
  const refused = parseLogin('claude', base + 'Invalid code. Please make sure the full code was copied.\r\n');
  assert.deepEqual([refused.succeeded, refused.rejected], [false, true]);
  // A well-formed code the server refused (sandbox, 2026-10-25).
  const failed = parseLogin('claude', base + 'Login failed: Request failed with status code 400\r\n');
  assert.deepEqual([failed.succeeded, failed.rejected], [false, true]);
});

test('claude: the sign-in is /login, not an inference-only setup-token', () => {
  assert.match(CLI_LOGINS.claude.command, /^exec env BROWSER=false claude auth login --claudeai$/);
  assert.doesNotMatch(CLI_LOGINS.claude.command, /setup-token/);
  assert.deepEqual(CLI_LOGINS.claude.clearEnv, ['CLAUDE_CODE_OAUTH_TOKEN']);
  assert.equal(CLI_LOGINS.claude.waitForExit, true);
});

test('claude: a link wrapped by the terminal without a hyperlink is joined back together', () => {
  const r = parseLogin('claude', 'Use the url below to sign in\r\nhttps://claude.com/cai/oauth/authorize?code=true&client_id=a\r\nbc&state=xyz\r\n\r\nPaste code here if prompted >');
  assert.equal(r.url, 'https://claude.com/cai/oauth/authorize?code=true&client_id=abc&state=xyz');
});

test('codex 0.162.1: device link and one-time code, no pasted code', () => {
  const r = parseLogin('codex', fx('codex-0.162.1.txt'));
  assert.equal(r.url, 'https://auth.openai.com/codex/device');
  assert.equal(r.code, 'ABCD-EFGH1');
  assert.equal(r.needsCode, false);
});

test('gemini 0.64.0: Google link and the authorization-code prompt', () => {
  const r = parseLogin('gemini', fx('gemini-0.64.0.txt'));
  assert.match(r.url, /^https:\/\/accounts\.google\.com\/o\/oauth2\/v2\/auth\?redirect_uri=https%3A%2F%2Fcodeassist\.google\.com%2Fauthcode/);
  assert.match(r.url, /client_id=REDACTED$/);
  assert.equal(r.needsCode, true);
});

test('nothing is invented from unrelated output', () => {
  for (const p of Object.keys(CLI_LOGINS)) {
    assert.deepEqual(parseLogin(p, 'bash: command not found\r\n'), { url: null, code: null, needsCode: false, succeeded: false, rejected: false });
  }
});

test('done() reads the server identity model_auth shape', () => {
  const a = { claude: { subscription_token: false, oauth: false }, codex: { signed_in: true }, gemini: { signed_in: false } };
  assert.equal(CLI_LOGINS.claude.done(a), false);
  // A saved setup-token alone is not this sign-in: it reports no usage.
  assert.equal(CLI_LOGINS.claude.done({ claude: { subscription_token: true, oauth: false } }), false);
  assert.equal(CLI_LOGINS.claude.done({ claude: { subscription_token: false, oauth: true } }), true);
  assert.equal(CLI_LOGINS.codex.done(a), true);
  assert.equal(CLI_LOGINS.gemini.done(a), false);
  assert.equal(stripAnsi('\x1b[1mbold\x1b[0m\r\n'), 'bold\n');
});

test('gh 2.81.0 auth login --web: device link and one-time code, then its success line', () => {
  // Recorded with a throwaway GH_CONFIG_DIR and stdin closed (2026-10-25); code replaced.
  const base = fx('gh-auth-login-2.81.0.txt');
  const r = parseLogin('github', base);
  assert.equal(r.url, 'https://github.com/login/device');
  assert.equal(r.code, 'AB12-CD34');
  assert.equal(r.needsCode, false);
  assert.equal(r.succeeded, false);
  assert.equal(parseLogin('github', base + '✓ Authentication complete.\n✓ Logged in as tester\n').succeeded, true);
  // Not a model: done() reads identity.github, never model_auth.
  assert.equal(CLI_LOGINS.github.done({ claude: { oauth: true } }, { github: { signed_in: false } }), false);
  assert.equal(CLI_LOGINS.github.done({}, { github: { signed_in: true } }), true);
  assert.equal(CLI_LOGINS.github.notModel, true);
});
