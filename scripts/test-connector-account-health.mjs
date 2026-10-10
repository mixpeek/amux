// Exercise the actual dashboard account classifier without a browser/provider mock.
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
const source = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const start = source.indexOf('function _cxGoogleSvcState(');
const end = source.indexOf('// The four google-', start);
assert(start >= 0 && end > start);
const context = vm.createContext({ _CONN_LEG: { 'google-gmail': 'gmail' }, _connIsGoogle: c => c.id.startsWith('google-') });
vm.runInContext(source.slice(start, end), context);
context._connAccts = { accounts: [{ account: 'shared-label', needs_reauth: true,
  families: { 'fixture-oauth': 'needs_reauth', slack: 'ok', google: 'ok' },
  canary: { slack: { status: 'ok' }, gmail: { status: 'ok' } } }] };
assert.equal(context._connAccountsFor({ id: 'fixture-oauth' })[0].st[0], 'Expired');
assert.equal(context._connAccountsFor({ id: 'slack' })[0].st[0], 'Active');
assert.equal(context._connAccountsFor({ id: 'google-gmail' })[0].st[0], 'Active');
context._connAccts.accounts[0].families.google = 'needs_reauth';
assert.equal(context._connAccountsFor({ id: 'google-gmail' })[0].st[0], 'Expired');
console.log('PASS: revoked custom account offers reconnect without poisoning healthy grants sharing its label');
