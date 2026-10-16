// Google revokes a session whose cookies show up in a second browser, so the
// CDP login sync must never copy google.com or youtube.com (2026-10-15: three
// sign-outs of every Google account in Ethan's Chrome, each right after a sync).
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { isSessionBound, SESSION_BOUND_SITES } from '../skills/chrome-cdp/scripts/profile-sync.mjs';

test('google and youtube are session-bound, lookalikes are not', () => {
  for (const s of ['google.com', 'accounts.google.com', '.youtube.com']) assert.ok(isSessionBound(s), s);
  for (const s of ['github.com', 'notgoogle.com', 'google.com.evil.io']) assert.ok(!isSessionBound(s), s);
});

test('the CDP list matches the server list', () => {
  const rs = readFileSync(new URL('../crates/amux-server/src/integrations/browser_login_sync.rs', import.meta.url), 'utf8');
  const m = rs.match(/SESSION_BOUND_SITES: &\[&str\] = &\[([^\]]*)\]/);
  assert.ok(m, 'server list found');
  const server = [...m[1].matchAll(/"([^"]+)"/g)].map(x => x[1]);
  assert.deepEqual([...SESSION_BOUND_SITES].sort(), server.sort());
});

test('the CDP daemon checks the list before reading cookies', () => {
  const src = readFileSync(new URL('../skills/chrome-cdp/scripts/cdp.mjs', import.meta.url), 'utf8');
  const at = src.indexOf('if (isSessionBound(site))');
  assert.ok(at > 0 && at < src.indexOf("cdp.send('Storage.getCookies'", at), 'gate precedes the cookie read');
});
