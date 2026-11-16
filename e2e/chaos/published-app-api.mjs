#!/usr/bin/env node
// END-TO-END (governance PR 5): a published app is its own principal, with no
// amux access unless a person gave it some.
//
// A private amux and a real published app: a small Python server started the
// way a worker starts one (its environment holds the worker's identity token,
// as a worker's shell would hand down), published on its port. The app has a
// /probe endpoint that calls the amux API back the ways an app could:
//   none     no credential at all
//   worker   the worker token it inherited
//   owner    the owner token, read from disk
//   token    its own app token, for a call outside its allowlist
//   allowed  its own app token, for the call a person allowed
//   bogus    an app token that matches no app
// Checks:
//   a. the diagnostic (GET /api/contract/counters -> published_app) reports
//      mode report, measured, origin detection supported, the app credentialed;
//   b. publishing returns the app token once and the list never shows it;
//      the public /pub/<id>/ page serves the app;
//   c. report mode: every probe still answers as before, and each is counted
//      under the app by kind (uncredentialed, inherited_worker_token,
//      owner_token, outside_allowlist, allowed), one log line per kind;
//   d. the allowed call is attributed app:<id> in the interaction receipts;
//   e. a worker asks for access when publishing: nothing is allowed until a
//      person approves, approving allows exactly what was asked, and a worker
//      cannot set an app's access itself;
//   f. enforce: none, worker, owner, token and bogus are refused 403
//      published_app_api_refused with a how_to_fix; the allowed call still
//      works; the owner calling from outside the app is unaffected; /pub/
//      still serves;
//   g. AMUX_PUBLISHED_APP_API=off takes the checks away again.
//
// Linux only for the process checks (origin detection reads /proc); the
// scenario says so and skips them elsewhere.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/published-app-api.mjs
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import {spawn} from 'node:child_process';
import {startAmux, waitFor, freePort} from './harness.mjs';

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
const LINUX = process.platform === 'linux';
const ownerTokenFile = path.join(amux.home, ['auth', 'token'].join('_'));
const ownerToken = () => fs.readFileSync(ownerTokenFile, 'utf8').trim();
const OWNER = () => ({Authorization: 'Bearer ' + ownerToken()});
// The owner signed in to the dashboard (static_files.rs: the owner-session
// cookie is sha256 of "amux-owner-session:" + the owner token).
const PERSON = () => ({...OWNER(), Cookie: '__Host-amux_owner=' + crypto.createHash('sha256').update('amux-owner-session:' + ownerToken()).digest('hex')});
const counters = async () => (await amux.req('GET', '/api/contract/counters', undefined, 10000, OWNER())).body.published_app || {};
const logLines = verdict => fs.readFileSync(amux.serverLog, 'utf8').split('\n').filter(l => l.includes(verdict));
const restart = async env => { await amux.down(); Object.assign(amux.env, env); await amux.up(); };

// The worker that "started" the app, with a minted identity token on disk.
fs.mkdirSync(path.join(amux.home, 'sessions'), {recursive: true});
fs.mkdirSync(path.join(amux.home, 'worker-tokens'), {recursive: true});
fs.writeFileSync(path.join(amux.home, 'sessions', 'w-app.env'), 'CC_TAGS=""\nCC_ARCHIVED=0\n');
const workerToken = crypto.randomBytes(24).toString('hex');
fs.writeFileSync(path.join(amux.home, 'worker-tokens', 'w-app.sha256'),
  crypto.createHash('sha256').update(workerToken).digest('hex'), {mode: 0o600});

// The app: answers / for visitors and /probe?kind=... by calling amux back.
const appDir = fs.mkdtempSync(path.join(os.tmpdir(), 'pubapp-'));
const appPy = path.join(appDir, 'app.py');
fs.writeFileSync(appPy, `
import json, os, ssl, sys, urllib.request, urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs
CTX = ssl._create_unverified_context()
BASE = os.environ["AMUX_URL"]
def call(method, path, headers, body=None):
    req = urllib.request.Request(BASE + path, method=method, headers=headers,
        data=None if body is None else json.dumps(body).encode())
    if body is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=10) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()
class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def do_GET(self):
        u = urlparse(self.path)
        if u.path != "/probe":
            self.send_response(200); self.send_header("Content-Type", "text/html"); self.end_headers()
            self.wfile.write(b"<h1>published app</h1>"); return
        kind = parse_qs(u.query).get("kind", [""])[0]
        tok = os.environ.get("AMUX_APP_TOKEN", "")
        if kind == "none":
            st, b = call("GET", "/api/sessions", {})
        elif kind == "worker":
            st, b = call("GET", "/api/sessions", {"X-Amux-Session": os.environ["AMUX_SESSION"], "X-Amux-Worker-Token": os.environ["AMUX_WORKER_TOKEN"]})
        elif kind == "owner":
            st, b = call("GET", "/api/sessions", {"Authorization": "Bearer " + open(os.environ["OWNER_FILE"]).read().strip()})
        elif kind == "token":
            st, b = call("GET", "/api/sessions", {"X-Amux-App-Token": tok, "X-Amux-Session": os.environ["AMUX_SESSION"]})
        elif kind == "allowed":
            st, b = call("POST", "/api/board", {"X-Amux-App-Token": tok}, {"title": "filed by the published app", "status": "todo"})
        elif kind == "bogus":
            st, b = call("GET", "/api/sessions", {"X-Amux-App-Token": "app_" + "0" * 64})
        else:
            st, b = 0, "unknown kind"
        out = json.dumps({"status": st, "body": b[:600]}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json"); self.end_headers()
        self.wfile.write(out)
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
`);

const appPort = await freePort();
let app;
const startApp = token => {
  app = spawn('python3', [appPy, String(appPort)], {stdio: 'ignore', env: {
    ...process.env, AMUX_URL: amux.base, AMUX_APP_TOKEN: token, AMUX_SESSION: 'w-app',
    AMUX_WORKER_TOKEN: workerToken, OWNER_FILE: ownerTokenFile,
  }});
};
// harness.request speaks https; the app is plain http.
const http = await import('node:http');
const plain = p => new Promise((res, rej) => {
  const r = http.request(`http://127.0.0.1:${appPort}${p}`, {timeout: 20000}, resp => {
    let b = ''; resp.on('data', c => b += c); resp.on('end', () => res({status: resp.statusCode, body: b}));
  });
  r.on('error', rej); r.end();
});
const probe = async kind => JSON.parse((await plain('/probe?kind=' + kind)).body);

let ok = false;
try {
  // b. Publish as the owner, signed in (a person): access is allowed at once.
  const pub = await amux.req('POST', '/api/page-links', {kind: 'app', port: appPort, title: 'probe app', api: ['POST /api/board']}, 10000, PERSON());
  check('b. the owner publishes the app with access to POST /api/board', pub.status === 201 && pub.body.api_allow?.[0] === 'POST /api/board', pub);
  const id = pub.body.id, token = pub.body.app_token;
  check('b. the app token is returned once, at publish', typeof token === 'string' && token.startsWith('app_') && pub.body.app_credential === true, pub.body);
  const list = await amux.req('GET', '/api/page-links', undefined, 10000, OWNER());
  check('b. the list never shows the token or its hash', !JSON.stringify(list.body).includes(token)
    && !JSON.stringify(list.body).includes(crypto.createHash('sha256').update(token).digest('hex')), list.body);
  startApp(token);
  await waitFor('the app to listen', async () => { try { return (await plain('/')).status === 200; } catch { return false; } }, 15000);
  const visit = await amux.req('GET', `/pub/${id}/`);
  check('b. the public page serves the app', visit.status === 200 && JSON.stringify(visit.body).includes('published app'), visit);

  // a. Diagnostic.
  let c = await counters();
  check('a. report is the default, measured, the app credentialed', c.mode === 'report' && c.measured === true
    && c.apps === 1 && c.apps_with_credential === 1, c);
  check('a. origin detection says whether it can run here', c.origin_detection?.supported === LINUX && c.origin_detection?.measured === LINUX, c);

  // c. Report mode: everything answers, everything is counted.
  const kinds = LINUX ? ['none', 'worker', 'owner', 'token'] : ['token'];
  for (const k of kinds) {
    const r = await probe(k);
    check(`c. report: the ${k} probe still answers 200`, r.status === 200, r);
  }
  const al = await probe('allowed');
  check('c. report: the allowed call works', al.status >= 200 && al.status < 300, al);
  c = await counters();
  const row = c.by_app?.[id] || {};
  if (LINUX) {
    check('c. counted: uncredentialed, inherited worker token, owner token', row.uncredentialed >= 1 && row.inherited_worker_token >= 1 && row.owner_token >= 1, row);
  }
  check('c. counted: outside its allowlist, and allowed', row.outside_allowlist >= 1 && row.allowed >= 1 && row.refused === 0, row);
  await waitFor('would-refuse log lines', () => logLines('published_app_api_would_be_refused').length >= (LINUX ? 4 : 1), 10000);
  const before = logLines('published_app_api_would_be_refused').length;
  await probe('token');
  check('c. one log line per app and kind, not per call', logLines('published_app_api_would_be_refused').length === before);

  // d. Attribution.
  const recent = await amux.req('GET', '/api/interactions/recent?since_h=1', undefined, 10000, OWNER());
  const rows = recent.body.interactions || [];
  const mine = rows.filter(i => String(i.request?.path || '').startsWith('/api/board') && i.request?.method === 'POST');
  check('d. the allowed call is attributed to the app', mine.some(i => i.origin?.session === `app:${id}`), {status: recent.status, mine: mine.slice(0, 5)});

  // e. A worker asks; a person decides.
  const asked = await amux.req('POST', '/api/page-links', {kind: 'app', port: appPort + 1, title: 'asks', api: ['GET /api/sessions']},
    10000, {...OWNER(), 'X-Amux-Session': 'w-app', 'X-Amux-Worker-Token': workerToken});
  check('e. a worker publishes and asks: pending, nothing allowed', asked.status === 201 && asked.body.pending === true
    && asked.body.api_requested?.[0] === 'GET /api/sessions' && (asked.body.api_allow || []).length === 0, asked);
  const self = await amux.req('POST', `/api/page-links/${asked.body.id}/api`, {allow: ['* /api/sessions/**']}, 10000,
    {...OWNER(), 'X-Amux-Session': 'w-app', 'X-Amux-Worker-Token': workerToken});
  check('e. the worker cannot set the access itself', self.status === 403, self);
  const never = await amux.req('POST', `/api/page-links/${asked.body.id}/api`, {allow: ['POST /api/page-links/x/approve']}, 10000, PERSON());
  check('e. no one can give an app its own access controls', never.status === 400, never);
  const appr = await amux.req('POST', `/api/page-links/${asked.body.id}/approve`, undefined, 10000, PERSON());
  check('e. approving allows exactly what it asked', appr.status === 200 && appr.body.api_allow?.[0] === 'GET /api/sessions'
    && (appr.body.api_requested || []).length === 0, appr);
  await amux.req('DELETE', `/api/page-links/${asked.body.id}`, undefined, 10000, PERSON());

  // f. Enforce.
  await restart({AMUX_PUBLISHED_APP_API: 'enforce', AMUX_LOOPBACK_TRUST: 'report'});
  check('f. restarted in enforce', (await counters()).mode === 'enforce');
  for (const k of (LINUX ? ['none', 'worker', 'owner', 'token', 'bogus'] : ['token', 'bogus'])) {
    const r = await probe(k);
    let body = {};
    try { body = JSON.parse(r.body); } catch {}
    check(`f. enforce: the ${k} probe is refused with a way forward`, r.status === 403 && body.reason === 'published_app_api_refused' && !!body.how_to_fix, r);
  }
  const al2 = await probe('allowed');
  check('f. enforce: the allowed call still works', al2.status >= 200 && al2.status < 300, al2);
  const outside = await amux.req('GET', '/api/sessions', undefined, 10000, OWNER());
  check('f. enforce: the owner calling from outside the app is unaffected', outside.status === 200, outside.status);
  const visit2 = await amux.req('GET', `/pub/${id}/`);
  check('f. enforce: the public page still serves', visit2.status === 200, visit2.status);
  await waitFor('refused log line', () => logLines('published_app_api_refused').length > 0, 10000);

  // g. Off.
  await restart({AMUX_PUBLISHED_APP_API: 'off'});
  const g = await probe(LINUX ? 'none' : 'token');
  check('g. AMUX_PUBLISHED_APP_API=off checks nothing', g.status === 200 && (await counters()).mode === 'off', g);
  ok = true;
} finally {
  try { app?.kill(); } catch {}
  fs.rmSync(appDir, {recursive: true, force: true});
  await amux.stop();
  console.log(JSON.stringify({ok, passed: checks.filter(c => c.ok).length, linux_process_checks: LINUX}));
  if (!ok) process.exitCode = 1;
}
