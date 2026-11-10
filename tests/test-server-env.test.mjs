// A test server (chaos harness or e2e/serve-head.sh) must not inherit the
// developer's live credentials; see CREDENTIAL_NAME in e2e/chaos/harness.mjs.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { execFileSync } from 'node:child_process';
import { testServerEnv } from '../e2e/chaos/harness.mjs';

const live = {
  STRIPE_LIVE_SECRET_KEY: 'x', OPENAI_API_KEY: 'x', GRANOLA_API_KEY: 'x', GDRIVE_REFRESH_TOKEN: 'x',
  NETSUITE_LOGIN_PASSWORD: 'x', GOOGLE_APPLICATION_CREDENTIALS: 'x', TWILIO_ACCOUNT_SID: 'x', apple_key_id: 'x',
};
const keep = { PATH: '/usr/bin:/bin', HOME: '/h', LANG: 'C', SSH_AUTH_SOCK: '/s', KEYCHAIN_PROFILE: 'p', TOKENIZERS_PARALLELISM: 'x' };

test('the chaos harness drops every credential var and keeps the rest', () => {
  const env = testServerEnv({ ...live, ...keep, AMUX_TASK_GUARD: '1', TMUX: 't' });
  for (const k of Object.keys(live)) assert.ok(!(k in env), `${k} reached the test server`);
  for (const k of Object.keys(keep)) assert.equal(env[k], keep[k], `${k} was dropped`);
  assert.ok(!('AMUX_TASK_GUARD' in env) && !('TMUX' in env));
});

test('serve-head.sh drops the same credential vars (its committed loop, run as is)', () => {
  const src = fs.readFileSync(new URL('../e2e/serve-head.sh', import.meta.url), 'utf8');
  const loop = src.match(/^for _k in \$\(compgen -e\); do\n[\s\S]*?^done$/m);
  assert.ok(loop, 'serve-head.sh has no credential loop');
  const out = execFileSync('bash', ['-c', `_scrubbed=""\n${loop[0]}\nenv`], {
    env: { ...live, ...keep, AMUX_HOME: '/a' }, encoding: 'utf8',
  });
  const names = new Set(out.split('\n').map((l) => l.split('=')[0]));
  for (const k of Object.keys(live)) assert.ok(!names.has(k), `${k} reached the e2e server`);
  for (const k of [...Object.keys(keep), 'AMUX_HOME']) assert.ok(names.has(k), `${k} was dropped`);
});
