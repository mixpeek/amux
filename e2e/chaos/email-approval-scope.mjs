#!/usr/bin/env node
// END-TO-END (governance PR 6): who may see and decide a held external email,
// and a standing bypass leaves a receipt.
//
// A private amux (own home, port and tmux socket) with its owner token on
// disk, and no mail account connected: nothing here can reach a provider. A
// held draft is released with "schedule for later", which records the human's
// decision without sending; the one-send-per-approval property is pinned by
// the handler test `held_email_is_scoped_to_the_proven_caller` with a mocked
// transport.
//   a. two workers' external sends are held; the diagnostic (GET
//      /api/contract/counters -> email_gate) reports mode and measured;
//   b. report (default): a worker still lists both drafts, and it is counted;
//   c. enforce: a worker lists only its own; another worker cannot withdraw
//      it; a worker cannot approve; a headerless list is refused; the owner
//      releases a draft, recorded as `owner-token`, verified, whatever
//      X-Amux-Approver claims;
//   d. a session on AMUX_EMAIL_EXTERNAL_EXEMPT skips the hold and leaves a
//      ledger row and a policy receipt naming the rule;
//   e. AMUX_EMAIL_APPROVAL_SCOPE=off: a worker lists both again.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/email-approval-scope.mjs
import fs from 'node:fs';
import path from 'node:path';
import {startAmux, waitFor} from './harness.mjs';

const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
  console.log('ok -', name);
};
const ownerToken = () => fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim();
const OWNER = () => ({Authorization: 'Bearer ' + ownerToken()});
const W = lane => ({'X-Amux-Session': lane});
const LIST = '/api/email/approvals';
const counters = async () => (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.email_gate || {};
const ids = r => (r.body.pending || []).map(d => d.id);
const hold = async lane => {
  const r = await amux.req('POST', '/api/email/send', {
    to: 'buyer@customer.example', subject: 'e2e ' + lane, body: 'held draft from ' + lane, force_new_thread: true,
  }, 15000, W(lane));
  check(`a. ${lane}'s external send is held`, r.status === 403 && /^apr_/.test(r.body.approval_id || ''), r);
  return r.body.approval_id;
};
const ledger = () => {
  const p = path.join(amux.home, 'logs', 'email-sent.jsonl');
  return fs.existsSync(p) ? fs.readFileSync(p, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse) : [];
};
const restart = async mode => {
  await amux.down();
  amux.env.AMUX_EMAIL_APPROVAL_SCOPE = mode;
  await amux.up();
};

let ok = false;
try {
  // a.
  const a1 = await hold('e2e-alpha');
  const b1 = await hold('e2e-beta');
  let c = await counters();
  check('a. report is the default and the diagnostic measured', c.mode === 'report' && c.measured === true, c);

  // b. Report.
  let r = await amux.req('GET', LIST, undefined, 10000, W('e2e-alpha'));
  check('b. report: a worker still lists both drafts', r.status === 200 && ids(r).includes(a1) && ids(r).includes(b1), r.body);
  c = await counters();
  check('b. ...and the out-of-scope view is counted', (c.decisions?.['list.would_refuse'] || 0) >= 1, c);

  // c. Enforce.
  await restart('enforce');
  check('c. restarted in enforce', (await counters()).mode === 'enforce');
  r = await amux.req('GET', LIST, undefined, 10000, W('e2e-alpha'));
  check('c. a worker lists only its own draft', r.status === 200 && ids(r).join() === a1, r.body);
  r = await amux.req('GET', LIST, undefined, 10000, W('e2e-beta'));
  check('c. the other worker lists only its own', r.status === 200 && ids(r).join() === b1, r.body);
  // The harness sends a JSON content type, so a bodiless POST carries `{}`.
  r = await amux.req('POST', `/api/email/reject/${a1}`, {}, 10000, W('e2e-beta'));
  check("c. a worker cannot withdraw another worker's draft", r.status === 403 && r.body.code === 'not_your_draft', r);
  r = await amux.req('POST', `/api/email/approve/${b1}`, {}, 10000, W('e2e-beta'));
  check('c. a worker cannot approve its own draft', r.status === 403 && r.body.code === 'worker_cannot_approve', r);
  r = await amux.req('GET', LIST, undefined, 10000, {});
  check('c. a headerless list is refused', r.status === 403 && r.body.code === 'unproven_caller', r);
  r = await amux.req('GET', LIST, undefined, 10000, OWNER());
  check('c. the owner lists both, both still pending', r.status === 200 && ids(r).includes(a1) && ids(r).includes(b1), r.body);
  const later = Date.now() / 1000 + 30 * 86400;
  r = await amux.req('POST', `/api/email/approve/${a1}`, {send_at: later}, 10000, {...OWNER(), 'X-Amux-Approver': 'someone-else'});
  check('c. the owner releases it (scheduled for later)', r.status === 200 && r.body.scheduled === true, r);
  const row = ledger().filter(x => x.approval_id === a1 && x.scheduled === true).pop();
  check('c. the ledger names the proven owner, verified, not the claimed name',
    row?.approved_by === 'owner-token' && row?.approver_verified === true, row);
  r = await amux.req('POST', `/api/email/reject/${b1}`, {}, 10000, W('e2e-beta'));
  check('c. a worker can still withdraw its own draft', r.status === 200 && r.body.rejected === true, r);
  c = await counters();
  check('c. refusals are counted', (c.decisions?.['list.refused'] || 0) >= 1 && (c.decisions?.['reject.refused'] || 0) >= 1, c);

  // d. A standing bypass.
  // The legacy list is read live from server.env (settings::effective_env).
  fs.appendFileSync(path.join(amux.home, 'server.env'), '\nAMUX_EMAIL_EXTERNAL_EXEMPT=e2e-gamma\n');
  r = await amux.req('POST', '/api/email/send', {
    to: 'buyer@customer.example', subject: 'e2e gamma', body: 'exempt', force_new_thread: true,
  }, 15000, W('e2e-gamma'));
  check('d. the exempt session is not held (no account, so the send itself fails)', !r.body.approval_id, r);
  const bypassRow = ledger().filter(x => x.hold_bypassed === true && x.session === 'e2e-gamma').pop();
  check('d. the ledger names the rule', bypassRow?.bypass_rule === 'AMUX_EMAIL_EXTERNAL_EXEMPT', bypassRow);
  const rec = await amux.req('GET', '/api/policy/receipts?limit=1000', undefined, 10000, OWNER());
  const bypass = (rec.body.items || []).filter(x => x.action === 'email_external_bypass');
  check('d. a policy receipt names the worker and the rule',
    bypass.some(x => x.actor === 'worker:e2e-gamma' && x.rule_id === 'email-external-exempt'), {status: rec.status, bypass});
  c = await counters();
  check('d. the bypass is counted', (c.bypass_uses?.AMUX_EMAIL_EXTERNAL_EXEMPT || 0) >= 1, c);
  await waitFor('bypass log line', () => fs.readFileSync(amux.serverLog, 'utf8').includes('email_hold_bypassed_by_rule'), 10000);

  // e. Off.
  const c1 = await hold('e2e-alpha');
  await hold('e2e-beta');
  await restart('off');
  r = await amux.req('GET', LIST, undefined, 10000, W('e2e-alpha'));
  // Re-queuing alpha's draft reuses its approval (AMUX-5240), which the owner
  // scheduled in c, so read both lists.
  const sessions = [...(r.body.pending || []), ...(r.body.scheduled || [])].map(d => d.session);
  check('e. off: a worker lists every worker\'s drafts again',
    r.status === 200 && sessions.includes('e2e-alpha') && sessions.includes('e2e-beta') && !!c1, r.body);
  ok = true;
} finally {
  const receipt = {scenario: 'email-approval-scope', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
