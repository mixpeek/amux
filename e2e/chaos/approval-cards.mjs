#!/usr/bin/env node
// END-TO-END (governance PR 12): every approval waits on a person as ONE
// Needs-you card, and deciding the card runs the item's own approve or reject.
//
// A private amux with two customer workers (wexus, rothco, group customers),
// an owner-added vault card (rule: approval above $100), and no mail account,
// so nothing reaches a provider (a held email is released with "schedule for
// later", which records the decision without sending).
//   a. a held external email, a spend grant, a pending publish, an owner-gated
//      land request and (enforce) a capability ask each file exactly one card
//      carrying `approval_ref`; the email's card folds into its triage item;
//   b. the board refuses moving an undecided approval card to todo, and names
//      how to decide it;
//   c. the creating worker cannot decide (decide endpoint or board PATCH);
//   d. a member scoped to rothco cannot decide wexus's spend;
//   e. the owner decides each through the card: email (scheduled), spend
//      (approve and remember), publish (board PATCH status=done), capability
//      (approve writes the grant, the next read is allowed), land (reject);
//      each card closes, and GET /api/approvals/cards names the decider from
//      the ledger, verified;
//   f. the remembered spend answers the next identical charge with no grant
//      and no card, and the use is logged; a larger charge still asks;
//      revoking it makes the same charge ask again;
//   g. AMUX_APPROVAL_CARDS=off files no card for a new held email.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/approval-cards.mjs
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import https from 'node:https';
import crypto from 'node:crypto';
import {startAmux, waitFor} from './harness.mjs';

const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0', AMUX_NEEDS_INPUT_AUTO: '0',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail).slice(0, 2000));
  console.log('ok -', name);
};
const ownerToken = () => fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim();
const OWNER = () => ({Authorization: 'Bearer ' + ownerToken()});
// The owner signed in to the dashboard: a publish is approved only by a person
// with a session (governance PR 5), not by the bearer alone.
const PERSON = () => ({...OWNER(), Cookie: '__Host-amux_owner=' + crypto.createHash('sha256').update('amux-owner-session:' + ownerToken()).digest('hex')});

// Workers, folders, tokens.
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-appr-'));
const dirs = {wexus: path.join(root, 'customers', 'wexus'), rothco: path.join(root, 'customers', 'rothco')};
fs.mkdirSync(path.join(amux.home, 'sessions'), {recursive: true});
fs.mkdirSync(path.join(amux.home, 'worker-tokens'), {recursive: true});
const tokens = {};
for (const [w, d] of Object.entries(dirs)) {
  fs.mkdirSync(d, {recursive: true});
  fs.writeFileSync(path.join(d, 'lead.md'), `# ${w}\n`);
  fs.writeFileSync(path.join(amux.home, 'sessions', w + '.env'), `CC_DIR=${d}\nCC_TAGS=customers\nCC_ARCHIVED=0\n`);
  tokens[w] = crypto.randomBytes(24).toString('hex');
  fs.writeFileSync(path.join(amux.home, 'worker-tokens', w + '.sha256'),
    crypto.createHash('sha256').update(tokens[w]).digest('hex'), {mode: 0o600});
}
const AS = w => ({'X-Amux-Session': w, 'X-Amux-Worker-Token': tokens[w]});

const board = async () => {
  const r = await amux.req('GET', '/api/board', undefined, 10000, OWNER());
  return Array.isArray(r.body) ? r.body : (r.body.items || r.body.issues || []);
};
const cardsOf = async kind => (await board()).filter(c => (c.tags || []).includes('approval:' + kind));
const links = async (open = false) => (await amux.req('GET', '/api/approvals/cards' + (open ? '?open=1' : ''), undefined, 10000, OWNER())).body;
const linkOf = async ref => ((await links()).items || []).find(x => x.approval_ref === ref);
const decide = (ref, decision, headers, extra = {}) =>
  amux.req('POST', '/api/approvals/decide', {approval_ref: ref, decision, ...extra}, 15000, headers);
const restart = async env => { await amux.down(); Object.assign(amux.env, env); await amux.up(); };
const holdEmail = async (w, subject) => {
  const r = await amux.req('POST', '/api/email/send', {
    to: 'buyer@customer.example', subject, body: 'held draft', force_new_thread: true,
  }, 15000, AS(w));
  return r.body.approval_id;
};
const fill = (item, amount, merchant) => amux.req('POST', `/api/vault/${item}/fill`,
  {amount_usd: amount, merchant, purpose: 'e2e charge'}, 15000, AS('wexus'));
// A form POST (the invite accept is a browser form) that returns the Set-Cookie.
const formPost = (p, form) => new Promise((resolve, reject) => {
  const data = Buffer.from(new URLSearchParams(form).toString());
  const r = https.request(amux.base + p, {method: 'POST', rejectUnauthorized: false, timeout: 15000,
    headers: {'Content-Type': 'application/x-www-form-urlencoded', 'Content-Length': data.length}}, res => {
    res.resume(); res.on('end', () => resolve({status: res.statusCode, headers: res.headers}));
  });
  r.on('error', reject); r.write(data); r.end();
});

let ok = false;
try {
  // a. Five kinds, one card each.
  const apr = await holdEmail('wexus', 'e2e approval card');
  check('a. the external email is held', /^apr_/.test(apr || ''), apr);
  await waitFor('email card', async () => (await cardsOf('email')).length === 1, 10000);
  const card = await amux.req('POST', '/api/vault', {name: 'Test Mastercard',
    fields: {number: '5555555555554444', exp: '09/28', cvc: '123', name: 'Test Holder', zip: '10001'}}, 10000, OWNER());
  check('setup: the owner adds a card with the $100 rule', card.status === 201, card.body);
  const item = card.body.item.id;
  const g1 = await fill(item, 250, 'Test Shop');
  check('a. a $250 charge needs approval', g1.status === 403 && g1.body.requires_approval === true && g1.body.grant_id, g1.body);
  const grant1 = g1.body.grant_id;
  await waitFor('grant card', async () => (await cardsOf('grant')).length === 1, 10000);
  const pub = await amux.req('POST', '/api/page-links', {kind: 'file', path: path.join(dirs.wexus, 'lead.md'), title: 'lead'}, 10000, AS('wexus'));
  check('a. a worker publish is pending', pub.status === 201 && pub.body.pending === true, pub.body);
  await waitFor('publish card', async () => (await cardsOf('publish')).length === 1, 10000);
  const patchId = crypto.randomBytes(20).toString('hex');
  const land = await amux.req('POST', '/api/land/approvals/request', {patch_id: patchId, files: ['scripts/review-tiers.txt'],
    diffstat: '1 file changed', subjects: ['e2e']}, 10000, AS('wexus'));
  check('a. an owner-gated land is requested with a card', land.status === 200 && land.body.card, land.body);
  await restart({AMUX_CAPABILITIES: 'enforce'});
  const cross = await amux.req('GET', '/api/fs/read?path=' + encodeURIComponent(path.join(dirs.rothco, 'lead.md')), undefined, 10000, AS('wexus'));
  check('a. enforce: reading rothco\'s folder is refused', cross.status === 403 && cross.body.code === 'capability_refused', cross);
  await waitFor('capability card linked', async () => !!(await linkOf('capability:wexus:folders')), 10000);
  await holdEmail('wexus', 'e2e approval card');  // the same draft again
  await new Promise(r => setTimeout(r, 1000));
  check('a. a re-requested draft files no second card', (await cardsOf('email')).length === 1, await cardsOf('email'));
  const open = (await links(true)).items || [];
  const refs = open.map(x => x.approval_ref).sort();
  const want = [`email:${apr}`, `grant:${grant1}`, `publish:${pub.body.id}`, `land:${patchId}`, 'capability:wexus:folders'].sort();
  check('a. five open approvals, one per item, each linked to a card', JSON.stringify(refs) === JSON.stringify(want)
    && open.every(x => x.card), {refs, want});
  const q = (await amux.req('GET', '/api/needs-input', undefined, 10000, OWNER())).body.items || [];
  const emailItem = q.find(x => x.kind === 'email' && x.approval_id === apr);
  const emailCard = (await cardsOf('email'))[0].id;
  check('a. the email\'s card folds into its triage item (one ask, not two)',
    emailItem?.approval?.ref === `email:${apr}` && emailItem.card === emailCard && !q.some(x => x.kind === 'card' && x.card === emailCard), {emailItem});
  check('a. the spend card carries its approval and expires with the grant',
    q.find(x => x.approval?.ref === `grant:${grant1}`)?.approval?.expires_at > Date.now() / 1000, q.map(x => x.approval));

  // b. The board refuses a move that is not a decision.
  const grantCard = (await cardsOf('grant'))[0].id;
  const mv = await amux.req('PATCH', `/api/board/${grantCard}`, {status: 'todo'}, 10000, OWNER());
  check('b. an undecided approval card cannot be moved to todo', mv.status === 409 && mv.body.code === 'approval_card'
    && mv.body.approve?.body?.approval_ref === `grant:${grant1}`, mv);

  // c. The creating worker never decides.
  let r = await decide(`grant:${grant1}`, 'approve', AS('wexus'));
  check('c. wexus cannot approve its own spend', r.status === 403, r);
  r = await amux.req('PATCH', `/api/board/${grantCard}`, {status: 'done'}, 10000, AS('wexus'));
  check('c. nor close the card on the board', r.status === 403, r);
  check('c. the grant is still pending', fs.existsSync(path.join(amux.home, 'grants', grant1 + '.json')));

  // d. A member scoped to rothco.
  const inv = await amux.req('POST', '/api/org/invites', {email: 'ann@e2e.invalid', scope_level: 'worker', scope_name: 'rothco'}, 10000, OWNER());
  check('d. the owner invites a member scoped to rothco', inv.status === 201 || inv.status === 200, inv.body);
  const tok = inv.body.token || String(inv.body.url || '').split('/invite/')[1];
  const acc = await formPost(`/invite/${tok}`, {email: 'ann@e2e.invalid', name: 'Ann'});
  const cookie = (acc.headers['set-cookie'] || []).map(c => c.split(';')[0]).find(c => c.startsWith('amux_member='));
  check('d. the member accepts and holds a cookie', !!cookie, acc);
  r = await decide(`grant:${grant1}`, 'approve', {Cookie: cookie});
  check('d. a member outside wexus\'s scope cannot decide its spend', r.status === 403, r);

  // e. The owner decides each through its card.
  const later = Date.now() / 1000 + 30 * 86400;
  r = await decide(`email:${apr}`, 'approve', OWNER(), {body: {send_at: later}});
  check('e. email: approved from its card (scheduled, nothing sent)', r.status === 200 && r.body.scheduled === true, r);
  r = await decide(`grant:${grant1}`, 'approve', OWNER(), {remember: {}});
  check('e. spend: approved and remembered', r.status === 200 && r.body.ok === true && Number(r.body.standing_approval) > 0, r);
  const sa = r.body.standing_approval;
  const pubCard = (await cardsOf('publish'))[0].id;
  r = await amux.req('PATCH', `/api/board/${pubCard}`, {status: 'done'}, 10000, OWNER());
  check('e. publish: the board runs the link\'s own approve, which still wants a signed-in person', r.status === 403, r);
  r = await amux.req('PATCH', `/api/board/${pubCard}`, {status: 'done'}, 10000, PERSON());
  check('e. publish: status=done on the board, signed in, approves the link itself', r.status === 200, r);
  const links1 = await amux.req('GET', '/api/page-links', undefined, 10000, OWNER());
  const pl = (Array.isArray(links1.body) ? links1.body : (links1.body.links || links1.body.items || [])).find(l => l.id === pub.body.id);
  check('e. ...the link is approved by the owner', pl && pl.pending === false, pl);
  r = await decide('capability:wexus:folders', 'approve', OWNER());
  check('e. capability: approve writes the grant', r.status === 200, r);
  const again = await amux.req('GET', '/api/fs/read?path=' + encodeURIComponent(path.join(dirs.rothco, 'lead.md')), undefined, 10000, AS('wexus'));
  check('e. ...and the next read is allowed', again.status === 200, again);
  r = await decide(`land:${patchId}`, 'reject', OWNER(), {reason: 'not this diff'});
  check('e. land: rejected from its card', r.status === 200 && r.body.rejected === true, r);
  const landRec = JSON.parse(fs.readFileSync(path.join(amux.home, 'land-approvals.json'), 'utf8'))[patchId];
  check('e. ...recorded on the land record, so the gate keeps refusing it', !!landRec?.rejected_at && !landRec.approved_at, landRec);
  await waitFor('cards closed', async () => {
    const all = (await links()).items || [];
    return want.every(ref => all.find(x => x.approval_ref === ref)?.decided_at);
  }, 15000);
  const all = (await links()).items || [];
  const st = {};
  for (const c of await board()) st[c.id] = c.status;
  for (const ref of want) {
    const l = all.find(x => x.approval_ref === ref);
    const expect = ref.startsWith('land:') ? 'discarded' : 'done';
    check(`e. ${ref.split(':')[0]}: card ${expect}, decided by a verified person from the ledger`,
      st[l.card] === expect && l.decision === (expect === 'done' ? 'approved' : 'rejected')
      && l.decider_measured === true && l.approver_verified === true && ['owner', 'owner-token'].includes(l.decided_by), {l, status: st[l.card]});
  }

  // f. The remembered spend.
  const retry = await fill(item, 250, 'Test Shop');
  check('f. the approved charge passes the gate once (the browser fill itself has no page here)',
    retry.body.requires_approval !== true, retry.body);
  const next = await fill(item, 200, 'Test Shop');
  check('f. the next charge at that merchant within the amount needs no grant', next.body.requires_approval !== true, next.body);
  const bigger = await fill(item, 900, 'Test Shop');
  check('f. a larger charge still asks', bigger.status === 403 && bigger.body.requires_approval === true, bigger.body);
  const uses = await amux.req('GET', '/api/approvals/standing/uses', undefined, 10000, OWNER());
  const useRows = JSON.stringify(uses.body);
  check('f. the standing approval\'s use is logged', uses.status === 200 && useRows.includes('"approval"') && useRows.includes('wexus'), uses.body);
  const audit = fs.readFileSync(path.join(amux.home, 'logs', 'vault-audit.jsonl'), 'utf8');
  check('f. the vault audit names the standing approval', audit.includes('"decision":"standing_approval"'));
  const rv = await amux.req('DELETE', `/api/approvals/standing/${sa}`, undefined, 10000, OWNER());
  check('f. the owner revokes it', rv.status < 300, rv);
  const after = await fill(item, 200, 'Test Shop');
  check('f. after revoking, the same charge asks again', after.status === 403 && after.body.requires_approval === true, after.body);
  await waitFor('a new grant card', async () => (await cardsOf('grant')).length === 3, 10000);

  // g. Off.
  await restart({AMUX_APPROVAL_CARDS: 'off'});
  const before = (await cardsOf('email')).length;
  const apr2 = await holdEmail('rothco', 'e2e off');
  await new Promise(r2 => setTimeout(r2, 1500));
  check('g. off: the email is still held, and no card is filed', /^apr_/.test(apr2 || '') && (await cardsOf('email')).length === before);
  const c = (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.approval_cards || {};
  check('g. the diagnostic reports off, measured', c.mode === 'off' && c.measured === true, c);
  ok = true;
} finally {
  await amux.down();
  fs.rmSync(root, {recursive: true, force: true});
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length}));
  if (!ok) process.exitCode = 1;
}
