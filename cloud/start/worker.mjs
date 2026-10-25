// amux Cloud signup, as one Cloudflare Worker (owner design, 2026-10-17): a static page at
// amux.io/start, Stripe Checkout, and this function. No control-plane server and no database:
// a customer's server is standalone, and what links it to its subscription lives in Hetzner
// labels (amux-sub, amux-slug) and Stripe metadata.
//
//   POST /api/checkout       {plan, slug, email, pw_hash, source, ph, ab}  -> {url} of a Stripe Checkout session
//   GET  /api/slug?name=     -> {available, reason}
//   GET  /api/status?session_id=cs_...  -> {state: paying|creating|booting|ready|failed, url}
//   POST /api/billing        {email} -> emails a Stripe billing-portal link
//   POST /api/apple/servers  {identity_token} -> the Apple ID's servers, each with a two-minute sign-in link
//   POST /api/delete         {email} -> emails a one-hour link to /start/delete/?t=...
//   POST /api/delete/confirm {t, slug} -> ends the subscription now and erases the server,
//                            its address, backups and snapshots (no 30-day keep)
//   POST /webhooks/stripe    checkout.session.completed: create the server and its DNS record
//                            customer.subscription.deleted: snapshot it, then (cron) delete it
//   GET  /api/ops            (x-ops-key) the operator audit: books against servers, margin, funnel
//   cron 13:00 UTC daily     the operator audit; emails hello@amux.io only when something is wrong (and Mondays)
//                            A new subscription from the same email within those 30 days boots
//                            from that snapshot: the customer's data comes back, with the new password.
//   cron every minute        ready emails; delete servers whose final snapshot is done;
//                            delete snapshots older than 30 days; finish any paid signup the
//                            webhook did not (reconcile); check each server every 10 minutes;
//                            once an hour read each server's activation report (counts only) and
//                            send any setup nudge it makes due (only with NUDGES=on)
//
// Bindings (cloud/start/deploy.sh): STRIPE_SECRET_KEY, STRIPE_WEBHOOK_SECRET, HCLOUD_TOKEN,
// APPLE_BUNDLE_ID, DOOR_ENTRY_SECRET, CF_TOKEN, CF_ZONE_ID, GMAIL_OAUTH, GOLDEN_IMAGE, PLANS (JSON), MODE (live|test), TEST_KEY,
// OPS_KEY, POSTHOG_KEY and POSTHOG_PROJECT (the operator audit), NUDGES (on = send activation emails to customers; default off).

const SITE = 'https://amux.io';
const DOMAIN = 'amux.io';
const SNAPSHOT_DAYS = 30;
const RESERVED = new Set(('www api cloud control admin status mail app amux start docs blog help support billing accounts ' +
  'clerk clkmail demo sandbox login signin signup pricing dashboard console staging dev test cdn static assets ios ' +
  'android team security cp-test concierge').split(' '));
const BRANDING = { display_name: 'amux Cloud', icon: { type: 'url', url: 'https://amux.io/icon.png' },
  logo: { type: 'url', url: 'https://amux.io/icon.png' }, button_color: '#0b0f14' };

// ------------------------------------------------------------------ helpers
const json = (body, status = 200, extra = {}) => new Response(JSON.stringify(body), {
  status, headers: { 'content-type': 'application/json', 'cache-control': 'no-store', ...extra } });

function cors(req) {
  const o = req.headers.get('origin') || '';
  const ok = o === SITE || o === 'https://www.amux.io' || /^http:\/\/(localhost|127\.0\.0\.1)(:\d+)?$/.test(o);
  return ok ? { 'access-control-allow-origin': o, 'access-control-allow-headers': 'content-type,x-test-key',
                'access-control-allow-methods': 'GET,POST,OPTIONS', vary: 'origin' } : {};
}

function form(obj, prefix = '', out = []) {
  for (const [k, v] of Object.entries(obj)) {
    if (v === undefined || v === null) continue;
    const key = prefix ? `${prefix}[${k}]` : k;
    if (Array.isArray(v)) v.forEach((x, i) => typeof x === 'object' ? form(x, `${key}[${i}]`, out) : out.push([`${key}[${i}]`, String(x)]));
    else if (typeof v === 'object') form(v, key, out);
    else out.push([key, String(v)]);
  }
  return out;
}

async function stripe(env, method, path, params) {
  const q = params ? new URLSearchParams(form(params)).toString() : '';
  const url = 'https://api.stripe.com/v1' + path + (method === 'GET' && q ? '?' + q : '');
  const r = await fetch(url, { method, headers: { authorization: `Bearer ${env.STRIPE_SECRET_KEY}`,
    'content-type': 'application/x-www-form-urlencoded' }, body: method === 'GET' ? undefined : q });
  const d = await r.json();
  if (!r.ok) throw new Error(`stripe ${r.status}: ${d.error?.message || JSON.stringify(d)}`);
  return d;
}

async function hetzner(env, method, path, body) {
  const r = await fetch('https://api.hetzner.cloud/v1' + path, { method, headers: {
    authorization: `Bearer ${env.HCLOUD_TOKEN}`, 'content-type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined });
  const text = await r.text();
  const d = text ? JSON.parse(text) : {};
  if (!r.ok) { const e = new Error(`hetzner ${r.status}: ${d.error?.code || ''} ${d.error?.message || text}`); e.code = d.error?.code; e.status = r.status; throw e; }
  return d;
}

async function cf(env, method, path, body) {
  const r = await fetch('https://api.cloudflare.com/client/v4' + path, { method, headers: {
    authorization: `Bearer ${env.CF_TOKEN}`, 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });
  const d = await r.json();
  if (!d.success) { const e = new Error(`cloudflare ${r.status}: ${JSON.stringify(d.errors)}`); e.status = r.status; throw e; }
  return d.result;
}

// A delete that finds the thing already gone has done its job. The Stripe webhook and the delete
// page both erase an account, and the one that loses the race gets 404 for the server.
async function gone(f) {
  try { return await f(); } catch (e) { if (e.status === 404 || / 404:/.test(e.message)) return null; throw e; }
}

const plans = env => JSON.parse(env.PLANS).plans;
const host = slug => `${slug}.${DOMAIN}`;
const log = (event, fields = {}) => console.log(JSON.stringify({ event, ...fields }));

// ------------------------------------------------------------------ funnel events
// The signup page puts its PostHog visitor id (ph) and A/B variants (ab) in the order's metadata, so
// the paid, ready and canceled steps are recorded from here, under the same visitor as the site's
// events and out of reach of ad blockers (scripts/site-funnel.py reads them). The id is derived from
// the Stripe event, so a redelivered webhook is the same event to PostHog. Never fails an order.
const PH_PROJECT_KEY = 'phc_Ckeacj8y8X8YiLkwHBcpBsJVSnsFKohch2vMv9sJYmE6';   // public, the same one site.js sends
async function eventUuid(seed) {
  const h = [...new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(seed)))].slice(0, 16);
  h[6] = (h[6] & 0x0f) | 0x40; h[8] = (h[8] & 0x3f) | 0x80;
  const x = h.map(b => b.toString(16).padStart(2, '0')).join('');
  return `${x.slice(0, 8)}-${x.slice(8, 12)}-${x.slice(12, 16)}-${x.slice(16, 20)}-${x.slice(20)}`;
}
async function track(env, event, seed, meta, props = {}) {
  try {
    const ab = {};
    for (const kv of (meta?.ab || '').split(',')) { const [k, v] = kv.split('='); if (k && v) ab['ab_' + k] = v; }
    const r = await fetch('https://us.i.posthog.com/i/v0/e/', { method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ api_key: PH_PROJECT_KEY, event, uuid: await eventUuid(seed), distinct_id: meta?.ph || `cloud:${meta?.slug}`,
        properties: { plan: meta?.plan, source: meta?.source, test: env.MODE !== 'live', server_side: true,
                      $process_person_profile: false, ...ab, ...props } }) });
    log(r.ok ? 'track.sent' : 'track.failed', { name: event, status: r.status });
  } catch (e) { log('track.failed', { name: event, error: String(e.message || e).slice(0, 120) }); }
}

function slugProblem(slug) {
  if (!/^[a-z][a-z0-9-]{2,29}$/.test(slug || '') || slug.endsWith('-')) return 'Use 3 to 30 lowercase letters, digits or dashes, starting with a letter.';
  if (RESERVED.has(slug) || slug.startsWith('synthetic')) return 'That name is reserved.';
  return null;
}

async function slugTaken(env, slug) {
  const recs = await cf(env, 'GET', `/zones/${env.CF_ZONE_ID}/dns_records?name=${host(slug)}`);
  if (recs.length) return true;
  const s = await hetzner(env, 'GET', `/servers?label_selector=amux-slug=${slug}`);
  return s.servers.length > 0;
}

// A paid order owns its name before its server and DNS record exist (Hetzner can take minutes, or
// fail and be retried by the cron). Without this a second visitor, or the same one in another tab,
// could open a new payable checkout for a name already bought (codex e2e round 3, C15-3).
async function paidOrder(env, slug) {
  let after;
  for (let i = 0; i < 10; i++) {
    const page = await stripe(env, 'GET', '/subscriptions', { status: 'active', limit: 100, ...(after ? { starting_after: after } : {}) });
    const hit = page.data.find(s => s.metadata?.slug === slug);
    if (hit) return hit;
    if (!page.has_more || !page.data.length) return null;
    after = page.data[page.data.length - 1].id;
  }
  return null;
}

// ------------------------------------------------------------------ Stripe signature
async function verifyStripe(env, body, header) {
  const parts = Object.fromEntries((header || '').split(',').map(p => p.split('=')));
  const t = parts.t, v1 = (header || '').split(',').filter(p => p.startsWith('v1=')).map(p => p.slice(3));
  if (!t || !v1.length || Math.abs(Date.now() / 1000 - Number(t)) > 300) return false;
  const key = await crypto.subtle.importKey('raw', new TextEncoder().encode(env.STRIPE_WEBHOOK_SECRET),
    { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  const sig = new Uint8Array(await crypto.subtle.sign('HMAC', key, new TextEncoder().encode(`${t}.${body}`)));
  const hex = [...sig].map(b => b.toString(16).padStart(2, '0')).join('');
  return v1.some(s => s.length === hex.length && [...s].reduce((a, c, i) => a | (c.charCodeAt(0) ^ hex.charCodeAt(i)), 0) === 0);
}

// ------------------------------------------------------------------ email (Gmail API as hello@amux.io)
// GMAIL_SA: a service account key with domain-wide delegation for gmail.send, so the Worker mints
// its own hello@amux.io tokens and nothing expires. A user grant (GMAIL_OAUTH) dies whenever the
// amux.io Workspace session policy ends it (2026-10-17: every customer email failed with 502).
const MAIL_FROM = 'hello@amux.io';
let mailToken = { value: '', exp: 0, cred: '' };
const b64url = s => btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
async function tokenRequest(via, cred) {
  if (via === 'service_account') {
    const k = JSON.parse(atob(cred)), now = Math.floor(Date.now() / 1000);
    const unsigned = b64url(JSON.stringify({ alg: 'RS256', typ: 'JWT' })) + '.' + b64url(JSON.stringify({ iss: k.client_email,
      sub: MAIL_FROM, scope: 'https://www.googleapis.com/auth/gmail.send', aud: 'https://oauth2.googleapis.com/token', iat: now, exp: now + 3600 }));
    const der = Uint8Array.from(atob(k.private_key.replace(/-----[^-]+-----|\s/g, '')), c => c.charCodeAt(0));
    const key = await crypto.subtle.importKey('pkcs8', der, { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' }, false, ['sign']);
    const sig = new Uint8Array(await crypto.subtle.sign('RSASSA-PKCS1-v1_5', key, new TextEncoder().encode(unsigned)));
    return { grant_type: 'urn:ietf:params:oauth:grant-type:jwt-bearer', assertion: unsigned + '.' + b64url(String.fromCharCode(...sig)) };
  }
  const c = JSON.parse(atob(cred));
  return { client_id: c.client_id, client_secret: c.client_secret, refresh_token: c.refresh_token, grant_type: 'refresh_token' };
}
async function mailAccessToken(env) {
  // The service account first; the user grant still works while delegation is not yet approved.
  const creds = [['service_account', env.GMAIL_SA], ['user_grant', env.GMAIL_OAUTH]].filter(([, c]) => c);
  if (!creds.length) { log('email.not_configured', {}); return ''; }
  for (const [via, cred] of creds) {
    if (mailToken.cred === cred && mailToken.exp > Date.now() + 60e3) return mailToken.value;
    const r = await fetch('https://oauth2.googleapis.com/token', { method: 'POST', body: new URLSearchParams(await tokenRequest(via, cred)) });
    const t = await r.json().catch(() => ({}));
    if (t.access_token) {
      mailToken = { value: t.access_token, exp: Date.now() + (t.expires_in || 0) * 1000, cred };
      return t.access_token;
    }
    log('email.token_failed', { via, status: r.status, error: t.error || '' });
  }
  return '';
}
async function sendMail(env, to, subject, text) {
  const access = await mailAccessToken(env);
  if (!access) return false;
  const msg = `From: amux Cloud <hello@amux.io>\r\nTo: ${to}\r\nSubject: ${subject}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n${text}\r\n\r\n-- \r\namux Cloud\r\n`;
  const raw = btoa(unescape(encodeURIComponent(msg))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  const r = await fetch('https://gmail.googleapis.com/gmail/v1/users/me/messages/send', { method: 'POST',
    headers: { authorization: `Bearer ${access}`, 'content-type': 'application/json' }, body: JSON.stringify({ raw }) });
  if (r.status === 401) mailToken = { value: '', exp: 0, cred: '' };
  log(r.ok ? 'email.sent' : 'email.failed', { subject, status: r.status });
  return r.ok;
}

// ------------------------------------------------------------------ provisioning
// First boot runs the bootstrap from main, fetched over the image's own checkout, so a server
// created from an older golden image still gets today's setup. The image's copy is the fallback.
const BOOT = [
  'set -a; . /etc/amux/standalone.env; set +a',
  'H=$(getent passwd amux | cut -d: -f6); B=$H/Dev/amux/cloud/desktop/bootstrap.sh',
  'runuser -u amux -- git -C $H/Dev/amux pull -q --ff-only >/var/log/amux-standalone.log 2>&1 || echo CHECKOUT_PULL_FAILED >>/var/log/amux-standalone.log',
  '[ -f $B ] || B=/opt/amux/bootstrap.sh',
  'bash $B >>/var/log/amux-standalone.log 2>&1 || echo BOOTSTRAP_FAILED >>/var/log/amux-standalone.log',
].join('; ');
// The page where a server's owner manages billing or deletes the account, linked from the
// server's Settings. A test-mode server's link carries the test function's address.
function accountUrl(env, email) {
  const q = new URLSearchParams({ email });
  if (env.MODE === 'test' && env.SELF_URL) q.set('api', env.SELF_URL);
  return `${SITE}/start/billing/?${q}`;
}
function userData(env, slug, email, pwHash, entryKey) {
  // Standalone: the golden image's bootstrap gets its domain and its one owner login. The
  // password arrives already hashed (door.py scrypt format); nothing else about the customer.
  // `owner`: no member token, so amux treats this login as the host owner (Settings can invite teammates).
  const envFile = [`AMUX_DOMAIN='${host(slug)}'`, `AMUX_WEB_USERS='${email}:${pwHash}:${email}:owner'`,
    `AMUX_ACCOUNT_URL='${accountUrl(env, email)}'`,
    `AMUX_SKIP_PACKAGES=1`, ...(entryKey ? [`AMUX_ENTRY_SECRET='${entryKey}'`] : [])].join('\n');
  return `#cloud-config
write_files:
  - path: /etc/amux/standalone.env
    permissions: '0600'
    content: |
${envFile.split('\n').map(l => '      ' + l).join('\n')}
runcmd:
  - [bash, -c, "${BOOT}"]
`;
}

async function goldenImage(env) {
  if (env.GOLDEN_IMAGE) return Number(env.GOLDEN_IMAGE);
  const d = await hetzner(env, 'GET', '/images?type=snapshot&label_selector=amux-golden&sort=created:desc');
  return d.images[0].id;
}

// The newest unexpired final snapshot of a server this email owned. Matched through the old
// subscription's customer, because a canceled name is free for anyone after its server is gone.
async function finalSnapshotFor(env, email) {
  if (!email) return null;
  const imgs = await hetzner(env, 'GET', '/images?type=snapshot&label_selector=amux-cloud-final=1');
  const mine = [];
  for (const i of imgs.images) {
    if (i.status !== 'available' || Number(i.labels['amux-expires'] || 0) * 1000 < Date.now()) continue;
    const old = await stripe(env, 'GET', `/subscriptions/${i.labels['amux-sub']}`, { expand: ['customer'] }).catch(() => null);
    if ((old?.customer?.email || '').toLowerCase() === email.toLowerCase()) mine.push(i);
  }
  return mine.sort((a, b) => Date.parse(b.created) - Date.parse(a.created))[0] || null;
}

// Idempotent, and safe to run again after a partial failure: the webhook calls it, Stripe
// redelivers it, and the cron's reconcile() calls it for any paid subscription whose signup is
// not finished (pw_hash still in its metadata). Each step checks before it acts, and pw_hash is
// cleared only after the server, its backups and its DNS record all exist.
async function provision(env, { sub, meta, email }) {
  if (!sub || !meta?.slug || !meta?.pw_hash) throw new Error(`subscription ${sub} lacks signup data`);
  // Stripe redelivers a checkout event for up to 3 days, and the event still carries the signup
  // data after the account is gone, so only a live subscription gets a server. Without this a
  // retried delivery rebuilt an erased account an hour after its deletion (fin-fbb1, 2026-10-19). It comes before the
  // resume branch too: a server still being erased must not get its address published again (C12-6).
  const current = await stripe(env, 'GET', `/subscriptions/${sub}`);
  if (!LIVE.includes(current.status) || current.metadata?.erase === '1') {
    log('provision.skipped_ended', { sub, slug: meta.slug, status: current.status, erase: current.metadata?.erase === '1' });
    return null;
  }
  const existing = await hetzner(env, 'GET', `/servers?label_selector=amux-sub=${sub}`);
  if (existing.servers.length) {
    const srv = existing.servers[0];
    log('provision.resume', { sub, server: srv.id });   // a redelivery, or a retry after a partial failure
    return finish(env, sub, srv.labels['amux-slug'], srv);
  }
  let slug = meta.slug;
  if (await slugTaken(env, slug)) slug = `${slug}-${sub.slice(-4).toLowerCase()}`;   // lost a race since checkout
  const plan = plans(env).find(p => p.id === meta.plan) || plans(env)[0];
  const golden = await goldenImage(env);
  const prior = await finalSnapshotFor(env, email || meta.email);
  const labels = { 'amux-cloud': '1', 'amux-standalone': '1', 'amux-sub': sub, 'amux-slug': slug,
                   'amux-plan': plan.id, 'amux-mode': env.MODE, ...(prior ? { 'amux-restored-from': String(prior.id) } : {}) };
  let image = prior ? prior.id : golden;
  let srv, lastErr;
  // Hetzner sometimes has no capacity for a type in a location (412 resource_unavailable, seen
  // 2026-10-17 for cx43 in all three at once). Try each; if all refuse, throw, and the cron
  // retries every minute until one has room, emailing the operator if it takes over 10 minutes.
  for (const location of ['fsn1', 'nbg1', 'hel1']) {
    try {
      srv = (await hetzner(env, 'POST', '/servers', { name: `amux-${slug}`, server_type: plan.server_type, image,
        location, labels, user_data: userData(env, slug, email, meta.pw_hash, await entryKey(env, slug)), start_after_create: true })).server;
      break;
    } catch (e) {
      lastErr = e;
      // A snapshot that cannot boot on this plan (a smaller disk) must not block the signup: start
      // fresh, and the snapshot stays until it expires.
      if (image !== golden && !['resource_unavailable', 'placement_error'].includes(e.code)) {
        log('provision.restore_failed', { sub, slug, image, error: String(e).slice(0, 200) });
        image = golden; delete labels['amux-restored-from'];
        try {
          srv = (await hetzner(env, 'POST', '/servers', { name: `amux-${slug}`, server_type: plan.server_type, image,
            location, labels, user_data: userData(env, slug, email, meta.pw_hash, await entryKey(env, slug)), start_after_create: true })).server;
          break;
        } catch (e2) { lastErr = e2; if (!['resource_unavailable', 'placement_error'].includes(e2.code)) throw e2; continue; }
      }
      if (!['resource_unavailable', 'placement_error'].includes(e.code)) throw e;
    }
  }
  if (!srv) { log('provision.no_capacity', { sub, slug, type: plan.server_type, error: String(lastErr).slice(0, 200) }); throw lastErr; }
  log('provision.created', { sub, slug, server: srv.id, plan: plan.id, mode: env.MODE, restored_from: labels['amux-restored-from'] || null });
  return finish(env, sub, slug, srv);
}

async function finish(env, sub, slug, srv) {
  // Hetzner's daily backups (7 kept, +20% of the server price): a restorable copy of the whole disk
  // without anything on the server to maintain. The monitor re-enables them if this fails.
  if (!srv.backup_window)
    await hetzner(env, 'POST', `/servers/${srv.id}/actions/enable_backup`).catch(e => log('backup.enable_failed', { slug, error: String(e).slice(0, 200) }));
  await ensureDns(env, sub, slug, srv);
  // The hash has done its job (it is on the server now); an empty value removes it from Stripe and
  // marks the signup finished, so reconcile() stops looking at this subscription.
  await stripe(env, 'POST', `/subscriptions/${sub}`, { metadata: { slug, hetzner_id: srv.id, pw_hash: '' } });
  log('provision.finished', { sub, slug, server: srv.id });
  return srv;
}

async function ensureDns(env, sub, slug, srv) {
  const ip = srv.public_net.ipv4.ip;
  const recs = await cf(env, 'GET', `/zones/${env.CF_ZONE_ID}/dns_records?name=${host(slug)}`);
  if (recs.some(r => r.type === 'A' && r.content === ip)) return false;
  for (const r of recs) await cf(env, 'DELETE', `/zones/${env.CF_ZONE_ID}/dns_records/${r.id}`);
  await cf(env, 'POST', `/zones/${env.CF_ZONE_ID}/dns_records`, { type: 'A', name: host(slug), content: ip,
    proxied: false, ttl: 60, comment: `amux cloud ${sub}` });
  log('dns.written', { slug, replaced: recs.length });
  return true;
}

// Paid subscriptions whose signup is not finished: the webhook failed (no capacity, an API
// error, a DNS write that did not happen) or never arrived. Runs every minute; the webhook gets
// the first 90 seconds to itself.
async function reconcile(env) {
  const since = Math.floor(Date.now() / 1000) - 7 * 86400;
  const subs = (await stripe(env, 'GET', '/subscriptions', { status: 'active', 'created[gte]': since, limit: 100,
                                                              expand: ['data.customer'] })).data;
  for (const s of subs) {
    const m = s.metadata || {};
    if (!m.slug || !m.pw_hash) continue;
    const age = Date.now() / 1000 - s.created;
    if (age < 90) continue;
    try {
      await provision(env, { sub: s.id, meta: m, email: (s.customer?.email || m.email || '').toLowerCase() });
      log('reconcile.provisioned', { sub: s.id, slug: m.slug, age_s: Math.round(age) });
    } catch (e) {
      log('reconcile.failed', { sub: s.id, slug: m.slug, age_s: Math.round(age), error: String(e.message || e).slice(0, 200) });
      if (age > 600 && !m.alerted) {
        await sendMail(env, OPERATOR, `[amux cloud ${env.MODE}] paid order ${m.slug} has no server after ${Math.round(age / 60)} min`,
          `Subscription ${s.id} (${m.slug}, plan ${m.plan}) was paid ${Math.round(age / 60)} minutes ago and its server is not finished.\r\n\r\n` +
          `Last error: ${String(e.message || e).slice(0, 400)}\r\n\r\nThe Worker keeps retrying every minute; nothing needs doing unless this repeats.`);
        await stripe(env, 'POST', `/subscriptions/${s.id}`, { metadata: { alerted: String(Math.floor(Date.now() / 1000)) } });
      }
    }
  }
}

async function erase(env, sub, slug) {
  // The customer asked to delete their account: server (Hetzner drops its backups with it),
  // address and every snapshot, now.
  const d = await hetzner(env, 'GET', `/servers?label_selector=amux-sub=${sub}`);
  for (const s of d.servers) await gone(() => hetzner(env, 'DELETE', `/servers/${s.id}`));
  // The address goes by name, not only through a listed server: when the webhook and the delete
  // page race, the second finds no server, and the record was left pointing at a deleted IP
  // (e2efull-ba83, 2026-10-18). A name bought again by another subscription keeps its record.
  let dns = 0;
  for (const name of new Set([slug, ...d.servers.map(s => s.labels['amux-slug'])].filter(Boolean))) {
    const owners = await hetzner(env, 'GET', `/servers?label_selector=amux-slug=${name}`);
    if (owners.servers.some(s => s.labels['amux-sub'] !== sub)) { log('account.erase_dns_kept', { sub, slug: name }); continue; }
    for (const r of await cf(env, 'GET', `/zones/${env.CF_ZONE_ID}/dns_records?name=${host(name)}`)) {
      await gone(() => cf(env, 'DELETE', `/zones/${env.CF_ZONE_ID}/dns_records/${r.id}`)); dns++;
    }
  }
  const imgs = await hetzner(env, 'GET', `/images?type=snapshot&label_selector=amux-sub=${sub}`);
  for (const i of imgs.images) await gone(() => hetzner(env, 'DELETE', `/images/${i.id}`));
  // A restored server deleted before it was ready still points at the snapshot it came from.
  for (const s of d.servers) if (s.labels['amux-restored-from'])
    await gone(() => hetzner(env, 'DELETE', `/images/${s.labels['amux-restored-from']}`));
  log('account.erased', { sub, servers: d.servers.length, dns, snapshots: imgs.images.length });
}

async function deprovision(env, sub, eraseNow, slug) {
  // Ended subscription: power off, keep a snapshot for 30 days (resubscribing can restore it),
  // and let the cron delete the server and its address once the snapshot exists.
  if (eraseNow) return erase(env, sub, slug);
  const d = await hetzner(env, 'GET', `/servers?label_selector=amux-sub=${sub}`);
  for (const s of d.servers) {
    if (s.labels['amux-ending']) continue;
    const expires = Math.floor(Date.now() / 1000) + SNAPSHOT_DAYS * 86400;
    const img = await hetzner(env, 'POST', `/servers/${s.id}/actions/create_image`, { type: 'snapshot',
      description: `amux cloud final ${s.labels['amux-slug']}`, labels: { 'amux-cloud-final': '1',
        'amux-sub': sub, 'amux-slug': s.labels['amux-slug'], 'amux-expires': String(expires) } });
    await hetzner(env, 'PUT', `/servers/${s.id}`, { labels: { ...s.labels, 'amux-ending': String(img.image.id) } });
    log('deprovision.snapshot', { sub, server: s.id, image: img.image.id });
  }
}

// ------------------------------------------------------------------ status
async function status(env, sessionId) {
  const s = await stripe(env, 'GET', `/checkout/sessions/${sessionId}`);
  if (s.status !== 'complete') return { state: 'paying' };
  const sub = s.subscription;
  const waited_s = Math.max(0, Math.round(Date.now() / 1000 - s.created));
  const d = await hetzner(env, 'GET', `/servers?label_selector=amux-sub=${sub}`);
  if (!d.servers.length) return { state: 'creating', waited_s };
  const srv = d.servers[0], slug = srv.labels['amux-slug'], url = `https://${host(slug)}`;
  if (srv.labels['amux-ready'] === '1') return { state: 'ready', url, email: s.customer_details?.email };
  return { state: (await isUp(slug)) ? 'ready' : 'booting', url, email: s.customer_details?.email, waited_s };
}

// Ready means usable: the door answers, amux-server answers behind it, its dashboard loads, and the
// first self-update has run (so no update restart lands in the customer's first minutes).
async function isUp(slug) {
  try {
    const r = await fetch(`https://${host(slug)}/_door/status`, { cf: { cacheTtl: 0 } });
    if (r.status !== 200) return false;
    const st = await r.json();
    return Boolean(st.live_commit && st.dashboard && st.verdict && st.verdict !== 'unknown');
  } catch { return false; }
}

// ------------------------------------------------------------------ cron
async function tick(env) {
  const d = await hetzner(env, 'GET', `/servers?label_selector=amux-standalone=1,amux-mode=${env.MODE}`);
  for (const s of d.servers) {
    const slug = s.labels['amux-slug'];
    if (s.labels['amux-ending']) {
      const img = await hetzner(env, 'GET', `/images/${s.labels['amux-ending']}`).catch(() => null);
      if (img && img.image.status === 'available') {
        await hetzner(env, 'DELETE', `/servers/${s.id}`);
        for (const r of await cf(env, 'GET', `/zones/${env.CF_ZONE_ID}/dns_records?name=${host(slug)}`))
          await cf(env, 'DELETE', `/zones/${env.CF_ZONE_ID}/dns_records/${r.id}`);
        log('deprovision.deleted', { slug, server: s.id });
      }
      continue;
    }
    if (s.labels['amux-ready'] !== '1' && await isUp(slug)) {
      const sub = await stripe(env, 'GET', `/subscriptions/${s.labels['amux-sub']}`, { expand: ['customer'] });
      const email = sub.customer?.email;
      if (email && !email.startsWith('synthetic+')) {
        await sendMail(env, email, 'Your amux server is ready',
          `Your server is ready: https://${host(slug)}\r\n\r\n` +
          (s.labels['amux-restored-from'] ? `It was restored from your previous server, with your workers, board and files. ` : '') +
          `Sign in with this email address and the password you chose at signup. ` +
          `Then connect a model: sign in with Claude, ChatGPT or Google, or paste an API key.\r\n\r\n` +
          `Billing and cancellation: ${SITE}/start/billing/`);
      }
      await hetzner(env, 'PUT', `/servers/${s.id}`, { labels: { ...s.labels, 'amux-ready': '1' } });
      await track(env, 'cloud_ready', 'ready:' + s.labels['amux-sub'], sub.metadata,
                  { minutes_to_ready: Math.round((Date.now() - Date.parse(s.created)) / 60000) });
      // The data is live again, so the snapshot it came from is no longer the customer's only copy.
      if (s.labels['amux-restored-from']) {
        await gone(() => hetzner(env, 'DELETE', `/images/${s.labels['amux-restored-from']}`));
        log('restore.snapshot_deleted', { slug, image: s.labels['amux-restored-from'] });
      }
      log('provision.ready', { slug, ms_since_create: Date.now() - Date.parse(s.created) });
    }
  }
  await monitor(env, d.servers);
  await reconcile(env);
  const imgs = await hetzner(env, 'GET', '/images?type=snapshot&label_selector=amux-cloud-final=1');
  for (const i of imgs.images) {
    if (Number(i.labels['amux-expires'] || 0) * 1000 < Date.now()) {
      await hetzner(env, 'DELETE', `/images/${i.id}`);
      log('snapshot.expired', { image: i.id, slug: i.labels['amux-slug'] });
    }
  }
}

// ------------------------------------------------------------------ monitor
// Each ready server is checked every 10 minutes (staggered by id) through its public, secret-free
// /_door/status: is it reachable, did its last self-update succeed, is the updater still running,
// are backups on. Two bad checks in a row email the operator; recovery emails again. State lives
// in the server's labels (amux-fail = consecutive bad checks, amux-alert = when we last emailed).
const OPERATOR = 'hello@amux.io';
async function serverProblem(env, s) {
  if (!s.backup_window) return 'backups are off';
  let st;
  try {
    const r = await fetch(`https://${host(s.labels['amux-slug'])}/_door/status`, { cf: { cacheTtl: 0 } });
    if (r.status !== 200) return `status page answered HTTP ${r.status}`;
    st = await r.json();
  } catch (e) { return `unreachable (${String(e).slice(0, 80)})`; }
  const age = Date.now() / 1000 - (st.checked_at || 0);
  if (!st.live_commit) return 'amux-server is not answering behind the door';
  if (st.dashboard === false) return 'the dashboard does not load';
  if (st.verdict === 'unknown') return Date.now() - Date.parse(s.created) > 30 * 60e3 ? 'self-updater has never reported' : '';
  if (age > 3 * 3600) return `self-updater last reported ${Math.round(age / 3600)}h ago`;
  if (String(st.verdict).startsWith('failed')) return `self-update ${st.verdict}: ${st.detail || ''}`;
  // Caddy renews about 30 days before expiry, so under 14 means renewal has been failing for weeks.
  if (typeof st.tls_days_left === 'number' && st.tls_days_left < 14)
    return st.tls_days_left < 0 ? 'TLS certificate is expired or invalid' : `TLS certificate expires in ${st.tls_days_left} days (renewal is failing)`;
  return '';
}
async function monitor(env, servers) {
  const minute = new Date().getUTCMinutes();
  for (const s of servers) {
    if (s.labels['amux-ending'] || s.labels['amux-ready'] !== '1' || (s.id + minute) % 10) continue;
    if (!s.backup_window) await hetzner(env, 'POST', `/servers/${s.id}/actions/enable_backup`).catch(() => {});
    await ensureDns(env, s.labels['amux-sub'], s.labels['amux-slug'], s).catch(e => log('dns.repair_failed', { slug: s.labels['amux-slug'], error: String(e).slice(0, 200) }));
    const problem = await serverProblem(env, s).catch(e => `monitor error ${String(e).slice(0, 80)}`);
    const slug = s.labels['amux-slug'], fails = Number(s.labels['amux-fail'] || 0);
    const alerted = Number(s.labels['amux-alert'] || 0), now = Math.floor(Date.now() / 1000);
    const labels = { ...s.labels };
    if (problem) {
      labels['amux-fail'] = String(fails + 1);
      log('monitor.problem', { slug, problem, consecutive: fails + 1, mode: env.MODE });
      if (fails + 1 >= 2 && now - alerted > 6 * 3600) {
        await sendMail(env, OPERATOR, `[amux cloud ${env.MODE}] ${slug}: ${problem}`.slice(0, 150),
          `Server ${host(slug)} (Hetzner ${s.id}, plan ${s.labels['amux-plan']}) has failed ${fails + 1} checks in a row.\r\n\r\n` +
          `Problem: ${problem}\r\n\r\nStatus page: https://${host(slug)}/_door/status`);
        labels['amux-alert'] = String(now);
      }
    } else {
      if (alerted) await sendMail(env, OPERATOR, `[amux cloud ${env.MODE}] ${slug} recovered`, `${host(slug)} passes its checks again.`);
      delete labels['amux-fail']; delete labels['amux-alert'];
      log('monitor.ok', { slug });
    }
    // Once an hour (amux-rep holds the hour it last ran): the activation report, and any nudge it makes due.
    const hour = String(Math.floor(Date.now() / 3600e3));
    if (labels['amux-rep'] !== hour) {
      const rep = await fetchReport(env, slug);
      labels['amux-rep'] = hour;
      if (rep.fetched) {
        labels['amux-act'] = stage(rep.report);
        await nudge(env, s, rep.report, labels).catch(e => log('nudge.failed', { slug, error: String(e).slice(0, 200) }));
      }
      log('activation.report', { slug, fetched: rep.fetched, why: rep.why, stage: labels['amux-act'], unmeasured: rep.report?.unmeasured });
    }
    if (JSON.stringify(labels) !== JSON.stringify(s.labels)) await hetzner(env, 'PUT', `/servers/${s.id}`, { labels });
  }
}

// ------------------------------------------------------------------ activation and friction
// Each server's door answers GET /_door/report with counts only: sign-ins, which model providers
// are connected (booleans), workers, cards, schedules, error groups, disk. Never content. The token
// is signed with the same per-server key as the two-minute sign-in links, with scope "report" and
// no person in it, so it cannot sign anyone in (cloud/desktop/door.py verify_report_token).
async function reportToken(env, slug) {
  const key = await entryKey(env, slug);
  if (!key) return '';
  const body = b64u(new TextEncoder().encode(JSON.stringify({ aud: slug, scope: 'report', exp: Math.floor(Date.now() / 1000) + 120 })));
  const k = await crypto.subtle.importKey('raw', new TextEncoder().encode(key), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  return `${body}.${b64u(await crypto.subtle.sign('HMAC', k, new TextEncoder().encode(body)))}`;
}
async function fetchReport(env, slug) {
  const t = await reportToken(env, slug);
  if (!t) return { fetched: false, why: 'DOOR_ENTRY_SECRET not configured' };
  try {
    const r = await fetch(`https://${host(slug)}/_door/report`, { headers: { authorization: `Bearer ${t}` }, cf: { cacheTtl: 0 } });
    // 404 is what an older door answers (before its hourly self-update) and what a refused token gets.
    if (r.status === 404) return { fetched: false, why: 'no report (door not updated yet, or key refused)' };
    if (!r.ok) return { fetched: false, why: `HTTP ${r.status}` };
    return { fetched: true, report: await r.json() };
  } catch (e) { return { fetched: false, why: `unreachable (${String(e).slice(0, 80)})` }; }
}
// Where the customer is in setup, as a label value: not_signed_in, no_model, no_worker, idle, working.
function stage(rep) {
  if (rep.workers?.measured && rep.workers.active_24h > 0) return 'working';
  if (rep.workers?.measured && rep.workers.ever_ran > 0) return 'idle';
  if (rep.signin?.measured && !rep.signin.ever) return 'not_signed_in';
  if (rep.model?.measured && !rep.model.any) return 'no_model';
  if (rep.workers?.measured) return 'no_worker';
  return 'unknown';
}
const ageHours = s => (Date.now() - Date.parse(s.created)) / 3600e3;
// Start simple. Each rule fires only on a measured section, so a probe that did not run never reads as a stuck customer.
function friction(rep, ageH, prevGroups = 0) {
  const out = [], now = Date.now() / 1000;
  const si = rep.signin, mo = rep.model, wk = rep.workers, er = rep.errors, dk = rep.disk;
  if (si?.measured && !si.ever && ageH >= 24) {
    // Sign-ins are counted from when the door started counting; an older server's earlier sign-ins are unknown.
    const trackedH = (now - si.tracking_since) / 3600;
    if (trackedH >= ageH - 1) out.push(`never signed in (${Math.round(ageH)}h since created)`);
    else if (trackedH >= 24) out.push(`no sign-in in the ${Math.round(trackedH)}h since counting began`);
  }
  if (mo?.measured && !mo.any && ageH >= 24) out.push(`no model connected after ${Math.round(ageH)}h`);
  if (wk?.measured && wk.ever_ran === 0 && ageH >= 48) out.push(`no worker has ever run after ${Math.round(ageH)}h`);
  if (er?.measured && (er.groups_24h >= 25 || (er.groups_24h >= 10 && er.groups_24h >= 2 * prevGroups)))
    out.push(`${er.groups_24h} error groups in 24h (yesterday ${prevGroups})`);
  if (si?.measured && si.failed_24h >= 10) out.push(`${si.failed_24h} failed sign-ins in 24h`);
  if (dk?.measured && dk.used_pct > 85) out.push(`disk ${dk.used_pct}% full`);
  return out;
}
function activationRow(slug, ageH, rep) {
  if (!rep.fetched) return { slug, age_h: Math.round(ageH), measured: false, why: rep.why };
  const r = rep.report, n = (sec, k) => (r[sec]?.measured ? r[sec][k] : null);
  return { slug, age_h: Math.round(ageH), measured: Boolean(r.measured), unmeasured: r.unmeasured || [], stage: stage(r),
    signed_in: n('signin', 'ever'), last_seen_at: n('signin', 'last_seen_at'), failed_signins_24h: n('signin', 'failed_24h'),
    model: n('model', 'any'), providers: Object.entries(n('model', 'providers') || {}).filter(([, v]) => v).map(([k]) => k),
    workers: n('workers', 'total'), workers_ever_ran: n('workers', 'ever_ran'), workers_active_24h: n('workers', 'active_24h'),
    tokens_today: n('tokens', 'today'), cards: n('board', 'cards'), schedules: n('schedules', 'total'),
    error_groups_24h: n('errors', 'groups_24h'), disk_used_pct: n('disk', 'used_pct'), commit: n('version', 'commit') };
}

// Activation nudges to the customer, from hello@amux.io. Held unless NUDGES=on (the owner reviews
// customer copy first). Each goes at most once per server (amux-nudged), at least a day apart
// (amux-nudge-at). A server labelled amux-nudge-off (set it when a customer replies "stop") gets none.
const NUDGE_MAX_AGE_H = 14 * 24;
function nudgeText(kind, slug) {
  const url = `https://${host(slug)}`;
  if (kind === 'm') return ['Connect a model to your amux server',
    `Your amux server at ${url} is running, but no AI model is connected yet, so workers cannot start.\r\n\r\n` +
    `To connect one:\r\n\r\n` +
    `1. Open ${url} and sign in with this email address and the password you chose at signup.\r\n\r\n` +
    `2. Click Connect a model at the top of the page, then Sign in with Claude to use your Claude subscription, or paste an Anthropic API key. ChatGPT, Gemini and OpenAI keys work too.\r\n\r\n` +
    `It takes about a minute. If something gets in the way, reply to this email and tell us where you got stuck.\r\n\r\n` +
    `To stop these setup emails, reply "stop".`];
  return ['Start your first worker on amux',
    `Your amux server at ${url} has not run a worker yet. A worker is an AI agent with its own terminal on your server.\r\n\r\n` +
    `To start one:\r\n\r\n` +
    `1. Open ${url} and sign in.\r\n\r\n` +
    `2. Click + at the top, then New worker.\r\n\r\n` +
    `3. Give it a name and tell it what to work on, for example "set up a Python project for my scraper".\r\n\r\n` +
    `If you are stuck, reply to this email and we will help.\r\n\r\n` +
    `To stop these setup emails, reply "stop".`];
}
async function nudge(env, s, rep, labels) {
  const slug = s.labels['amux-slug'], ageH = ageHours(s), now = Math.floor(Date.now() / 1000);
  const sent = new Set((labels['amux-nudged'] || '').split('-').filter(Boolean));
  let due = '';
  if (ageH >= 24 && rep.model?.measured && !rep.model.any && !sent.has('m')) due = 'm';
  else if (ageH >= 72 && rep.workers?.measured && rep.workers.ever_ran === 0 && !sent.has('w')) due = 'w';
  if (!due) return;
  const held = env.NUDGES !== 'on' ? 'NUDGES is not on' : labels['amux-nudge-off'] ? 'customer opted out'
    : ageH > NUDGE_MAX_AGE_H ? 'server older than 14 days' : now - Number(labels['amux-nudge-at'] || 0) < 86400 ? 'a nudge went out in the last day' : '';
  if (held) { log('nudge.held', { slug, kind: due, why: held }); return; }
  const sub = await stripe(env, 'GET', `/subscriptions/${s.labels['amux-sub']}`, { expand: ['customer'] });
  const email = sub.customer?.email;
  if (!email || email.startsWith('synthetic+')) { log('nudge.held', { slug, kind: due, why: 'no customer email' }); return; }
  const [subject, text] = nudgeText(due, slug);
  if (await sendMail(env, email, subject, text)) {
    sent.add(due);
    labels['amux-nudged'] = [...sent].sort().join('-');
    labels['amux-nudge-at'] = String(now);
    log('nudge.sent', { slug, kind: due });
  }
}

// ------------------------------------------------------------------ operator audit
// Once a day (13:00 UTC) the books are checked against the servers, so nobody has to look.
// Fixed without asking: a server whose subscription Stripe says has ended is snapshotted and
// deleted the same way a cancellation is. Reported, because they need a decision: a paying
// customer with no server, a server with no subscription Stripe knows, a server failing its
// checks, a plan whose price no longer covers about 1.5x its server. The email goes out only when
// something is wrong, plus a Monday summary. GET /api/ops (header x-ops-key) returns the same report.
async function allSubs(env) {
  const out = [];
  let after;
  for (let i = 0; i < 20; i++) {
    const page = await stripe(env, 'GET', '/subscriptions', { status: 'all', limit: 100, ...(after ? { starting_after: after } : {}) });
    out.push(...page.data.filter(s => s.metadata?.slug));
    if (!page.has_more || !page.data.length) break;
    after = page.data[page.data.length - 1].id;
  }
  return out;
}
const LIVE = ['active', 'trialing', 'past_due'];
async function funnel(env) {
  if (!env.POSTHOG_KEY || !env.POSTHOG_PROJECT) return { measured: false, why: 'POSTHOG_KEY not configured' };
  const q = "select properties.$pathname as p, count(distinct person_id) from events where event = '$pageview' and " +
            "properties.$host = 'amux.io' and timestamp > now() - interval 1 day and (p like '/cloud%' or p like '/pricing%' or p like '/start%') group by p";
  const r = await fetch(`https://us.posthog.com/api/projects/${env.POSTHOG_PROJECT}/query/`, { method: 'POST',
    headers: { authorization: `Bearer ${env.POSTHOG_KEY}`, 'content-type': 'application/json' },
    body: JSON.stringify({ query: { kind: 'HogQLQuery', query: q } }) });
  if (!r.ok) return { measured: false, why: `PostHog answered HTTP ${r.status}` };
  const visitors = {};
  for (const [path, n] of (await r.json()).results || []) {
    const k = path.split('/')[1] || '/'; visitors[k] = (visitors[k] || 0) + n;
  }
  return { measured: true, visitors_24h: visitors };
}
async function audit(env) {
  const subs = await allSubs(env), bySub = new Map(subs.map(s => [s.id, s]));
  const servers = (await hetzner(env, 'GET', `/servers?label_selector=amux-standalone=1,amux-mode=${env.MODE}`)).servers;
  const problems = [], fixed = [], activation = [], stuck = [], day = Math.floor(Date.now() / 1000) - 86400;
  let mrr = 0, cost = 0;
  for (const s of servers) {
    if (s.labels['amux-ending']) continue;
    const slug = s.labels['amux-slug'], sub = bySub.get(s.labels['amux-sub']);
    const price = (s.server_type?.prices || []).find(x => x.location === s.datacenter?.location?.name) || (s.server_type?.prices || [])[0];
    const monthly = Number(price?.price_monthly?.gross || 0);
    cost += monthly;
    if (!sub) { problems.push(`${slug}: server ${s.id} has no subscription Stripe knows (${s.labels['amux-sub']})`); continue; }
    if (['canceled', 'incomplete_expired'].includes(sub.status) && sub.metadata?.erase === '1') {
      await erase(env, sub.id, slug);   // the customer deleted the account: no snapshot is kept
      fixed.push(`${slug}: account was deleted, server ${s.id} erased`);
      continue;
    }
    if (['canceled', 'incomplete_expired'].includes(sub.status)) {   // the cancellation webhook was missed
      await deprovision(env, sub.id, false);
      fixed.push(`${slug}: subscription ${sub.status}, server snapshotted for deletion`);
      continue;
    }
    if (!LIVE.includes(sub.status)) { problems.push(`${slug}: subscription is ${sub.status} and its server still runs`); continue; }
    const item = sub.items?.data?.[0], amount = (item?.price?.unit_amount || 0) * (item?.quantity || 1) / 100;
    mrr += amount;
    if (monthly && amount && amount < monthly * 1.5)
      problems.push(`${slug}: plan ${s.labels['amux-plan']} pays $${amount.toFixed(2)}/mo for a server costing about ${monthly.toFixed(2)}/mo`);
    if (Number(s.labels['amux-fail'] || 0) >= 2) problems.push(`${slug}: failing its checks (${s.labels['amux-fail']} in a row)`);
    if (s.labels['amux-ready'] !== '1') continue;
    // Is this paying customer actually using it? Fetched live; amux-errg keeps yesterday's error-group count.
    const ageH = ageHours(s), rep = await fetchReport(env, slug);
    activation.push(activationRow(slug, ageH, rep));
    if (!rep.fetched) continue;
    const prev = Number(s.labels['amux-errg'] || 0);
    for (const f of friction(rep.report, ageH, prev)) stuck.push(`${slug}: ${f}`);
    const g = rep.report.errors?.measured ? String(rep.report.errors.groups_24h) : s.labels['amux-errg'];
    const act = stage(rep.report);
    if (g !== s.labels['amux-errg'] || act !== s.labels['amux-act']) {
      const labels = { ...s.labels, 'amux-act': act, ...(g !== undefined ? { 'amux-errg': g } : {}) };
      await hetzner(env, 'PUT', `/servers/${s.id}`, { labels }).catch(e => log('activation.label_failed', { slug, error: String(e).slice(0, 120) }));
    }
  }
  const withServer = new Set(servers.map(s => s.labels['amux-sub']));
  for (const sub of subs)
    if (LIVE.includes(sub.status) && !withServer.has(sub.id) && sub.created < day)
      problems.push(`${sub.metadata.slug}: ${sub.status} subscription ${sub.id} has no server`);
  const pastDue = subs.filter(s => s.status === 'past_due').map(s => s.metadata.slug);
  const checkouts = (await stripe(env, 'GET', '/checkout/sessions', { 'created[gte]': day, limit: 100 })).data
    .filter(c => c.metadata?.slug);
  const report = { mode: env.MODE, measured: true, at: new Date().toISOString(),
    customers: subs.filter(s => LIVE.includes(s.status)).length, servers: servers.length, past_due: pastDue,
    mrr_usd: Math.round(mrr * 100) / 100, server_cost_eur: Math.round(cost * 100) / 100,
    checkouts_24h: { started: checkouts.length, paid: checkouts.filter(c => c.status === 'complete').length },
    funnel: await funnel(env).catch(e => ({ measured: false, why: String(e).slice(0, 120) })), problems, fixed,
    activation, friction: stuck };
  log('ops.audit', { ...report, activation: undefined, activation_rows: activation.length, funnel: undefined, visitors: report.funnel.visitors_24h, funnel_measured: report.funnel.measured });
  return report;
}
async function dailyAudit(env) {
  const r = await audit(env), monday = new Date().getUTCDay() === 1;
  if (!r.problems.length && !r.fixed.length && !r.friction.length && !monday) return r;
  const yn = v => (v === null || v === undefined ? '?' : v ? 'yes' : 'no');
  const row = a => !a.measured && a.why ? `- ${a.slug} (${a.age_h}h): no report, ${a.why}` :
    `- ${a.slug} (${a.age_h}h, ${a.stage}): signed in ${yn(a.signed_in)}, model ${a.model ? a.providers.join('+') || 'yes' : yn(a.model)}, ` +
    `workers ${a.workers ?? '?'} (ever ran ${a.workers_ever_ran ?? '?'}, active 24h ${a.workers_active_24h ?? '?'}), cards ${a.cards ?? '?'}, ` +
    `schedules ${a.schedules ?? '?'}, error groups ${a.error_groups_24h ?? '?'}, disk ${a.disk_used_pct ?? '?'}%` +
    (a.unmeasured.length ? ` (not measured: ${a.unmeasured.join(', ')})` : '');
  const lines = [
    r.problems.length ? `Needs a look:\r\n\r\n${r.problems.map(x => '- ' + x).join('\r\n\r\n')}` : 'Nothing needs a look.',
    r.friction.length ? `Customers stuck in setup:\r\n\r\n${r.friction.map(x => '- ' + x).join('\r\n\r\n')}` : '',
    r.activation.length ? `Activation:\r\n\r\n${r.activation.map(row).join('\r\n\r\n')}` : '',
    r.fixed.length ? `Fixed automatically:\r\n\r\n${r.fixed.map(x => '- ' + x).join('\r\n\r\n')}` : '',
    `Customers: ${r.customers}. Servers: ${r.servers}. Past due: ${r.past_due.length ? r.past_due.join(', ') : 'none'}.`,
    `Revenue: $${r.mrr_usd}/mo list price. Server cost: about EUR ${r.server_cost_eur}/mo.`,
    `Last 24h: ${r.checkouts_24h.started} checkouts started, ${r.checkouts_24h.paid} paid.`,
    r.funnel.measured ? `Visitors (24h): ${Object.entries(r.funnel.visitors_24h).map(([k, v]) => `/${k} ${v}`).join(', ') || 'none'}.`
                      : `Visitors: not measured (${r.funnel.why}).`,
  ].filter(Boolean);
  const stuckSlugs = new Set(r.friction.map(x => x.split(':')[0]));
  const subject = [r.problems.length ? `${r.problems.length} thing(s) need a look` : '',
                   stuckSlugs.size ? `${stuckSlugs.size} stuck customer(s)` : ''].filter(Boolean).join(', ') ||
                  (r.fixed.length ? 'fixed automatically' : 'weekly summary');
  await sendMail(env, OPERATOR, `[amux cloud ${env.MODE}] ${subject}`, lines.join('\r\n\r\n'));
  return r;
}

// ------------------------------------------------------------------ account deletion
async function accountSubs(env, email) {
  const out = [];
  for (const c of (await stripe(env, 'GET', '/customers', { email, limit: 10 })).data)
    for (const s of (await stripe(env, 'GET', '/subscriptions', { customer: c.id, status: 'all', limit: 20 })).data)
      if (s.metadata?.slug) out.push(s);
  return out;
}
const b64u = b => btoa(String.fromCharCode(...new Uint8Array(b))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
async function tokenKey(env) {
  return crypto.subtle.importKey('raw', new TextEncoder().encode('amux-delete:' + env.STRIPE_WEBHOOK_SECRET),
    { name: 'HMAC', hash: 'SHA-256' }, false, ['sign', 'verify']);
}
// ------------------------------------------------------------------ Sign in with Apple (iOS app)
// The app sends Apple's identity token. The answer lists the account's servers, each with a
// two-minute sign-in link that the server's door checks with its own key (/_door/enter).
// Each server's key is derived from DOOR_ENTRY_SECRET and reaches it in user_data at creation.
async function entryKey(env, slug) {
  if (!env.DOOR_ENTRY_SECRET) return '';
  const k = await crypto.subtle.importKey('raw', new TextEncoder().encode(env.DOOR_ENTRY_SECRET),
    { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  const mac = new Uint8Array(await crypto.subtle.sign('HMAC', k, new TextEncoder().encode('amux-entry|' + slug)));
  return [...mac].map(x => x.toString(16).padStart(2, '0')).join('');
}
async function entryLink(env, slug, email) {
  const key = await entryKey(env, slug);
  if (!key) return '';
  const body = b64u(new TextEncoder().encode(JSON.stringify({ sub: email, aud: slug, exp: Math.floor(Date.now() / 1000) + 120, via: 'apple' })));
  const k = await crypto.subtle.importKey('raw', new TextEncoder().encode(key), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  return `https://${host(slug)}/_door/enter?t=${body}.${b64u(await crypto.subtle.sign('HMAC', k, new TextEncoder().encode(body)))}`;
}
let appleKeys = { keys: [], at: 0 };
const unb64u = x => Uint8Array.from(atob(x.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0));
async function appleClaims(env, token) {
  const [h, p, sig] = String(token || '').split('.');
  if (!h || !p || !sig) throw new Error('malformed token');
  const head = JSON.parse(new TextDecoder().decode(unb64u(h))), c = JSON.parse(new TextDecoder().decode(unb64u(p)));
  if (head.alg !== 'RS256') throw new Error('unexpected algorithm');
  if (!appleKeys.keys.some(k => k.kid === head.kid) || Date.now() - appleKeys.at > 3600e3)
    appleKeys = { keys: (await (await fetch('https://appleid.apple.com/auth/keys')).json()).keys || [], at: Date.now() };
  const jwk = appleKeys.keys.find(k => k.kid === head.kid);
  if (!jwk) throw new Error('unknown signing key');
  const key = await crypto.subtle.importKey('jwk', jwk, { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' }, false, ['verify']);
  if (!(await crypto.subtle.verify('RSASSA-PKCS1-v1_5', key, unb64u(sig), new TextEncoder().encode(h + '.' + p))))
    throw new Error('bad signature');
  if (c.iss !== 'https://appleid.apple.com') throw new Error('wrong issuer');
  if (c.aud !== env.APPLE_BUNDLE_ID) throw new Error('token is for another app');
  if (!(c.exp * 1000 > Date.now())) throw new Error('expired');
  return c;
}
// The subscriptions this Apple ID may open: those already linked to it, otherwise those whose
// account email is the verified email in the token (which then links it, so a later sign-in with a
// private relay address still finds them).
async function appleSubs(env, c) {
  const linked = (await stripe(env, 'GET', '/customers/search', { query: `metadata['apple_sub']:'${String(c.sub).replace(/'/g, '')}'` })).data;
  const out = [];
  for (const cu of linked)
    for (const s of (await stripe(env, 'GET', '/subscriptions', { customer: cu.id, status: 'all', limit: 20 })).data)
      if (s.metadata?.slug) out.push(s);
  if (out.length || !c.email || String(c.email_verified) !== 'true') return out;
  const byEmail = await accountSubs(env, String(c.email).toLowerCase());
  for (const cu of new Set(byEmail.map(s => s.customer?.id || s.customer)))
    await stripe(env, 'POST', `/customers/${cu}`, { metadata: { apple_sub: c.sub } });
  return byEmail;
}

async function signToken(env, claims) {
  const body = b64u(new TextEncoder().encode(JSON.stringify(claims)));
  return body + '.' + b64u(await crypto.subtle.sign('HMAC', await tokenKey(env), new TextEncoder().encode(body)));
}
async function verifyToken(env, t) {
  const [body, sig] = String(t).split('.');
  if (!body || !sig) return null;
  const raw = Uint8Array.from(atob(sig.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0));
  if (!(await crypto.subtle.verify('HMAC', await tokenKey(env), raw, new TextEncoder().encode(body)))) return null;
  const c = JSON.parse(atob(body.replace(/-/g, '+').replace(/_/g, '/')));
  return c.exp > Date.now() ? c : null;
}

// ------------------------------------------------------------------ routes
async function route(req, env) {
  const url = new URL(req.url);
  const p = url.pathname;
  if (req.method === 'OPTIONS') return new Response(null, { status: 204, headers: cors(req) });
  if (p === '/health') return json({ ok: true, mode: env.MODE, measured: true });

  if (p === '/api/slug' && req.method === 'GET') {
    const name = (url.searchParams.get('name') || '').toLowerCase();
    const problem = slugProblem(name);
    if (problem) return json({ available: false, reason: problem }, 200, cors(req));
    const taken = await slugTaken(env, name) || !!(await paidOrder(env, name));
    return json({ available: !taken, reason: taken ? 'That name is taken.' : '' }, 200, cors(req));
  }

  if (p === '/api/checkout' && req.method === 'POST') {
    if (env.MODE === 'test' && req.headers.get('x-test-key') !== env.TEST_KEY)
      return json({ error: 'test mode needs its key' }, 403, cors(req));   // test cards must not buy real servers
    const b = await req.json().catch(() => ({}));
    const slug = (b.slug || '').toLowerCase(), email = (b.email || '').trim().toLowerCase();
    const plan = plans(env).find(x => x.id === b.plan);
    const problem = slugProblem(slug) || (!plan && 'Pick a plan.') ||
      (!/^[a-z0-9._+-]+@[a-z0-9-]+(\.[a-z0-9-]+)+$/.test(email) && 'Enter a valid email address.') ||
      (!/^scrypt\$[A-Za-z0-9+/=]{20,40}\$[A-Za-z0-9+/=]{40,50}$/.test(b.pw_hash || '') && 'Password could not be prepared; reload and try again.');
    if (problem) return json({ error: problem }, 400, cors(req));
    if (await slugTaken(env, slug)) return json({ error: 'That name is taken.' }, 409, cors(req));
    const prior = await paidOrder(env, slug);
    if (prior) {
      const mine = (prior.metadata?.email || '').toLowerCase() === email;
      log('checkout.refused_paid_name', { slug, same_email: mine });
      return json({ error: mine
        ? `You already paid for ${host(slug)}. It is being set up; the "ready" email goes to ${email}.`
        : 'That name is taken.' }, 409, cors(req));
    }
    const price = (await stripe(env, 'GET', '/prices', { lookup_keys: [plan.stripe_lookup_key], active: 'true' })).data[0];
    const meta = { slug, plan: plan.id, email, pw_hash: b.pw_hash, source: (b.source || 'start').slice(0, 40),
                   ...(/^[A-Za-z0-9._:-]{1,100}$/.test(b.ph || '') ? { ph: b.ph } : {}),
                   ...(/^[a-z0-9_]+=[a-z0-9_-]+(,[a-z0-9_]+=[a-z0-9_-]+)*$/.test(b.ab || '') ? { ab: b.ab.slice(0, 200) } : {}) };
    const api = env.MODE === 'test' ? `&api=${encodeURIComponent(url.origin)}` : '';
    const s = await stripe(env, 'POST', '/checkout/sessions', {
      mode: 'subscription', line_items: [{ price: price.id, quantity: 1 }], customer_email: email,
      allow_promotion_codes: true, billing_address_collection: 'auto',   // no automatic tax: amux Cloud does not collect EU VAT
      branding_settings: BRANDING, metadata: meta, subscription_data: { metadata: meta },
      success_url: `${SITE}/start/welcome/?session_id={CHECKOUT_SESSION_ID}${api}`,
      cancel_url: `${SITE}/start/?canceled=1`,
    });
    // One payable checkout per server name: a double submit, a second tab or a retry would otherwise
    // leave two open sessions that could both be paid. Keep the newest and expire the rest, so
    // concurrent requests converge on the same session.
    const open = (await stripe(env, 'GET', '/checkout/sessions', { status: 'open', 'created[gte]': Math.floor(Date.now() / 1000) - 86400,
                                                                   limit: 100 })).data.filter(x => x.metadata?.slug === slug);
    if (!open.some(x => x.id === s.id)) open.push(s);
    open.sort((a, b) => (b.created - a.created) || (b.id < a.id ? -1 : 1));
    for (const old of open.slice(1))
      await stripe(env, 'POST', `/checkout/sessions/${old.id}/expire`).catch(e => log('checkout.expire_failed', { slug, error: String(e).slice(0, 120) }));
    log('checkout.created', { slug, plan: plan.id, mode: env.MODE, expired: open.length - 1 });
    return json({ url: open[0].url }, 200, cors(req));
  }

  if (p === '/api/status' && req.method === 'GET') {
    const sid = url.searchParams.get('session_id') || '';
    if (!/^cs_(test|live)_[A-Za-z0-9]+$/.test(sid)) return json({ error: 'bad session' }, 400, cors(req));
    try { return json(await status(env, sid), 200, cors(req)); }
    catch (e) { if (/stripe 404/.test(String(e))) return json({ error: 'no such order' }, 404, cors(req)); throw e; }
  }

  if (p === '/api/billing' && req.method === 'POST') {
    // The answer is the same whether or not the address has an account, so it reveals nothing.
    const email = ((await req.json().catch(() => ({}))).email || '').trim().toLowerCase();
    const custs = email ? (await stripe(env, 'GET', '/customers', { email, limit: 10 })).data : [];
    for (const c of custs) {
      const subs = (await stripe(env, 'GET', '/subscriptions', { customer: c.id, status: 'all', limit: 10 })).data;
      if (!subs.some(s => s.metadata?.slug)) continue;
      const portal = await stripe(env, 'POST', '/billing_portal/sessions', { customer: c.id, return_url: `${SITE}/start/billing/`,
                                                                            ...(env.PORTAL_CONFIG ? { configuration: env.PORTAL_CONFIG } : {}) });
      // A send that failed is reported, so the page can say so instead of "a link is on its way".
      if (!(await sendMail(env, email, 'Manage your amux Cloud billing',
        `Open this link within an hour to change your card, see invoices or cancel:\r\n\r\n${portal.url}`)))
        return json({ error: 'email could not be sent' }, 502, cors(req));
      break;
    }
    return json({ ok: true }, 200, cors(req));
  }

  if (p === '/api/ops' && req.method === 'GET') {
    if (!env.OPS_KEY || req.headers.get('x-ops-key') !== env.OPS_KEY) return json({ error: 'not found' }, 404);
    return json(await audit(env));
  }

  if (p === '/api/apple/servers' && req.method === 'POST') {
    const b = await req.json().catch(() => ({}));
    let c;
    try { c = await appleClaims(env, b.identity_token); }
    catch (e) {
      log('apple.refused', { reason: e.message });
      return json({ error: 'Apple sign-in could not be verified. Try again.' }, 401, cors(req));
    }
    const live = (await appleSubs(env, c)).filter(s => ['active', 'trialing', 'past_due'].includes(s.status) && !s.metadata.pw_hash);
    const servers = [];
    for (const s of live)
      servers.push({ name: s.metadata.slug, url: `https://${host(s.metadata.slug)}`,
                     login_url: await entryLink(env, s.metadata.slug, s.metadata.email || '') });
    log('apple.servers', { found: servers.length, linked_by: c.email ? 'email_or_sub' : 'sub' });
    if (!servers.length)
      return json({ servers, error: 'No amux Cloud server for this Apple ID. Add your server below with its address, or sign in with Apple using the email you signed up with.' }, 404, cors(req));
    return json({ servers }, 200, cors(req));
  }

  if (p === '/api/delete' && req.method === 'POST') {
    // Same answer whether or not the address has an account. The link proves the person reads that inbox.
    const email = ((await req.json().catch(() => ({}))).email || '').trim().toLowerCase();
    const subs = email ? await accountSubs(env, email) : [];
    if (subs.length) {
      const t = await signToken(env, { email, exp: Date.now() + 3600e3 });
      const sent = await sendMail(env, email, 'Delete your amux Cloud account',
        `Someone (we hope you) asked to delete the amux Cloud account for this address.\r\n\r\n` +
        `Open this link within an hour to confirm. Nothing is deleted until you confirm on that page:\r\n\r\n` +
        `${SITE}/start/delete/?t=${encodeURIComponent(t)}${env.MODE === 'test' ? '&api=' + encodeURIComponent(url.origin) : ''}\r\n\r\n` +
        `If this was not you, ignore this email.`);
      if (!sent) return json({ error: 'email could not be sent' }, 502, cors(req));
    }
    log('delete.requested', { known: subs.length > 0 });
    return json({ ok: true }, 200, cors(req));
  }

  if (p === '/api/delete/confirm' && req.method === 'POST') {
    const b = await req.json().catch(() => ({}));
    const claims = await verifyToken(env, b.t || '');
    if (!claims) return json({ error: 'This link has expired. Ask for a new one on the billing page.' }, 400, cors(req));
    const subs = await accountSubs(env, claims.email);
    // One host per name: a canceled earlier subscription for a name bought again is still erased
    // below, but listing it twice read as two servers (codex e2e round 3, C12-3).
    const hosts = [...new Set(subs.map(s => host(s.metadata.slug)))];
    if (url.searchParams.get('peek') === '1')
      return json({ servers: hosts }, 200, cors(req));
    if (!subs.some(s => s.metadata.slug === String(b.slug || '').trim().toLowerCase()))
      return json({ error: 'Type your server name exactly as shown to confirm.' }, 400, cors(req));
    for (const s of subs) {
      if (['active', 'trialing', 'past_due', 'unpaid', 'incomplete'].includes(s.status)) {
        await stripe(env, 'POST', `/subscriptions/${s.id}`, { metadata: { erase: '1' } });
        await stripe(env, 'DELETE', `/subscriptions/${s.id}`);   // the webhook erases; doing it here too is idempotent
      }
      await erase(env, s.id, s.metadata?.slug);
    }
    await sendMail(env, claims.email, 'Your amux Cloud account is deleted',
      `Your server${hosts.length > 1 ? 's are' : ' is'} deleted, with ${hosts.length > 1 ? 'their' : 'its'} backups and snapshots, ` +
      `and billing has stopped. Thank you for trying amux Cloud. amux itself stays free: https://amux.io`);
    log('delete.confirmed', { subs: subs.length });
    return json({ ok: true, deleted: hosts }, 200, cors(req));
  }

  if (p === '/webhooks/stripe' && req.method === 'POST') {
    const body = await req.text();
    if (!(await verifyStripe(env, body, req.headers.get('stripe-signature')))) return json({ error: 'bad signature' }, 400);
    const ev = JSON.parse(body);
    const o = ev.data.object;
    if (ev.type === 'checkout.session.completed' && o.mode === 'subscription' && o.metadata?.pw_hash &&
        ['paid', 'no_payment_required'].includes(o.payment_status)) {
      const sub = typeof o.subscription === 'string' ? o.subscription : o.subscription?.id;
      await track(env, 'cloud_paid', 'paid:' + sub, o.metadata, { amount_usd: (o.amount_total || 0) / 100 });
      await provision(env, { sub, meta: o.metadata, email: (o.customer_details?.email || o.metadata?.email || '').toLowerCase() });
    } else if (ev.type === 'customer.subscription.deleted' && o.metadata?.slug) {
      await track(env, 'cloud_canceled', 'canceled:' + o.id, o.metadata,
                  { days_active: Math.round((Date.now() / 1000 - (o.created || Date.now() / 1000)) / 86400), erase: o.metadata?.erase === '1' });
      await deprovision(env, o.id, o.metadata?.erase === '1', o.metadata?.slug);
    }
    return json({ received: true });
  }
  // A person who types cloud.amux.io lands on the concierge page; API callers still get JSON.
  if ((req.method === 'GET' || req.method === 'HEAD') && !p.startsWith('/api/'))
    return Response.redirect(SITE + '/concierge', 302);
  return json({ error: 'not found' }, 404, cors(req));
}

export default {
  async fetch(req, env) {
    try { return await route(req, env); }
    catch (e) { log('error', { path: new URL(req.url).pathname, error: String(e.message || e).slice(0, 300) });
                return json({ error: 'Something went wrong on our side. Please try again.' }, 500, cors(req)); }
  },
  async scheduled(_ev, env, ctx) {
    ctx.waitUntil(tick(env).catch(e => log('cron.error', { error: String(e.message || e).slice(0, 300) })));
    const now = new Date();
    if (now.getUTCHours() === 13 && now.getUTCMinutes() === 0)
      ctx.waitUntil(dailyAudit(env).catch(e => log('ops.audit_failed', { error: String(e.message || e).slice(0, 300) })));
  },
};
