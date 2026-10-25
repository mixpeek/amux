// node --test skills/chrome-cdp/scripts/profile-sync.test.mjs
//
// A rotating refresh-token login (Supabase, Auth0, MSAL, Okta) must never be
// copied from the owner's Chrome into an amux profile: two holders of one token
// sign each other out (flawless-footage.ts.app, 2026-10-20). These tests run the
// shipped capture() and syncSite() against a fake CDP connection.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';

// profile-sync writes its log under $HOME; keep the real one clean.
const home = mkdtempSync(join(tmpdir(), 'ps-test-'));
process.env.HOME = home;
const ps = await import('./profile-sync.mjs');

function fakeCdp({ href, ls, cookies }) {
  return {
    async send(method) {
      if (method === 'Runtime.evaluate') return { result: { value: JSON.stringify({ href, ls }) } };
      if (method === 'Storage.getCookies') return { cookies };
      throw new Error(`unexpected CDP call ${method}`);
    },
  };
}

const SB = ['sb-abcdefghijklmnop-auth-token', JSON.stringify({ access_token: 'a'.repeat(40), refresh_token: 'r'.repeat(20), expires_at: 1 })];
const posthog = { name: 'ph_phc_x_posthog', value: '1', domain: '.ts.app', path: '/', secure: false, httpOnly: false };

test('rotating session keys are recognised by name and by value', () => {
  assert.equal(ps.isRotatingSessionEntry(...SB), true);
  assert.equal(ps.isRotatingSessionEntry('@@auth0spajs@@::client::aud::scope', '{}'), true);
  assert.equal(ps.isRotatingSessionEntry('okta-token-storage', '{}'), true);
  assert.equal(ps.isRotatingSessionEntry('my-app-session', '{"refresh_token":"abcdefghijk"}'), true);
  assert.equal(ps.isRotatingSessionEntry('theme', 'dark'), false);
  assert.equal(ps.isRotatingSessionEntry('ph_phc_x_posthog', '{"distinct_id":"x"}'), false);
});

test('capture leaves the rotating token out and keeps the rest of localStorage', async () => {
  const cap = await ps.capture(fakeCdp({ href: 'https://flawless-footage.ts.app/x', ls: [SB, ['theme', 'dark']], cookies: [posthog] }));
  assert.deepEqual(cap.rotatingSessionKeys, [SB[0]]);
  assert.deepEqual(cap.localStorage, [['theme', 'dark']]);
});

test('a login that is only a rotating token is refused, with the sign-in instruction', async () => {
  const out = await ps.syncSite(null, fakeCdp({ href: 'https://flawless-footage.ts.app/x', ls: [SB], cookies: [posthog] }), 's',
    { profile: 'ethan-tubescience', reason: 'explicit save-profile', list: [] });
  assert.equal(out.verdict, 'cdp_sync_rotating_session_not_copied');
  assert.deepEqual(out.rotating_session_not_copied, [SB[0]]);
  assert.match(out.sign_in_instead, /amux browser start ethan-tubescience https:\/\/flawless-footage\.ts\.app --headed/);
  assert.equal(out.profile, undefined, 'nothing was imported into a profile');
  const logged = readFileSync(join(home, '.cache', 'cdp', 'profile-sync.log'), 'utf8');
  assert.match(logged, /cdp_sync_rotating_session_not_copied/);
});

test('a page with no rotating token reports none', async () => {
  const cap = await ps.capture(fakeCdp({ href: 'https://example.com/', ls: [['theme', 'dark']], cookies: [] }));
  assert.deepEqual(cap.rotatingSessionKeys, []);
  assert.deepEqual(cap.localStorage, [['theme', 'dark']]);
});
