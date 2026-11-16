#!/usr/bin/env node
// END-TO-END (governance PR 11): what a worker may do comes from one resolver
// over the environment and group layers, and a refusal asks the owner.
//
// A private amux with two customer workers in group `customers`, each with its
// own folder, and a fake stored Google grant for x@ and y@:
//   wexus   folder customers/wexus, minted token, connectors scope -> x@ only
//   rothco  folder customers/rothco
// Checks:
//   a. the diagnostic (contract counters -> capabilities) reports report mode;
//   b. wexus reads its own folder;
//   c. report: wexus reading rothco's folder is allowed, counted would_refuse,
//      receipted, and files NO card;
//   d. GET /api/workers/{name}/capabilities: the owner sees wexus with its
//      sources; wexus sees itself; wexus is refused rothco's;
//   e. enforce: the same read is 403 capability_refused naming the grant, and
//      exactly one Needs-you card (ask_type access) is filed, even on repeat;
//   f. enforce: a connector mint for an account outside wexus's scope is still
//      refused and now files a connectors card;
//   g. a worker cannot grant itself AMUX_CAP_FOLDERS; the owner can, through
//      the scope API, and the next read is allowed;
//   h. AMUX_CAP_PUBLISH=none (set by the owner) refuses wexus's publish;
//   i. AMUX_CAPABILITIES=off takes the new folder check away.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/capabilities.mjs
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';
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
const X = 'x@e2e.invalid', Y = 'y@e2e.invalid';

// Folders and files.
// Outside amux's home: publishing refuses amux's own files before any capability check.
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-cap-'));
const dirs = {wexus: path.join(root, 'customers', 'wexus'), rothco: path.join(root, 'customers', 'rothco')};
for (const [w, d] of Object.entries(dirs)) {
  fs.mkdirSync(d, {recursive: true});
  fs.writeFileSync(path.join(d, 'lead.md'), `# ${w}\n`);
}
// Workers.
fs.mkdirSync(path.join(amux.home, 'sessions'), {recursive: true});
fs.mkdirSync(path.join(amux.home, 'worker-tokens'), {recursive: true});
for (const [w, d] of Object.entries(dirs)) {
  fs.writeFileSync(path.join(amux.home, 'sessions', w + '.env'), `CC_DIR=${d}\nCC_TAGS=customers\nCC_ARCHIVED=0\n`);
}
const token = crypto.randomBytes(24).toString('hex');
fs.writeFileSync(path.join(amux.home, 'worker-tokens', 'wexus.sha256'),
  crypto.createHash('sha256').update(token).digest('hex'), {mode: 0o600});
const AS = {'X-Amux-Session': 'wexus', 'X-Amux-Worker-Token': token};
// Fake grants, fresh: a mint never leaves the box.
const fam = path.join(amux.home, 'connectors', 'google');
fs.mkdirSync(fam, {recursive: true});
for (const [acct, tok] of [[X, 'FAKE-BEARER-X'], [Y, 'FAKE-BEARER-Y']]) {
  fs.writeFileSync(path.join(fam, acct + '.json'), JSON.stringify({
    token: tok, scopes: 'https://www.googleapis.com/auth/gmail.modify', expires_at: Date.now() / 1000 + 3600,
  }), {mode: 0o600});
}

const read = (file, headers = AS) => amux.req('GET', '/api/fs/read?path=' + encodeURIComponent(file), undefined, 10000, headers);
const caps = async () => (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.capabilities || {};
const cards = async () => {
  const r = await amux.req('GET', '/api/board', undefined, 10000, OWNER());
  const items = Array.isArray(r.body) ? r.body : (r.body.items || r.body.issues || []);
  return items.filter(c => (c.source === 'capabilities') || (c.creator === 'capabilities'));
};
const restart = async env => { await amux.down(); Object.assign(amux.env, env); await amux.up(); };
const rothcoLead = path.join(dirs.rothco, 'lead.md');

let ok = false;
try {
  const scoped = await amux.req('PUT', '/api/scope', {level: 'worker', name: 'wexus', capability: 'connectors',
    value: {'google-gmail': {enabled: true, account: X}}}, 10000, OWNER());
  check('setup: the owner scopes wexus to x@', scoped.status === 200, scoped);

  // a.
  let c = await caps();
  check('a. report is the default mode, measured', c.mode === 'report' && c.measured === true, c);

  // b.
  const own = await read(path.join(dirs.wexus, 'lead.md'));
  check('b. wexus reads its own folder', own.status === 200, own);

  // c.
  const cross = await read(rothcoLead);
  check('c. report: reading rothco\'s folder is still allowed', cross.status === 200, cross);
  c = await caps();
  check('c. and counted would_refuse', (c.counts || {})['folders.would_refuse'] >= 1, c);
  await waitFor('would-refuse receipt', async () => {
    const r = await amux.req('GET', '/api/policy/receipts?limit=1000', undefined, 10000, OWNER());
    return (r.body.items || []).some(x => x.action === 'capability_folders' && x.effect === 'would_refuse' && x.actor === 'worker:wexus');
  }, 10000);
  check('c. a receipt names wexus and the would_refuse effect', true);
  check('c. report files no card', (await cards()).length === 0, await cards());

  // d.
  const vOwner = await amux.req('GET', '/api/workers/wexus/capabilities', undefined, 10000, OWNER());
  check('d. the owner reads wexus\'s capabilities', vOwner.status === 200 && vOwner.body.measured === true
    && vOwner.body.folders.own === fs.realpathSync(dirs.wexus) && vOwner.body.outbound.value === 'held'
    && vOwner.body.publish.value === 'public' && vOwner.body.connectors.measured === true, vOwner.body);
  check('d. connectors come from the connectors scope', JSON.stringify(vOwner.body.connectors.entitled).includes(X), vOwner.body.connectors);
  const vSelf = await amux.req('GET', '/api/workers/wexus/capabilities', undefined, 10000, AS);
  check('d. wexus reads its own', vSelf.status === 200, vSelf);
  const vOther = await amux.req('GET', '/api/workers/rothco/capabilities', undefined, 10000, AS);
  check('d. wexus may not read rothco\'s', vOther.status === 403, vOther);

  // e.
  await restart({AMUX_CAPABILITIES: 'enforce'});
  check('e. restarted in enforce', (await caps()).mode === 'enforce');
  const e1 = await read(rothcoLead);
  check('e. reading rothco\'s folder is refused with the grant named', e1.status === 403
    && e1.body.code === 'capability_refused' && e1.body.capability === 'folders' && /AMUX_CAP_FOLDERS/.test(e1.body.grant), e1);
  await waitFor('one folders card', async () => (await cards()).some(x => (x.tags || []).includes('capability:folders')), 10000);
  await read(rothcoLead);
  await new Promise(r => setTimeout(r, 1500));
  const folderCards = (await cards()).filter(x => (x.tags || []).includes('capability:folders'));
  check('e. exactly one folders card, even on repeat', folderCards.length === 1, folderCards);
  check('e. it is a typed Needs-you access ask', folderCards[0].status === 'needsyou' && folderCards[0].ask_type === 'access'
    && /\?$/.test(folderCards[0].ask_question || ''), folderCards[0]);
  const e2 = await read(path.join(dirs.wexus, 'lead.md'));
  check('e. its own folder is still open', e2.status === 200, e2);

  // f.
  const f1 = await amux.req('POST', `/api/connectors/google-gmail/token?account=${encodeURIComponent(Y)}`, undefined, 10000, AS);
  check('f. a mint outside its connectors scope is refused', f1.status === 403 && !f1.body.access_token, f1);
  await waitFor('connectors card', async () => (await cards()).some(x => (x.tags || []).includes('capability:connectors')), 10000);
  check('f. and files a connectors card', true);

  // g.
  const g0 = await amux.req('PUT', '/api/scope', {level: 'worker', name: 'wexus', capability: 'env',
    value: {AMUX_CAP_FOLDERS: dirs.rothco}}, 10000, AS);
  check('g. wexus cannot grant itself a folder', g0.status >= 400, g0);
  const g1 = await amux.req('PUT', '/api/scope', {level: 'worker', name: 'wexus', capability: 'env',
    value: {AMUX_CAP_FOLDERS: dirs.rothco}}, 10000, OWNER());
  check('g. the owner grants it through the scope API', g1.status === 200, g1);
  const asks = JSON.parse(fs.readFileSync(path.join(amux.home, 'capability-asks.json'), 'utf8'));
  check('g. the grant cleared the open folders ask (a later refusal may ask again)',
    !Object.keys(asks).some(k => k.startsWith('wexus') && k.endsWith('folders')), asks);
  const g2 = await read(rothcoLead);
  check('g. the next read is allowed', g2.status === 200, g2);
  const vAfter = await amux.req('GET', '/api/workers/wexus/capabilities', undefined, 10000, OWNER());
  check('g. the view shows the grant and its layer', JSON.stringify(vAfter.body.folders.grants).includes('worker wexus'), vAfter.body.folders);

  // h.
  const h0 = await amux.req('PUT', '/api/scope', {level: 'worker', name: 'wexus', capability: 'env',
    value: {AMUX_CAP_PUBLISH: 'none'}}, 10000, OWNER());
  check('h. the owner sets AMUX_CAP_PUBLISH=none', h0.status === 200, h0);
  const h1 = await amux.req('POST', '/api/page-links', {kind: 'file', path: path.join(dirs.wexus, 'lead.md'), title: 't'}, 10000, AS);
  check('h. wexus\'s publish is refused', h1.status === 403 && h1.body.capability === 'publish', h1);

  // i.
  await amux.req('PUT', '/api/scope', {level: 'worker', name: 'wexus', capability: 'env',
    value: {AMUX_CAP_FOLDERS: null}}, 10000, OWNER());
  await restart({AMUX_CAPABILITIES: 'off'});
  const i1 = await read(rothcoLead);
  check('i. AMUX_CAPABILITIES=off takes the folder check away', i1.status === 200 && (await caps()).mode === 'off', i1);

  ok = true;
} finally {
  await amux.down();
  fs.rmSync(root, {recursive: true, force: true});
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length}));
  if (!ok) process.exitCode = 1;
}
