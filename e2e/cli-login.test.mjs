// T-CLI-5: each CLI login adapter against recorded output of the CLI version the
// cloud image pins. A CLI update that changes its prompts or link fails here.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { CLI_LOGINS, parseLogin, stripAnsi } from '../crates/amux-dashboard/static/state/cli-login.mjs';

const fx = (name) => readFileSync(new URL(`./fixtures/cli-login/${name}`, import.meta.url), 'utf8');

test('claude 2.1.299: whole link from the OSC 8 hyperlink, waits for a pasted code', () => {
  const r = parseLogin('claude', fx('claude-2.1.299.txt'));
  assert.match(r.url, /^https:\/\/claude\.com\/cai\/oauth\/authorize\?code=true&client_id=/);
  assert.match(r.url, /redirect_uri=https%3A%2F%2Fplatform\.claude\.com%2Foauth%2Fcode%2Fcallback/);
  assert.match(r.url, /&state=/, 'the link must be complete, not cut at a wrap');
  assert.equal(r.needsCode, true);
  assert.equal(r.code, null);
  assert.equal(r.token, null);
});

test('claude: the printed token is captured and ends the wait for a code', () => {
  const raw = fx('claude-2.1.299.txt') + '\r\n\x1b[32mYour OAuth token (valid for 1 year):\x1b[39m\r\n\r\nsk-ant-oat01-' + 'A'.repeat(40) + '\r\n';
  const r = parseLogin('claude', raw);
  assert.equal(r.token, 'sk-ant-oat01-' + 'A'.repeat(40));
  assert.equal(r.needsCode, false);
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
    assert.deepEqual(parseLogin(p, 'bash: command not found\r\n'), { url: null, code: null, needsCode: false, token: null });
  }
});

test('done() reads the server identity model_auth shape', () => {
  const a = { claude: { subscription_token: false, oauth: false }, codex: { signed_in: true }, gemini: { signed_in: false } };
  assert.equal(CLI_LOGINS.claude.done(a), false);
  assert.equal(CLI_LOGINS.codex.done(a), true);
  assert.equal(CLI_LOGINS.gemini.done(a), false);
  assert.equal(stripAnsi('\x1b[1mbold\x1b[0m\r\n'), 'bold\n');
});
