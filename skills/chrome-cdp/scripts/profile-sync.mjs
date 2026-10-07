// profile-sync - carry a site's logins from the real Chrome into an amux
// saved profile, so the next worker reaches the site on the first rung of the
// access ladder (amux browser) instead of borrowing the real Chrome again.
//
// Ethan, 2026-09-28: "if im giving credentials, or overriding with context to
// CDP ... it should save the profile and or append to existing amux profiles
// so we have a more native approach next time."
//
// Capture goes through the CDP connection the tab daemon already holds, so it
// adds no "Allow remote debugging?" prompt, and Storage.getCookies returns the
// values decrypted, so nothing depends on either browser's keychain key.
// Import starts the amux profile's own browser (no prompt, amux launches it
// with a debugging port), writes cookies and the origin's localStorage, reads
// the cookies back, stops the browser if it started it, and registers the
// site on the profile. An existing profile that already lists the site is
// appended to; otherwise a profile named after the site is created.
//
// Every run appends one JSON line to ~/.cache/cdp/profile-sync.log.

import { execFileSync } from 'child_process';
import { appendFileSync, mkdirSync } from 'fs';
import { homedir } from 'os';
import { resolve } from 'path';

const LOG_DIR = resolve(homedir(), '.cache', 'cdp');
const LOG = resolve(LOG_DIR, 'profile-sync.log');
const sleep = (ms) => new Promise(r => setTimeout(r, ms));

export function log(obj) {
  try { mkdirSync(LOG_DIR, { recursive: true, mode: 0o700 }); } catch {}
  try { appendFileSync(LOG, JSON.stringify({ at: new Date().toISOString(), ...obj }) + '\n'); } catch {}
}

// Registrable domain, close enough for cookie scoping: app.netsuite.com ->
// netsuite.com, portal.example.co.uk -> example.co.uk. Null for IPs,
// localhost and single-label hosts, which are never synced.
const SECOND_LEVEL = new Set(['co', 'com', 'org', 'net', 'gov', 'ac', 'edu', 'ne', 'or']);
export function siteOf(host) {
  host = String(host || '').toLowerCase().replace(/\.$/, '');
  if (!host.includes('.') || /^[\d.]+$/.test(host) || host.includes(':')) return null;
  const p = host.split('.');
  if (p.length >= 3 && p[p.length - 1].length === 2 && SECOND_LEVEL.has(p[p.length - 2])) return p.slice(-3).join('.');
  return p.slice(-2).join('.');
}

export const onSite = (c, site) => {
  const d = String(c.domain || '').replace(/^\./, '');
  return d === site || d.endsWith('.' + site);
};

// The cookies a login changes: httpOnly or secure ones on the site.
export function fingerprint(cookies, site) {
  return cookies
    .filter(c => onSite(c, site) && (c.httpOnly || c.secure))
    .map(c => `${c.name}|${c.domain}|${c.path}|${c.value}`)
    .sort()
    .join('\n');
}

function amuxBase() {
  try { return execFileSync('amux', ['url'], { encoding: 'utf8', timeout: 5000 }).trim(); }
  catch { return (process.env.AMUX_URL || 'https://localhost:8824').trim(); }
}

function api(method, path, body) {
  const args = ['-sk', '--max-time', '90', '-X', method, '-H', 'Content-Type: application/json'];
  if (process.env.AMUX_SESSION) args.push('-H', `X-Amux-Session: ${process.env.AMUX_SESSION}`);
  if (body) args.push('-d', JSON.stringify(body));
  args.push(amuxBase() + path);
  const out = execFileSync('curl', args, { encoding: 'utf8', maxBuffer: 16 << 20 });
  try { return JSON.parse(out); } catch { return { error: out.slice(0, 200) || 'empty response' }; }
}

export function profilesIndex() {
  const d = api('GET', '/api/browser/profiles');
  return Array.isArray(d.profiles) ? d.profiles : [];
}

export function coveringProfile(site, list) {
  return list.find(p => (p.domains || []).some(d => d === site || d.endsWith('.' + site) || site.endsWith('.' + d)));
}

// What the tab shows right now: its site, origin, the site's cookies (not
// partitioned ones, whose scope would change on import) and localStorage.
export async function capture(cdp, sessionId) {
  const r = await cdp.send('Runtime.evaluate', {
    expression: 'JSON.stringify({href: location.href, ls: (() => { try { return Object.entries(localStorage) } catch (e) { return [] } })()})',
    returnByValue: true,
  }, sessionId);
  const page = JSON.parse(r.result.value);
  const url = new URL(page.href);
  if (!/^https?:$/.test(url.protocol)) return null;
  const site = siteOf(url.hostname);
  if (!site) return null;
  const { cookies } = await cdp.send('Storage.getCookies', {});
  return {
    site, host: url.hostname, origin: url.origin, href: page.href,
    cookies: cookies.filter(c => onSite(c, site) && !c.partitionKey),
    allCookies: cookies,
    localStorage: page.ls,
  };
}

// A session cookie is given 30 days. Chrome drops session cookies when the
// profile's browser closes, and importInto stops that browser right after the
// copy, so they used to vanish: brex reported 9/9 landed and held 0, GitHub
// kept 3 of 14, Cloudflare 3 of 43 (2026-10-07).
const SESSION_TTL_S = 30 * 86400;

// Does this cookie look like a signed-in session? Same rule as the server's
// integrations::browser_logins::is_auth_cookie, so both sides agree.
const NOT_AUTH = new Set(['__cf_bm', '_cfuvid', 'cf_clearance', '__cflb', '_ga', '_gid', '_gcl_au', '_fbp', '_fbc',
  '__stripe_mid', '__stripe_sid', 'm', 'ajs_anonymous_id', 'ajs_user_id', 'nid', 'aec', 'ar_debug', 'datadome', 'aws-waf-token']);
const AUTH_MARKERS = ['sess', 'auth', 'token', 'login', 'logged', 'sid', 'jwt', 'remember', 'li_at', 'access',
  'refresh', 'user', 'account', 'identity', '_secure-'];
export function isAuthCookie(name) {
  const n = String(name || '').toLowerCase();
  if (NOT_AUTH.has(n) || n.startsWith('_ga_') || n.startsWith('_hj')) return false;
  return AUTH_MARKERS.some(m => n.includes(m));
}

// A sign-in page is not a login worth saving.
export function looksLikeLoginPage(href) {
  try {
    const u = new URL(href);
    return /(^|\/)(log-?in|sign-?in|signin|auth|sso|oauth|session\/new|account\/login)(\/|$|\?)/i.test(u.pathname + '/');
  } catch { return false; }
}

function cookieParam(c) {
  const p = { name: c.name, value: c.value, domain: c.domain, path: c.path, secure: c.secure, httpOnly: c.httpOnly };
  if (c.sameSite) p.sameSite = c.sameSite;
  if (!c.session && c.expires > 0) p.expires = c.expires;
  else p.expires = Math.floor(Date.now() / 1000) + SESSION_TTL_S;
  if (c.priority) p.priority = c.priority;
  if (c.sourceScheme) p.sourceScheme = c.sourceScheme;
  return p;
}

export async function importInto(CDPClass, profile, cap) {
  const running = () => (api('GET', '/api/browser/status').browsers || []).find(b => b.profile === profile && b.cdp_port);
  let b = running();
  let started = false;
  if (!b) {
    if (!profilesIndex().some(p => p.name === profile)) {
      const c = api('POST', '/api/browser/profile/create', { name: profile });
      if (c.error) throw new Error(`create profile ${profile}: ${JSON.stringify(c.error).slice(0, 200)}`);
    }
    const s = api('POST', '/api/browser/start', { profile, url: 'about:blank', headless: true });
    if (s.error) throw new Error(`start profile ${profile}: ${JSON.stringify(s.error).slice(0, 200)}`);
    started = true;
    for (let i = 0; i < 75 && !b; i++) { await sleep(200); b = running(); }
    if (!b) throw new Error(`profile ${profile} did not come up`);
  }
  try {
    const v = JSON.parse(execFileSync('curl', ['-s', '--max-time', '5', `http://127.0.0.1:${b.cdp_port}/json/version`], { encoding: 'utf8' }));
    const cdp = new CDPClass();
    await cdp.connect(v.webSocketDebuggerUrl);
    try {
      const params = cap.cookies.map(cookieParam);
      if (params.length) await cdp.send('Storage.setCookies', { cookies: params });
      let localStorageSet = 0;
      if (cap.localStorage?.length) {
        const { targetId } = await cdp.send('Target.createTarget', { url: cap.origin });
        const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
        let onOrigin = false;
        for (let i = 0; i < 60 && !onOrigin; i++) {
          await sleep(250);
          try {
            const r = await cdp.send('Runtime.evaluate', { expression: 'location.origin', returnByValue: true }, sessionId);
            onOrigin = r.result.value === cap.origin;
          } catch {}
        }
        if (onOrigin) {
          const r = await cdp.send('Runtime.evaluate', {
            expression: `(() => { const e = ${JSON.stringify(cap.localStorage)}; for (const [k, v] of e) localStorage.setItem(k, v); return e.length })()`,
            returnByValue: true,
          }, sessionId);
          localStorageSet = r.result.value;
        }
        await cdp.send('Target.closeTarget', { targetId }).catch(() => {});
      }
      // Read back: a set that the browser silently dropped must not report success.
      const { cookies } = await cdp.send('Storage.getCookies', {});
      const have = new Set(cookies.filter(c => onSite(c, cap.site)).map(c => `${c.name}|${c.domain}|${c.path}`));
      const landed = params.filter(p => have.has(`${p.name}|${p.domain}|${p.path}`)).length;
      return { cookies_sent: params.length, cookies_landed: landed, local_storage_set: localStorageSet };
    } finally {
      cdp.close();
    }
  } finally {
    if (started) api('POST', '/api/browser/stop', { profile });
  }
}

// Capture the tab's site and save it. `profile` overrides the choice.
export async function syncSite(CDPClass, cdp, sessionId, { profile: explicit = '', reason = 'explicit', list = null } = {}) {
  const cap = await capture(cdp, sessionId);
  if (!cap) return { skipped: 'not an http(s) page on a registrable domain' };
  if (!cap.cookies.length) return { skipped: `no cookies for ${cap.site}`, site: cap.site };
  // An AUTOMATIC save needs a signed-in page: a logged-out Brex page once
  // registered "brex" from its tracking cookies, and the next lane was sent to
  // it (mixpeek-finances, 2026-10-07). An explicit save-profile is honoured.
  if (reason !== 'explicit save-profile') {
    const href = cap.origin + (cap.path || '');
    if (looksLikeLoginPage(cap.href || href)) {
      log({ skipped: 'login page', site: cap.site, reason });
      return { skipped: `this is a sign-in page for ${cap.site}; nothing to save until you are signed in`, site: cap.site };
    }
    if (!cap.cookies.some(c => isAuthCookie(c.name))) {
      log({ skipped: 'no session cookie', site: cap.site, reason });
      return { skipped: `no session or auth cookie for ${cap.site}; not signed in, nothing saved`, site: cap.site };
    }
  }
  const profiles = list || profilesIndex();
  const cover = coveringProfile(cap.site, profiles);
  let profile = (explicit || cover?.name || cap.site.split('.')[0]).replace(/[^A-Za-z0-9._-]/g, '-');
  if (profile.toLowerCase() === 'default') profile = `${cap.site.split('.')[0]}-login`;
  const res = await importInto(CDPClass, profile, cap);
  const had = profiles.find(p => p.name === profile);
  const reg = api('POST', '/api/browser/save-profile', {
    name: profile,
    host: cap.site,
    label: had?.label ? '' : `Logins for ${cap.site}, saved from Ethan's Chrome over CDP on ${new Date().toISOString().slice(0, 10)}.`,
  });
  const out = {
    profile, site: cap.site, appended_to_existing: !!had, reason, ...res,
    registered: !reg.error, register_error: reg.error || undefined,
  };
  log(out);
  return out;
}
