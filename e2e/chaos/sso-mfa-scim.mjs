#!/usr/bin/env node
// END-TO-END (governance PR 20): single sign-on (OIDC), a second factor
// (TOTP) and SCIM provisioning, against a private amux (own home, port and
// tmux socket) and a mock OpenID provider written here. Checks:
//   a. SSO: the flow is authorization code + PKCE S256 with state and nonce;
//      an invited member signs in and the cookie names them; the state is
//      good once; a wrong nonce, a forged signature, an unverified email,
//      a stranger and a provider error are each refused with no session;
//      a just-in-time domain makes a member with no access; an `amr` of mfa
//      marks the session; every sign-in and refusal is in the ledger;
//   b. SSO required: report counts an invite session; enforce refuses it
//      (API 403, page to /sso/login), lets the SSO session through, and
//      never touches the owner's bearer (break glass);
//   c. MFA: a member enrols (the secret is sealed, recovery codes hashed);
//      with enforce, an SSO session with no factor is refused until a code
//      is shown; a used code is refused; a recovery code works once;
//   d. step-up: report counts; enforce refuses the kill switch and a
//      capability change without a fresh factor and allows them with one;
//      a step-up token is the owner's alone;
//   e. SCIM: no token is 401; the owner issues one (hash kept); create, find
//      by filter, group membership; deactivate offboards (her SSO session
//      stops, sign-in refused, offboard rows by policy:scim); reactivate lets
//      her sign in again; delete is 404 after; a revoked token is 401;
//   f. counters for sso, mfa and scim say what ran.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/sso-mfa-scim.mjs
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import path from 'node:path';
import crypto from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {startAmux} from './harness.mjs';

const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail).slice(0, 2000));
  console.log('ok -', name);
};
const sleep = ms => new Promise(r => setTimeout(r, ms));

// ── The mock OpenID provider ────────────────────────────────────────────────
const idpKey = crypto.generateKeyPairSync('rsa', {modulusLength: 2048});
const rogueKey = crypto.generateKeyPairSync('rsa', {modulusLength: 2048});
const jwk = {...idpKey.publicKey.export({format: 'jwk'}), kid: 'idp1', alg: 'RS256', use: 'sig'};
const b64u = b => Buffer.from(b).toString('base64url');
const sign = (claims, key = idpKey.privateKey, kid = 'idp1') => {
  const h = b64u(JSON.stringify({alg: 'RS256', typ: 'JWT', kid}));
  const p = b64u(JSON.stringify(claims));
  return `${h}.${p}.${crypto.sign('sha256', Buffer.from(`${h}.${p}`), key).toString('base64url')}`;
};
const CLIENT_ID = 'amux-e2e', CLIENT_SECRET = 'e2e-client-secret';
const idp = {user: null, mode: 'good', codes: new Map(), pkceChecked: 0, authorizeSeen: []};
let ISSUER = '';
const server = http.createServer((req, res) => {
  const u = new URL(req.url, ISSUER);
  const json = (s, v) => { res.writeHead(s, {'Content-Type': 'application/json'}); res.end(JSON.stringify(v)); };
  if (u.pathname === '/.well-known/openid-configuration') {
    return json(200, {issuer: ISSUER, authorization_endpoint: ISSUER + '/authorize', token_endpoint: ISSUER + '/token',
      jwks_uri: ISSUER + '/jwks', response_types_supported: ['code'], code_challenge_methods_supported: ['S256']});
  }
  if (u.pathname === '/jwks') return json(200, {keys: [jwk]});
  if (u.pathname === '/authorize') {
    const q = Object.fromEntries(u.searchParams);
    idp.authorizeSeen.push(q);
    const back = new URL(q.redirect_uri);
    if (idp.mode === 'deny') {
      back.searchParams.set('error', 'access_denied'); back.searchParams.set('state', q.state);
    } else {
      const code = crypto.randomBytes(16).toString('hex');
      idp.codes.set(code, {...q, user: idp.user, mode: idp.mode});
      back.searchParams.set('code', code); back.searchParams.set('state', q.state);
    }
    res.writeHead(302, {Location: back.toString()}); return res.end();
  }
  if (u.pathname === '/token' && req.method === 'POST') {
    let buf = ''; req.on('data', c => buf += c);
    req.on('end', () => {
      const f = Object.fromEntries(new URLSearchParams(buf));
      const c = idp.codes.get(f.code); idp.codes.delete(f.code);
      if (!c) return json(400, {error: 'invalid_grant'});
      if (f.client_id !== CLIENT_ID || f.client_secret !== CLIENT_SECRET) return json(401, {error: 'invalid_client'});
      if (f.redirect_uri !== c.redirect_uri) return json(400, {error: 'invalid_grant', why: 'redirect_uri'});
      const want = crypto.createHash('sha256').update(f.code_verifier || '').digest('base64url');
      if (c.code_challenge_method !== 'S256' || want !== c.code_challenge) return json(400, {error: 'invalid_grant', why: 'pkce'});
      idp.pkceChecked++;
      const now = Math.floor(Date.now() / 1000);
      const claims = {iss: ISSUER, aud: CLIENT_ID, sub: 'sub-' + c.user.email, iat: now, exp: now + 300,
        nonce: c.mode === 'badnonce' ? 'not-the-nonce' : c.nonce, email: c.user.email,
        email_verified: c.mode !== 'unverified', name: c.user.name || '', amr: c.user.amr || ['pwd']};
      const id_token = c.mode === 'forged' ? sign(claims, rogueKey.privateKey) : sign(claims);
      json(200, {access_token: 'at', token_type: 'Bearer', expires_in: 300, id_token});
    });
    return;
  }
  res.writeHead(404); res.end();
});
await new Promise(r => server.listen(0, '127.0.0.1', r));
ISSUER = `http://127.0.0.1:${server.address().port}`;

// ── TOTP, as an authenticator app computes it ───────────────────────────────
const b32dec = s => {
  const A = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567'; let bits = 0, val = 0; const out = [];
  for (const ch of s.replace(/=+$/, '').toUpperCase()) { val = (val << 5) | A.indexOf(ch); bits += 5; if (bits >= 8) { out.push((val >>> (bits - 8)) & 255); bits -= 8; } }
  return Buffer.from(out);
};
const totp = (secret, t = Date.now() / 1000) => {
  const ctr = Buffer.alloc(8); ctr.writeBigUInt64BE(BigInt(Math.floor(t / 30)));
  const h = crypto.createHmac('sha1', b32dec(secret)).update(ctr).digest();
  const o = h[h.length - 1] & 15;
  return String(((h.readUInt32BE(o) & 0x7fffffff) % 1e6)).padStart(6, '0');
};
// A code for a step no code has used yet: the next window (one step of skew is allowed).
const step = () => Math.floor(Date.now() / 1000 / 30);
const used = new Map();
const freshCode = async (who, secret) => {
  for (;;) {
    const s = step();
    for (const d of [0, 1]) if ((used.get(who) ?? -1) < s + d) { used.set(who, s + d); return totp(secret, (s + d) * 30 + 1); }
    await sleep(2000);
  }
};

// ── amux ────────────────────────────────────────────────────────────────────
const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0', AMUX_VAULT_KEYSTORE: 'file',
}});
const ownerToken = () => fs.readFileSync(path.join(amux.home, 'auth_token'), 'utf8').trim();
const OWNER = () => ({Authorization: 'Bearer ' + ownerToken()});
const db = (sql, args = []) => JSON.parse(execFileSync('python3', ['-I', '-c',
  'import json,sqlite3,sys; c=sqlite3.connect(sys.argv[1],timeout=30); c.row_factory=sqlite3.Row; r=c.execute(sys.argv[2],json.loads(sys.argv[3])); print(json.dumps([dict(x) for x in r.fetchall()])); c.commit()',
  path.join(amux.home, 'amux.db'), sql, JSON.stringify(args)], {encoding: 'utf8'}));
const raw = (method, url, {headers = {}, body} = {}) => new Promise((resolve, reject) => {
  const mod = url.startsWith('https:') ? https : http;
  const data = body === undefined ? undefined : Buffer.from(typeof body === 'string' ? body : JSON.stringify(body));
  const r = mod.request(url, {method, rejectUnauthorized: false, timeout: 20000,
    headers: {...(data ? {'Content-Length': data.length} : {}), ...headers}}, res => {
    let buf = ''; res.on('data', c => buf += c);
    res.on('end', () => { let j = null; try { j = JSON.parse(buf); } catch {} resolve({status: res.statusCode, headers: res.headers, text: buf, body: j}); });
  });
  r.on('error', reject); if (data) r.write(data); r.end();
});
const cookieOf = r => (r.headers['set-cookie'] || []).map(c => c.split(';')[0]).find(c => c.startsWith('amux_member='));
const formPost = (p, form) => raw('POST', amux.base + p, {headers: {'Content-Type': 'application/x-www-form-urlencoded'}, body: new URLSearchParams(form).toString()});
const invite = async email => {
  const inv = await amux.req('POST', '/api/org/invites', {email, scope_level: 'global', grant_host_access: true}, 10000, OWNER());
  const tok = inv.body.token || String(inv.body.url || '').split('/invite/')[1];
  return cookieOf(await formPost(`/invite/${tok}`, {email, name: email.split('@')[0]}));
};
/** Walk the browser's part of a sign-in: /sso/login -> provider -> /sso/callback. */
const sso = async (user, mode = 'good', next = '/') => {
  idp.user = user; idp.mode = mode;
  const a = await raw('GET', `${amux.base}/sso/login?next=${encodeURIComponent(next)}`);
  if (a.status !== 303) return {stage: 'login', a};
  const b = await raw('GET', a.headers.location);
  if (b.status !== 302) return {stage: 'authorize', b};
  const c = await raw('GET', b.headers.location);
  return {stage: 'callback', status: c.status, cookie: cookieOf(c), location: c.headers.location, text: c.text, callback: b.headers.location};
};
const whoami = async cookie => (await amux.req('GET', '/api/mfa/status', undefined, 10000, {Cookie: cookie}));
const restart = async env => { await amux.down(); Object.assign(amux.env, env); await amux.up(); };
const counters = async () => (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body;
const identityRows = action => db("select actor, action, resource, decision, outcome from _amux_ledger where kind='identity' and action=? order by seq", [action]);
const SSO_ENV = {AMUX_OIDC_ISSUER: ISSUER, AMUX_OIDC_CLIENT_ID: CLIENT_ID, AMUX_OIDC_CLIENT_SECRET: CLIENT_SECRET,
  AMUX_OIDC_JIT_DOMAINS: 'jit.e2e.invalid'};
const ANN = 'ann@e2e.invalid', BOB = 'bob@e2e.invalid';

let ok = false;
try {
  await restart({...SSO_ENV, AMUX_OIDC_REDIRECT_URL: `${amux.base}/sso/callback`});

  // a. SSO.
  const annInvite = await invite(ANN);
  const bobInvite = await invite(BOB);
  check('a. two members accept invites', !!annInvite && !!bobInvite, {annInvite, bobInvite});
  let s = await sso({email: ANN, name: 'Ann'}, 'good', '/messages');
  check('a. an invited member signs in through the provider and lands where she was going',
    s.status === 303 && !!s.cookie && s.location === '/messages', s);
  const q = idp.authorizeSeen.at(-1);
  check('a. the request used PKCE S256, a state, a nonce and the configured client',
    q.code_challenge_method === 'S256' && q.code_challenge?.length === 43 && q.state && q.nonce && q.client_id === CLIENT_ID
      && q.response_type === 'code' && q.scope.includes('openid'), q);
  check('a. the provider checked the verifier against the challenge', idp.pkceChecked === 1, idp.pkceChecked);
  const annSso = s.cookie;
  let w = await whoami(annSso);
  check('a. the SSO cookie names Ann', w.status === 200 && w.body.subject === 'member:' + ANN, w);
  const row = db("select via, mfa_at, expires_at - created_at ttl from org_invites where token=?", [annSso.split('=')[1]])[0];
  check('a. it is an ordinary member session marked sso, expiring in 12 hours, no factor', row.via === 'sso' && row.ttl === 43200 && row.mfa_at === null, row);
  const replay = await raw('GET', s.callback);
  check('a. the same state is refused the second time', replay.status === 403 && !cookieOf(replay), replay.status);
  for (const [mode, why] of [['badnonce', 'nonce'], ['forged', 'id token'], ['unverified', 'verified'], ['deny', 'access_denied']]) {
    const r = await sso({email: ANN}, mode);
    check(`a. ${mode} is refused with no session`, r.status === 403 && !r.cookie && r.text.includes(why), r);
  }
  const before = db('select count(*) n from org_members')[0].n;
  const stranger = await sso({email: 'stranger@other.invalid'});
  check('a. a stranger is refused and no member is made', stranger.status === 403 && !stranger.cookie
    && stranger.text.includes('not a member') && db('select count(*) n from org_members')[0].n === before, stranger);
  const jit = await sso({email: 'new@jit.e2e.invalid', name: 'Newcomer'});
  const jm = db("select scope_level, scope_name, provisioned_by from org_members where email='new@jit.e2e.invalid'")[0];
  check('a. a just-in-time domain makes a member, with no access', jit.status === 303 && !!jit.cookie
    && jm.scope_level === 'worker' && jm.scope_name === '__unassigned__' && jm.provisioned_by === 'sso', {jit, jm});
  const jitWorkers = await amux.req('GET', '/api/sessions/anything', undefined, 10000, {Cookie: jit.cookie});
  check('a. that member reaches no worker', jitWorkers.status === 403, jitWorkers.status);
  const annMfa = await sso({email: ANN, amr: ['pwd', 'mfa']});
  check('a. an amr of mfa marks the session', db('select mfa_at from org_invites where token=?', [annMfa.cookie.split('=')[1]])[0].mfa_at > 0, annMfa);
  const signins = identityRows('sso_signin');
  check('a. every sign-in and refusal is in the ledger', signins.filter(r => r.decision === 'allowed').length >= 3
    && signins.filter(r => r.decision === 'refused').length >= 5 && identityRows('member_provisioned').length === 1, signins);

  // b. SSO required.
  let r = await amux.req('GET', '/api/board', undefined, 10000, {Cookie: bobInvite});
  check('b. report: an invite session still works', r.status === 200, r.status);
  check('b. report: and is counted', (await counters()).sso.counts.required_would_refuse > 0, (await counters()).sso);
  await restart({AMUX_SSO_REQUIRED: 'enforce'});
  r = await amux.req('GET', '/api/board', undefined, 10000, {Cookie: bobInvite});
  check('b. enforce: an invite session is refused on the API', r.status === 403 && r.body.code === 'sso_required', r);
  r = await raw('GET', amux.base + '/', {headers: {Cookie: bobInvite}});
  check('b. enforce: and sent to sign in from a page', r.status === 303 && r.headers.location === '/sso/login?next=%2F', r.status);
  r = await amux.req('GET', '/api/board', undefined, 10000, {Cookie: annSso});
  check('b. enforce: an SSO session goes through', r.status === 200, r.status);
  r = await amux.req('GET', '/api/board', undefined, 10000, OWNER());
  check('b. enforce: the owner\'s bearer is never sent to SSO (break glass)', r.status === 200, r.status);
  await restart({AMUX_SSO_REQUIRED: 'off'});

  // c. MFA for a member.
  let e = await amux.req('POST', '/api/mfa/enroll', {}, 10000, {Cookie: bobInvite});
  check('c. Bob starts enrolment and gets a key', e.status === 200 && /^[A-Z2-7]{32}$/.test(e.body.secret)
    && e.body.otpauth_uri.startsWith('otpauth://totp/amux:bob%40e2e.invalid?'), e);
  const bobSecret = e.body.secret;
  r = await amux.req('POST', '/api/mfa/enroll/confirm', {code: '000000'}, 10000, {Cookie: bobInvite});
  check('c. a wrong first code does not turn it on', r.status === 403 && r.body.code === 'mfa_bad_code', r);
  r = await amux.req('POST', '/api/mfa/enroll/confirm', {code: await freshCode('bob', bobSecret)}, 10000, {Cookie: bobInvite});
  check('c. the right one does, with ten recovery codes', r.status === 200 && r.body.recovery_codes.length === 10, r);
  const bobRecovery = r.body.recovery_codes;
  const f = db("select secret_sealed from mfa_factors where subject=?", ['member:' + BOB])[0];
  check('c. the secret is sealed, not stored', !f.secret_sealed.includes(bobSecret) && f.secret_sealed.includes('"ct"'), f);
  const rc = db("select code_hash from mfa_recovery_codes where subject=?", ['member:' + BOB]);
  check('c. recovery codes are stored as hashes', rc.length === 10 && rc.every(x => /^[0-9a-f]{64}$/.test(x.code_hash))
    && !rc.some(x => bobRecovery.includes(x.code_hash)), rc.length);
  await restart({AMUX_MFA_ENFORCE: 'enforce'});
  const bobSso = (await sso({email: BOB})).cookie;
  r = await amux.req('GET', '/api/board', undefined, 10000, {Cookie: bobSso});
  check('c. enforce: Bob\'s new session owes a factor', r.status === 403 && r.body.code === 'mfa_required', r);
  r = await raw('GET', amux.base + '/', {headers: {Cookie: bobSso}});
  check('c. enforce: a page sends him to /mfa', r.status === 303 && r.headers.location === '/mfa?next=%2F', r.status);
  r = await raw('GET', amux.base + '/mfa', {headers: {Cookie: bobSso}});
  check('c. /mfa itself opens', r.status === 200 && r.text.includes('Second factor'), r.status);
  const code = await freshCode('bob', bobSecret);
  r = await amux.req('POST', '/api/mfa/verify', {code}, 10000, {Cookie: bobSso});
  check('c. a code proves the session', r.status === 200 && r.body.session_marked === true, r);
  r = await amux.req('GET', '/api/board', undefined, 10000, {Cookie: bobSso});
  check('c. and Bob is through', r.status === 200, r.status);
  r = await amux.req('POST', '/api/mfa/verify', {code}, 10000, {Cookie: bobSso});
  check('c. the same code twice is refused', r.status === 403 && r.body.code === 'mfa_code_used', r);
  const bobSso2 = (await sso({email: BOB})).cookie;
  r = await amux.req('POST', '/api/mfa/verify', {code: bobRecovery[0]}, 10000, {Cookie: bobSso2});
  check('c. a recovery code works', r.status === 200 && r.body.with === 'recovery', r);
  r = await amux.req('POST', '/api/mfa/verify', {code: bobRecovery[0]}, 10000, {Cookie: bobSso2});
  check('c. once', r.status === 403, r);
  r = await amux.req('GET', '/api/board', undefined, 10000, {Cookie: annSso});
  check('c. optional policy: Ann, not enrolled, is not asked', r.status === 200, r.status);
  await restart({AMUX_MFA_ENFORCE: 'report', AMUX_MFA_POLICY: 'required'});
  r = await amux.req('GET', '/api/board', undefined, 10000, {Cookie: annSso});
  check('c. required + report: Ann goes through and is counted', r.status === 200 && (await counters()).mfa.counts.session_would_refuse > 0, r.status);
  await restart({AMUX_MFA_POLICY: 'optional'});

  // d. Step-up for the owner.
  e = await amux.req('POST', '/api/mfa/enroll', {}, 10000, OWNER());
  const ownerSecret = e.body.secret;
  r = await amux.req('POST', '/api/mfa/enroll/confirm', {code: await freshCode('owner', ownerSecret)}, 10000, OWNER());
  check('d. the owner enrols', r.status === 200 && e.body.subject === 'owner', {e: e.body, r});
  r = await amux.req('POST', '/api/audit/kill-switch', {engaged: true, reason: 'e2e'}, 10000, OWNER());
  check('d. report: the kill switch works without step-up', r.status === 200, r);
  await amux.req('POST', '/api/audit/kill-switch', {engaged: false}, 10000, OWNER());
  check('d. report: and that is counted', (await counters()).mfa.counts.step_up_would_refuse >= 2, (await counters()).mfa);
  await restart({AMUX_MFA_STEP_UP: 'enforce'});
  r = await amux.req('POST', '/api/audit/kill-switch', {engaged: true, reason: 'e2e'}, 10000, OWNER());
  check('d. enforce: the kill switch needs a fresh factor', r.status === 403 && r.body.code === 'step_up_required', r);
  fs.mkdirSync(path.join(amux.home, 'sessions'), {recursive: true});
  fs.writeFileSync(path.join(amux.home, 'sessions', 'wexus.env'), 'CC_TAGS=customers\nCC_ARCHIVED=0\n');
  r = await amux.req('PUT', '/api/scope', {level: 'worker', name: 'wexus', capability: 'env', value: {AMUX_CAP_PUBLISH: 'off'}}, 10000, OWNER());
  check('d. enforce: so does a capability change', r.status === 403 && r.body.code === 'step_up_required', r);
  r = await amux.req('POST', '/api/mfa/step-up', {code: '123456'}, 10000, OWNER());
  check('d. a wrong code gets no token', r.status === 403 && !r.body.token, r);
  r = await amux.req('POST', '/api/mfa/step-up', {code: await freshCode('owner', ownerSecret)}, 10000, OWNER());
  check('d. a right code gets a five-minute token', r.status === 200 && r.body.token?.startsWith('su_'), r);
  const SU = {...OWNER(), 'X-Amux-Step-Up': r.body.token};
  r = await amux.req('POST', '/api/audit/kill-switch', {engaged: true, reason: 'e2e'}, 10000, SU);
  check('d. with it the kill switch engages', r.status === 200, r);
  r = await amux.req('POST', '/api/audit/kill-switch', {engaged: false}, 10000, SU);
  check('d. and lifts', r.status === 200, r);
  r = await amux.req('PUT', '/api/scope', {level: 'worker', name: 'wexus', capability: 'env', value: {AMUX_CAP_PUBLISH: 'off'}}, 10000, SU);
  check('d. and the capability changes', r.status === 200, r);
  r = await amux.req('POST', '/api/audit/kill-switch', {engaged: true}, 10000, {Cookie: bobSso, 'X-Amux-Step-Up': SU['X-Amux-Step-Up']});
  check('d. the owner\'s token does nothing for a member', r.status === 403 && r.body.code === 'owner_only', r);

  // e. SCIM.
  r = await raw('GET', amux.base + '/scim/v2/Users');
  check('e. no token: 401', r.status === 401 && r.body.schemas[0].includes('Error'), r);
  r = await amux.req('POST', '/api/scim/tokens', {label: 'okta'}, 10000, OWNER());
  check('e. issuing a token is under step-up too', r.status === 403 && r.body.code === 'step_up_required', r);
  r = await amux.req('POST', '/api/scim/tokens', {label: 'okta'}, 10000, SU);
  check('e. the owner issues a SCIM token', r.status === 200 && r.body.token.startsWith('scim_'), r);
  const scimTok = r.body.token, scimId = r.body.id;
  check('e. only its hash is kept', db('select token_hash from scim_tokens')[0].token_hash !== scimTok, null);
  r = await amux.req('POST', '/api/scim/tokens', {label: 'x'}, 10000, {Cookie: bobSso});
  check('e. a member cannot issue one', r.status === 403, r);
  const S = {Authorization: 'Bearer ' + scimTok, 'Content-Type': 'application/scim+json'};
  const CAROL = 'carol@e2e.invalid';
  r = await raw('POST', amux.base + '/scim/v2/Users', {headers: S, body: {schemas: ['urn:ietf:params:scim:schemas:core:2.0:User'],
    userName: CAROL, name: {givenName: 'Carol', familyName: 'C'}, externalId: 'okta-carol', active: true}});
  check('e. the provider creates Carol', r.status === 201 && r.body.userName === CAROL && r.body.active === true, r);
  const carolId = r.body.id;
  r = await raw('POST', amux.base + '/scim/v2/Users', {headers: S, body: {userName: CAROL}});
  check('e. twice is a uniqueness conflict', r.status === 409 && r.body.scimType === 'uniqueness', r);
  r = await raw('GET', amux.base + '/scim/v2/Users?filter=' + encodeURIComponent(`userName eq "${CAROL}"`), {headers: S});
  check('e. a filter finds her', r.status === 200 && r.body.totalResults === 1 && r.body.Resources[0].id === carolId, r);
  r = await raw('POST', amux.base + '/scim/v2/Groups', {headers: S, body: {displayName: 'Finance', members: [{value: carolId}]}});
  check('e. a group is a team with her in it', r.status === 201 && r.body.members[0].value === carolId
    && db("select scope_name from org_teams where id=?", [r.body.id])[0].scope_name === '__unassigned__', r);
  const carol = (await sso({email: CAROL})).cookie;
  check('e. Carol signs in through the provider', (await whoami(carol)).body.subject === 'member:' + CAROL, null);
  r = await raw('PATCH', amux.base + `/scim/v2/Users/${carolId}`, {headers: S, body: {schemas: ['urn:ietf:params:scim:api:messages:2.0:PatchOp'],
    Operations: [{op: 'Replace', path: 'active', value: 'False'}]}});
  check('e. the provider deactivates her', r.status === 200 && r.body.active === false, r);
  r = await whoami(carol);
  check('e. her session stops at once', r.status === 403, r);
  const c2 = await sso({email: CAROL});
  check('e. she cannot sign in again', c2.status === 403 && !c2.cookie && c2.text.includes('removed'), c2);
  const off = db("select actor, action from _amux_ledger where kind='offboard' and on_behalf_of=?", ['member:' + CAROL]);
  check('e. offboarding ran, by the SCIM token', off.length >= 1 && off.every(x => x.actor === `policy:scim:${scimId}`), off);
  r = await raw('PATCH', amux.base + `/scim/v2/Users/${carolId}`, {headers: S, body: {Operations: [{op: 'replace', value: {active: true}}]}});
  check('e. reactivated', r.status === 200 && r.body.active === true, r);
  check('e. she signs in again', !!(await sso({email: CAROL})).cookie, null);
  r = await raw('DELETE', amux.base + `/scim/v2/Users/${carolId}`, {headers: S});
  check('e. deleted', r.status === 204, r.status);
  r = await raw('GET', amux.base + `/scim/v2/Users/${carolId}`, {headers: S});
  check('e. and gone', r.status === 404, r.status);
  r = await amux.req('DELETE', `/api/scim/tokens/${scimId}`, undefined, 10000, SU);
  check('e. the owner revokes the token', r.status === 200 && r.body.revoked === true, r);
  r = await raw('GET', amux.base + '/scim/v2/Users', {headers: S});
  check('e. a revoked token is 401', r.status === 401, r.status);
  const scimRows = db("select action from _amux_ledger where kind='identity' and actor=?", [`policy:scim:${scimId}`]).map(x => x.action);
  for (const a of ['scim_user_created', 'scim_group_created', 'scim_group_member_added', 'scim_user_reactivated', 'scim_user_deleted']) {
    check(`e. the ledger records ${a}`, scimRows.includes(a), scimRows);
  }

  // f. Counters.
  const k = await counters();
  // Counters are since this process started; the last restart was before d.
  check('f. sso, mfa and scim say what they measured since the last restart', k.sso.measured && k.mfa.measured && k.scim.measured
    && k.sso.counts.signed_in >= 2 && k.sso.counts.refused >= 1
    && k.mfa.counts.step_up_refused >= 3 && k.mfa.counts.step_up_issued === 1 && k.mfa.counts.step_up_passed >= 4
    && k.scim.counts.created === 1 && k.scim.counts.deactivated === 2 && k.scim.counts.reactivated === 1
    && k.scim.counts.unauthorized >= 2 && k.scim.counts.tokens_revoked === 1, {sso: k.sso, mfa: k.mfa, scim: k.scim});
  for (const a of ['mfa_enrolled', 'mfa_verified', 'mfa_step_up', 'scim_token_issued', 'scim_token_revoked']) {
    check(`f. the ledger records ${a}`, identityRows(a).length >= 1, a);
  }
  ok = true;
} finally {
  const receipt = {scenario: 'sso-mfa-scim', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
  server.close();
}
process.exit(ok ? 0 : 1);
