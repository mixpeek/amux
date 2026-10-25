// Offline tests for worker.mjs: the real module against in-memory Hetzner, Cloudflare, Stripe and
// Gmail fakes, with faults injected where production has failed or could. Run: node cloud/start/test-worker.mjs
// Each case prints PASS/FAIL; the exit status is the number of failures.
import fs from 'node:fs';
import crypto from 'node:crypto';

const src = fs.readFileSync(new URL('./worker.mjs', import.meta.url), 'utf8');
const { default: worker } = await import('data:text/javascript;base64,' + Buffer.from(src).toString('base64'));

function world() {
  const w = { servers: [], dns: [], subs: {}, sessions: [], mail: [], logs: [], images: [],
              faults: { dns: 0, capacity: 0, mail: 0 }, n: 0 };
  w.env = { MODE: 'test', TEST_KEY: 'k', GOLDEN_IMAGE: '1', CF_ZONE_ID: 'z', GMAIL_OAUTH: btoa(JSON.stringify({})),
            STRIPE_WEBHOOK_SECRET: crypto.randomBytes(16).toString('hex'),
            PLANS: JSON.stringify({ plans: [{ id: 'team', server_type: 'cx43', stripe_lookup_key: 'team' }] }) };
  const ok = d => Response.json(d);
  w.fetch = async (url, o = {}) => {
    const u = new URL(String(url)), m = o.method || 'GET', p = u.pathname;
    if (u.host === 'oauth2.googleapis.com') { w.tokenCalls = (w.tokenCalls || 0) + 1; return w.token ? w.token(new URLSearchParams(o.body)) : ok({ access_token: 't' }); }
    if (u.host === 'gmail.googleapis.com') {
      if (w.faults.mail) { w.faults.mail--; return new Response('{}', { status: 503 }); }
      w.mail.push(JSON.parse(o.body)); return ok({});
    }
    if (u.host === 'api.hetzner.cloud') {
      const body = o.body ? JSON.parse(o.body) : {};
      if (p === '/v1/servers' && m === 'GET') {
        const sel = (u.searchParams.get('label_selector') || '').split(',').filter(Boolean).map(x => x.split('='));
        return ok({ servers: w.servers.filter(s => sel.every(([k, v]) => s.labels[k] === v)) });
      }
      if (p === '/v1/servers' && m === 'POST') {
        if (w.faults.capacity) { w.faults.capacity--; return Response.json({ error: { code: 'resource_unavailable', message: 'error during placement' } }, { status: 412 }); }
        if (w.servers.some(s => s.name === body.name)) return Response.json({ error: { code: 'uniqueness_error', message: 'name' } }, { status: 409 });
        if (w.faults.bootImage && body.image !== 1) { w.faults.bootImage--; return Response.json({ error: { code: 'invalid_input', message: 'image disk is larger than server type disk' } }, { status: 422 }); }
        const s = { id: ++w.n, name: body.name, image: body.image, labels: body.labels, user_data: body.user_data, created: new Date().toISOString(), backup_window: null,
                    public_net: { ipv4: { ip: `192.0.2.${w.n}` } } };
        w.servers.push(s); return ok({ server: s });
      }
      let mm;
      if ((mm = p.match(/^\/v1\/servers\/(\d+)\/actions\/create_image$/))) { const img = { id: ++w.n, status: 'available', created: new Date().toISOString(), ...body }; w.images.push(img); return ok({ image: img }); }
      if ((mm = p.match(/^\/v1\/servers\/(\d+)\/actions\/enable_backup$/))) { w.servers.find(s => s.id == mm[1]).backup_window = '22-02'; return ok({}); }
      if ((mm = p.match(/^\/v1\/servers\/(\d+)$/)) && m === 'PUT') { const s = w.servers.find(s => s.id == mm[1]); s.labels = body.labels; return ok({ server: s }); }
      if ((mm = p.match(/^\/v1\/servers\/(\d+)$/)) && m === 'DELETE') { w.servers = w.servers.filter(s => s.id != mm[1]); return ok({}); }
      if (p === '/v1/images') {
        const sel = (u.searchParams.get('label_selector') || '').split(',').filter(Boolean).map(x => x.split('='));
        return ok({ images: w.images.filter(i => sel.every(([k, v]) => i.labels?.[k] === v)) });
      }
      if ((mm = p.match(/^\/v1\/images\/(\d+)$/)) && m === 'DELETE') {
        if (!w.images.some(i => i.id == mm[1])) return Response.json({ error: { code: 'not_found' } }, { status: 404 });
        w.images = w.images.filter(i => i.id != mm[1]); return ok({});
      }
      if ((mm = p.match(/^\/v1\/images\/(\d+)$/))) { const i = w.images.find(i => i.id == mm[1]); return i ? ok({ image: i }) : Response.json({ error: { code: 'not_found' } }, { status: 404 }); }
      throw new Error(`hetzner fake: ${m} ${p}`);
    }
    if (u.host === 'api.cloudflare.com') {
      if (m === 'GET') return ok({ success: true, result: w.dns.filter(r => r.name === u.searchParams.get('name')) });
      if (m === 'POST') {
        if (w.faults.dns) { w.faults.dns--; return Response.json({ success: false, errors: [{ message: 'injected' }] }, { status: 503 }); }
        const r = { id: 'r' + (++w.n), ...JSON.parse(o.body) }; w.dns.push(r); return ok({ success: true, result: r });
      }
      if (m === 'DELETE') { const id = p.split('/').pop(); w.dns = w.dns.filter(r => r.id !== id); return ok({ success: true, result: {} }); }
    }
    if (u.host === 'us.i.posthog.com') { (w.events ||= []).push(JSON.parse(o.body)); return ok({ status: 1 }); }
    if (u.host === 'appleid.apple.com') return ok({ keys: w.appleKeys || [] });
    if (u.host === 'api.stripe.com') {
      const f = new URLSearchParams(o.body || u.search.slice(1));
      if (p === '/v1/customers/search') {
        const sub = (f.get('query').match(/'([^']*)'$/) || [])[1];
        return ok({ data: Object.keys(w.appleLinks || {}).filter(c => w.appleLinks[c] === sub).map(id => ({ id })) });
      }
      let cm;
      if ((cm = p.match(/^\/v1\/customers\/([^/]+)$/)) && m === 'POST') {
        (w.appleLinks ||= {})[cm[1]] = f.get('metadata[apple_sub]'); return ok({ id: cm[1] });
      }
      if (p === '/v1/prices') return ok({ data: [{ id: 'price_team' }] });
      if (p === '/v1/checkout/sessions' && m === 'POST') {
        const meta = {}; for (const [k, v] of f) { const x = k.match(/^metadata\[(.+)\]$/); if (x) meta[x[1]] = v; }
        const s = { id: 'cs_test_' + (++w.n), url: 'https://checkout.test/' + w.n, status: 'open', created: Math.floor(Date.now() / 1000), metadata: meta,
                    params: [...f.keys()] };
        w.sessions.push(s); return ok(s);
      }
      if (p === '/v1/checkout/sessions' && m === 'GET') return ok({ data: w.sessions.filter(s => s.status === f.get('status')) });
      let mm;
      if ((mm = p.match(/^\/v1\/checkout\/sessions\/([^/]+)\/expire$/))) { w.sessions.find(s => s.id === mm[1]).status = 'expired'; return ok({}); }
      if ((mm = p.match(/^\/v1\/checkout\/sessions\/([^/]+)$/))) {
        const s = w.sessions.find(s => s.id === mm[1]);
        return s ? ok(s) : Response.json({ error: { message: `No such checkout.session: ${mm[1]}` } }, { status: 404 });
      }
      if (p === '/v1/subscriptions' && m === 'GET')
        return ok({ data: Object.values(w.subs).filter(s => (f.get('status') === 'all' || s.status === 'active') && (!f.get('customer') || s.customer_id === f.get('customer'))).map(s => ({ ...s, customer: { id: s.customer_id, email: s.email } })) });
      if ((mm = p.match(/^\/v1\/subscriptions\/([^/]+)$/)) && m === 'POST') {
        const s = w.subs[mm[1]]; for (const [k, v] of f) { const x = k.match(/^metadata\[(.+)\]$/); if (x) { if (v === '') delete s.metadata[x[1]]; else s.metadata[x[1]] = v; } }
        return ok(s);
      }
      if ((mm = p.match(/^\/v1\/subscriptions\/([^/]+)$/)) && m === 'GET') return ok({ ...w.subs[mm[1]], customer: { email: w.subs[mm[1]].email } });
      if ((mm = p.match(/^\/v1\/subscriptions\/([^/]+)$/)) && m === 'DELETE') { w.subs[mm[1]].status = 'canceled'; return ok({}); }
      if (p === '/v1/customers') return ok({ data: [...new Set(Object.values(w.subs).filter(s => s.email === f.get('email')).map(s => s.customer_id))].map(id => ({ id })) });
      if (p === '/v1/billing_portal/sessions') return ok({ url: 'https://portal.test/x' });
    }
    if (u.pathname === '/_door/report') {
      // The door's check, in node: a report-scoped token for this host under its derived key, else 404.
      w.reportCalls = (w.reportCalls || 0) + 1;
      const t = String(o.headers?.authorization || '').replace(/^Bearer /, ''), [b, mac] = t.split('.'), slug = u.host.split('.')[0];
      const key = crypto.createHmac('sha256', w.env.DOOR_ENTRY_SECRET || '').update('amux-entry|' + slug).digest('hex');
      const good = b && w.env.DOOR_ENTRY_SECRET && crypto.createHmac('sha256', key).update(b).digest('base64url') === mac;
      const c = good ? JSON.parse(Buffer.from(b, 'base64url')) : {};
      if (!good || c.scope !== 'report' || c.aud !== slug || !w.report) return new Response('', { status: 404 });
      w.lastReportToken = t;
      return Response.json(typeof w.report === 'function' ? w.report() : w.report);
    }
    if (u.pathname === '/_door/status') return w.up ? Response.json({ live_commit: 'abc', dashboard: true, verdict: 'current', checked_at: Math.floor(Date.now() / 1000), ...(typeof w.up === 'object' ? w.up : {}) }) : new Response('down', { status: 502 });
    throw new Error(`fake: unexpected ${m} ${url}`);
  };
  return w;
}

function paid(w, slug, ageS = 0) {
  const id = 'sub_' + slug;
  w.subs[id] = { id, status: 'active', created: Math.floor(Date.now() / 1000) - ageS, email: `hello+${slug}@amux.io`, customer_id: 'cus_' + slug,
                 metadata: { slug, plan: 'team', pw_hash: 'scrypt$x$y', email: `hello+${slug}@amux.io` } };
  return id;
}
async function call(w, req) {
  globalThis.fetch = w.fetch;
  const orig = console.log; console.log = s => { try { w.logs.push(JSON.parse(s)); } catch { w.logs.push(s); } };
  try { return await req(); } finally { console.log = orig; }
}
async function webhook(w, sub) {
  const s = w.subs[sub];
  const body = JSON.stringify({ type: 'checkout.session.completed', data: { object: { id: 'cs_test_x', mode: 'subscription', payment_status: 'paid',
    subscription: sub, metadata: { ...s.metadata }, customer_details: { email: s.email } } } });
  const t = Math.floor(Date.now() / 1000);
  const sig = crypto.createHmac('sha256', w.env.STRIPE_WEBHOOK_SECRET).update(`${t}.${body}`).digest('hex');
  return call(w, () => worker.fetch(new Request('https://w.test/webhooks/stripe', { method: 'POST', body, headers: { 'stripe-signature': `t=${t},v1=${sig}` } }), w.env));
}
const cron = w => call(w, () => new Promise((res, rej) => worker.scheduled({}, w.env, { waitUntil: p => p.then(res, rej) })));
const api = (w, path, body, headers = {}) => call(w, () => worker.fetch(new Request('https://w.test' + path,
  body ? { method: 'POST', body: JSON.stringify(body), headers: { 'content-type': 'application/json', ...headers } } : {}), w.env));

let failures = 0;
async function test(name, fn) {
  try { await fn(); console.log('PASS', name); }
  catch (e) { failures++; console.log('FAIL', name, '-', e.message); }
}
const eq = (a, b, what) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(`${what}: got ${JSON.stringify(a)}, want ${JSON.stringify(b)}`); };

await test('C15-2 DNS write fails, Stripe redelivers: replay finishes it (one server, one record)', async () => {
  const w = world(); const sub = paid(w, 'dnsfault'); w.faults.dns = 1;
  eq((await webhook(w, sub)).status, 500, 'first delivery');
  eq(w.subs[sub].metadata.pw_hash !== undefined, true, 'signup still marked unfinished');
  eq((await webhook(w, sub)).status, 200, 'replay');
  eq([w.servers.length, w.dns.length, w.subs[sub].metadata.pw_hash], [1, 1, undefined], 'servers, records, pw_hash');
});

await test('C15-2 DNS write fails and no redelivery: the cron finishes it', async () => {
  const w = world(); const sub = paid(w, 'dnscron', 120); w.faults.dns = 1;
  await webhook(w, sub); await cron(w);
  eq([w.servers.length, w.dns.length, w.subs[sub].metadata.pw_hash], [1, 1, undefined], 'servers, records, pw_hash');
});

await test('C3-1 no capacity anywhere: webhook fails, cron creates the server when capacity returns', async () => {
  const w = world(); const sub = paid(w, 'nocap', 120); w.faults.capacity = 3;
  eq((await webhook(w, sub)).status, 500, 'webhook with no capacity');
  eq(w.servers.length, 0, 'no server yet');
  await cron(w);
  eq([w.servers.length, w.dns.length], [1, 1], 'after cron');
  eq(w.logs.some(l => l.event === 'provision.no_capacity'), true, 'no_capacity logged');
});

await test('C3-1 still no server after 10 minutes: operator emailed once', async () => {
  const w = world(); const sub = paid(w, 'stuck', 700); w.faults.capacity = 9;
  await cron(w); await cron(w); await cron(w);
  const alerts = w.mail.filter(x => /paid order stuck has no server/.test(Buffer.from(x.raw, 'base64').toString()));
  eq(alerts.length, 1, 'operator alerts');
  eq(Boolean(w.subs[sub].metadata.alerted), true, 'alert recorded on the subscription');
});

await test('C3-1 the webhook gets 90 seconds before the cron touches a new order', async () => {
  const w = world(); paid(w, 'fresh', 10); await cron(w);
  eq(w.servers.length, 0, 'servers');
});

await test('replayed webhook after success: no second server', async () => {
  const w = world(); const sub = paid(w, 'replay');
  await webhook(w, sub); w.subs[sub].metadata.pw_hash = 'scrypt$x$y'; await webhook(w, sub);
  eq([w.servers.length, w.dns.length], [1, 1], 'servers, records');
});

await test('a checkout event redelivered after the account was deleted builds nothing (fin-fbb1)', async () => {
  const w = world(); const sub = paid(w, 'erased');
  await webhook(w, sub);
  w.subs[sub].metadata.erase = '1'; w.subs[sub].status = 'canceled';
  w.subs[sub].metadata.pw_hash = 'scrypt$x$y';   // the checkout event keeps the signup data
  w.servers.length = 0; w.dns.length = 0;   // what the deletion left
  eq((await webhook(w, sub)).status, 200, 'redelivery is acknowledged');
  eq([w.servers.length, w.dns.length], [0, 0], 'servers, records');
  eq(w.logs.some(l => l.event === 'provision.skipped_ended' && l.erase === true), true, 'skip logged');
  const ended = paid(w, 'ended'); w.subs[ended].status = 'canceled';
  await webhook(w, ended);
  eq(w.servers.length, 0, 'a canceled subscription gets no server either');
});

await test('C12-6 a redelivery while a deleted account\'s server is still going does not republish its address', async () => {
  const w = world(); const sub = paid(w, 'midway');
  await webhook(w, sub);
  w.subs[sub].status = 'canceled'; w.subs[sub].metadata.erase = '1'; w.subs[sub].metadata.pw_hash = 'scrypt$x$y';
  w.dns.length = 0;   // the address is gone, the server not yet
  await webhook(w, sub);
  eq(w.dns.length, 0, 'records');
  eq(w.logs.some(l => l.event === 'provision.resume' && l.sub === sub), false, 'no resume for an erased account');
});

await test('ops audit: a server left behind by a deleted account is erased with no snapshot', async () => {
  const w = world(); w.env.OPS_KEY = 'ops';
  const sub = paid(w, 'leftover'); await webhook(w, sub);
  w.subs[sub].status = 'canceled'; w.subs[sub].metadata.erase = '1';
  const rep = await (await call(w, () => worker.fetch(new Request('https://w.test/api/ops', { headers: { 'x-ops-key': 'ops' } }), w.env))).json();
  eq(rep.fixed, [`leftover: account was deleted, server ${w.logs.find(l => l.event === 'provision.created').server} erased`], 'fixed');
  eq([w.servers.length, w.dns.length, w.images.length], [0, 0, 0], 'servers, records, snapshots');
});

await test('forged webhook refused', async () => {
  const w = world();
  const r = await call(w, () => worker.fetch(new Request('https://w.test/webhooks/stripe', { method: 'POST', body: '{}',
    headers: { 'stripe-signature': `t=${Math.floor(Date.now() / 1000)},v1=${'0'.repeat(64)}` } }), w.env));
  eq(r.status, 400, 'status');
});

await test('C15-1 the same signup submitted twice leaves one payable checkout', async () => {
  const w = world(); const b = { plan: 'team', slug: 'twice', email: 'hello+twice@amux.io', pw_hash: 'scrypt$' + 'A'.repeat(24) + '$' + 'B'.repeat(44) };
  const r1 = await (await api(w, '/api/checkout', b, { 'x-test-key': 'k' })).json();
  const r2 = await (await api(w, '/api/checkout', b, { 'x-test-key': 'k' })).json();
  eq(w.sessions.filter(s => s.status === 'open').length, 1, 'open sessions');
  eq(r2.url, w.sessions.find(s => s.status === 'open').url, 'second answer is the open one');
  eq(r1.url !== r2.url, true, 'first was replaced');
});

await test('C15-3 a paid name gets no second checkout before its server exists', async () => {
  const w = world(); paid(w, 'paidname');
  const b = { plan: 'team', slug: 'paidname', email: 'hello+paidname@amux.io', pw_hash: 'scrypt$' + 'A'.repeat(24) + '$' + 'B'.repeat(44) };
  const r = await api(w, '/api/checkout', b, { 'x-test-key': 'k' });
  eq(r.status, 409, 'status');
  eq((await r.json()).error.includes('already paid'), true, 'owner is told it is being set up');
  const r2 = await api(w, '/api/checkout', { ...b, email: 'someone@else.io' }, { 'x-test-key': 'k' });
  eq((await r2.json()).error, 'That name is taken.', 'stranger');
  eq(w.sessions.length, 0, 'no checkout opened');
  w.subs.sub_paidname.status = 'canceled';   // deleted account: the name is free again
  eq((await api(w, '/api/checkout', b, { 'x-test-key': 'k' })).status, 200, 'resubscribe');
});

await test('checkout collects no tax (no EU VAT)', async () => {
  const w = world();
  await api(w, '/api/checkout', { plan: 'team', slug: 'notax', email: 'hello+notax@amux.io', pw_hash: 'scrypt$' + 'A'.repeat(24) + '$' + 'B'.repeat(44) }, { 'x-test-key': 'k' });
  eq(w.sessions.length, 1, 'sessions');
  eq(w.sessions[0].params.filter(k => k.startsWith('automatic_tax')), [], 'tax params');
});

await test('C14-2 unknown order answers 404, not 500', async () => {
  const w = world(); eq((await api(w, '/api/status?session_id=cs_test_nope')).status, 404, 'status');
});

await test('C14-1 billing email that cannot be sent is reported (502)', async () => {
  const w = world(); paid(w, 'mailfail'); w.faults.mail = 1;
  eq((await api(w, '/api/billing', { email: 'hello+mailfail@amux.io' })).status, 502, 'status');
});

await test('C14-1 billing for an unknown address still answers ok (reveals nothing)', async () => {
  const w = world(); eq((await api(w, '/api/billing', { email: 'nobody@example.com' })).status, 200, 'status');
});

await test('C12 delete: link, confirm with the wrong name refused, right name erases server, DNS, subscription', async () => {
  const w = world(); const sub = paid(w, 'gone'); await webhook(w, sub);
  eq((await api(w, '/api/delete', { email: 'hello+gone@amux.io' })).status, 200, 'request');
  const text = Buffer.from(w.mail.at(-1).raw.replace(/-/g, '+').replace(/_/g, '/'), 'base64').toString();
  const t = decodeURIComponent(text.match(/\?t=([^&\s]+)/)[1]);
  eq((await api(w, '/api/delete/confirm', { t, slug: 'other' })).status, 400, 'wrong name');
  eq(w.servers.length, 1, 'still there');
  const r = await api(w, '/api/delete/confirm', { t, slug: 'gone' });
  eq([r.status, w.servers.length, w.dns.length, w.subs[sub].status], [200, 0, 0, 'canceled'], 'after confirm');
});

await test('C12-4 delete after the webhook already removed the server still removes the address', async () => {
  // e2efull-ba83, 2026-10-18: the webhook and the delete page raced, the page found no server,
  // and the DNS record was left pointing at a deleted IP.
  const w = world(); const sub = paid(w, 'raced'); await webhook(w, sub);
  w.servers = [];                                         // the webhook's erase got there first
  await api(w, '/api/delete', { email: 'hello+raced@amux.io' });
  const text = Buffer.from(w.mail.at(-1).raw.replace(/-/g, '+').replace(/_/g, '/'), 'base64').toString();
  const t = decodeURIComponent(text.match(/\?t=([^&\s]+)/)[1]);
  const r = await api(w, '/api/delete/confirm', { t, slug: 'raced' });
  eq([r.status, w.dns.length], [200, 0], 'address removed, confirm answers');
});

await test('C12-4 an address now owned by another subscription is kept', async () => {
  // The first owner deleted long ago (server gone, subscription canceled); someone else bought the name.
  const w = world();
  w.subs.sub_first = { id: 'sub_first', status: 'canceled', created: 0, email: 'hello+first@amux.io', customer_id: 'cus_first',
                       metadata: { slug: 'reused', plan: 'team', pw_hash: 'scrypt$x$y', email: 'hello+first@amux.io' } };
  const now = paid(w, 'reused'); await webhook(w, now);
  eq(w.dns.length, 1, 'new owner has an address');
  await api(w, '/api/delete', { email: 'hello+first@amux.io' });
  const text = Buffer.from(w.mail.at(-1).raw.replace(/-/g, '+').replace(/_/g, '/'), 'base64').toString();
  const t = decodeURIComponent(text.match(/\?t=([^&\s]+)/)[1]);
  await api(w, '/api/delete/confirm', { t, slug: 'reused' });
  eq([w.dns.length, w.servers.length], [1, 1], 'new owner keeps address and server');
});

await test('C12-3 a name bought again is listed once for deletion', async () => {
  const w = world(); const sub = paid(w, 'again'); await webhook(w, sub);
  w.subs.sub_again_old = { ...w.subs[sub], id: 'sub_again_old', status: 'canceled' };   // the first purchase, deleted earlier
  await api(w, '/api/delete', { email: 'hello+again@amux.io' });
  const text = Buffer.from(w.mail.at(-1).raw.replace(/-/g, '+').replace(/_/g, '/'), 'base64').toString();
  const t = decodeURIComponent(text.match(/\?t=([^&\s]+)/)[1]);
  const peek = await (await api(w, '/api/delete/confirm?peek=1', { t })).json();
  eq(peek.servers.length, 1, 'one host');
  const r = await (await api(w, '/api/delete/confirm', { t, slug: 'again' })).json();
  eq(r.deleted.length, 1, 'deleted once');
  const done = Buffer.from(w.mail.at(-1).raw.replace(/-/g, '+').replace(/_/g, '/'), 'base64').toString();
  eq(done.includes('Your server is deleted'), true, 'singular');
});

await test('C12 delete link with a forged token refused', async () => {
  const w = world(); eq((await api(w, '/api/delete/confirm', { t: 'e30.AAAA', slug: 'x' })).status, 400, 'status');
});

await test('mail: a revoked user grant is logged as email.token_failed and reported (502)', async () => {
  const w = world(); paid(w, 'revoked');
  w.token = () => Response.json({ error: 'invalid_grant' }, { status: 400 });
  eq((await api(w, '/api/billing', { email: 'hello+revoked@amux.io' })).status, 502, 'status');
  eq(w.logs.find(l => l.event === 'email.token_failed')?.error, 'invalid_grant', 'logged reason');
});

await test('mail: the service account signs its own hello@amux.io token (no user grant to expire)', async () => {
  const w = world(); paid(w, 'sa1'); paid(w, 'sa2'); delete w.env.GMAIL_OAUTH;
  const { publicKey, privateKey } = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
  w.env.GMAIL_SA = btoa(JSON.stringify({ client_email: 'mailer@p.iam.gserviceaccount.com',
    private_key: privateKey.export({ type: 'pkcs8', format: 'pem' }) }));
  let claims;
  w.token = f => {
    const [h, c, sig] = f.get('assertion').split('.');
    const valid = crypto.verify('sha256', Buffer.from(h + '.' + c), publicKey, Buffer.from(sig, 'base64url'));
    claims = { grant: f.get('grant_type'), valid, ...JSON.parse(Buffer.from(c, 'base64url')) };
    return Response.json({ access_token: 'sa-token', expires_in: 3600 });
  };
  eq((await api(w, '/api/billing', { email: 'hello+sa1@amux.io' })).status, 200, 'first send');
  eq((await api(w, '/api/billing', { email: 'hello+sa2@amux.io' })).status, 200, 'second send');
  eq([claims.grant, claims.valid, claims.sub, claims.scope, claims.iss],
     ['urn:ietf:params:oauth:grant-type:jwt-bearer', true, 'hello@amux.io', 'https://www.googleapis.com/auth/gmail.send', 'mailer@p.iam.gserviceaccount.com'], 'assertion');
  eq([w.mail.length, w.tokenCalls], [2, 1], 'two emails, one token');
});

await test('mail: delegation not yet approved, the user grant still sends', async () => {
  const w = world(); paid(w, 'fallback');
  const { privateKey } = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
  w.env.GMAIL_SA = btoa(JSON.stringify({ client_email: 'm@p', private_key: privateKey.export({ type: 'pkcs8', format: 'pem' }) }));
  w.token = f => f.get('assertion') ? Response.json({ error: 'unauthorized_client' }, { status: 401 }) : Response.json({ access_token: 'u' });
  eq((await api(w, '/api/billing', { email: 'hello+fallback@amux.io' })).status, 200, 'status');
  eq(w.logs.find(l => l.event === 'email.token_failed')?.error, 'unauthorized_client', 'service account failure logged');
});

await test('C8 the paying customer is the server owner (no member token)', async () => {
  const w = world(); const sub = paid(w, 'owner'); await webhook(w, sub);
  eq(/AMUX_WEB_USERS='hello\+owner@amux\.io:scrypt\$x\$y:hello\+owner@amux\.io:owner'/.test(w.servers[0].user_data), true, 'user_data owner grant');
});

await test('the server\'s Settings links its owner to billing and deletion (test mode keeps the test function)', async () => {
  const w = world(); w.env.SELF_URL = 'https://amux-cloud-test.example.workers.dev';
  const sub = paid(w, 'acct'); await webhook(w, sub);
  const m = w.servers[0].user_data.match(/AMUX_ACCOUNT_URL='([^']+)'/);
  eq(!!m, true, 'user_data carries the account link');
  const u = new URL(m[1]);
  eq(u.origin + u.pathname, 'https://amux.io/start/billing/', 'billing page');
  eq(u.searchParams.get('email'), 'hello+acct@amux.io', 'owner email prefilled');
  eq(u.searchParams.get('api'), w.env.SELF_URL, 'test function');
  const w2 = world(); w2.env.MODE = 'live'; w2.env.SELF_URL = 'https://cloud.amux.io'; const sub2 = paid(w2, 'live1'); await webhook(w2, sub2);
  eq(w2.servers.length, 1, 'live server created');
  eq(/AMUX_ACCOUNT_URL='[^']*api=/.test(w2.servers[0].user_data), false, 'live link has no api override');
});

// Sign in with Apple: a fake Apple signing key, and the real door.py checking the Worker's link.
function apple(w, claims, { key, kid = 'k' + Math.random() } = {}) {
  const pair = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
  w.appleKeys = [{ ...pair.publicKey.export({ format: 'jwk' }), kid, alg: 'RS256', use: 'sig' }];
  const enc = o => Buffer.from(JSON.stringify(o)).toString('base64url');
  const body = enc({ alg: 'RS256', kid }) + '.' + enc({ iss: 'https://appleid.apple.com', aud: 'com.example.app',
    exp: Math.floor(Date.now() / 1000) + 600, ...claims });
  return body + '.' + crypto.sign('sha256', Buffer.from(body), key || pair.privateKey).toString('base64url');
}
function appleWorld(slug) {
  const w = world(); w.env.APPLE_BUNDLE_ID = 'com.example.app'; w.env.DOOR_ENTRY_SECRET = 'entry-root';
  const sub = paid(w, slug); return { w, sub };
}
async function doorAccepts(w, link) {
  const { execFileSync } = await import('node:child_process');
  const os = await import('node:os'), path = await import('node:path');
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'door-entry-'));
  const secret = w.servers[0].user_data.match(/AMUX_ENTRY_SECRET='([0-9a-f]+)'/)[1];
  fs.writeFileSync(path.join(dir, 'entry'), `AMUX_SLUG=${w.servers[0].name.replace(/^amux-/, "")}\nAMUX_FLEET_SECRET=${secret}\n`);
  const t = new URL(link).searchParams.get('t');
  const door = new URL('../desktop/', import.meta.url).pathname;
  try {
    return JSON.parse(execFileSync('python3', ['-I', '-c', 'import sys,json; sys.path.insert(0, sys.argv[1]); import door; print(json.dumps(door.verify_entry_token(sys.argv[2])))', door, t],
      { env: { PATH: process.env.PATH, DOOR_ENTRY: path.join(dir, 'entry'), DOOR_FLEET_ENV: path.join(dir, 'none') } }).toString());
  } finally { fs.rmSync(dir, { recursive: true }); }
}

await test('C7 Sign in with Apple: the signup email finds the server and the door accepts its link', async () => {
  const { w, sub } = appleWorld('ios1'); await webhook(w, sub);
  const r = await api(w, '/api/apple/servers', { identity_token: apple(w, { sub: 'apple-1', email: 'hello+ios1@amux.io', email_verified: 'true' }) });
  const d = await r.json();
  eq([r.status, d.servers.map(s => s.name)], [200, ['ios1']], 'servers');
  const c = await doorAccepts(w, d.servers[0].login_url);
  eq([c.sub, c.aud, c.via], ['hello+ios1@amux.io', 'ios1', 'apple'], 'door claims');
  eq(w.appleLinks?.cus_ios1, 'apple-1', 'Apple ID linked to the customer');
  // A later sign-in with a private relay address still finds it through the link.
  const r2 = await api(w, '/api/apple/servers', { identity_token: apple(w, { sub: 'apple-1', email: 'x@privaterelay.appleid.com', email_verified: 'true' }) });
  eq(r2.status, 200, 'relay address sign-in');
});

await test('C7 Sign in with Apple: a link for one server is refused by another', async () => {
  const { w, sub } = appleWorld('ios2'); await webhook(w, sub);
  const d = await (await api(w, '/api/apple/servers', { identity_token: apple(w, { sub: 'a2', email: 'hello+ios2@amux.io', email_verified: 'true' }) })).json();
  w.servers[0].name = "amux-other";
  let err = ''; try { await doorAccepts(w, d.servers[0].login_url); } catch (e) { err = String(e.stderr || e.message); }
  eq(/bad signature|wrong server/.test(err), true, 'door refusal');
});

await test('C4 Sign in with Apple: forged, wrong-app and unverified tokens are refused', async () => {
  const { w, sub } = appleWorld('ios3'); await webhook(w, sub);
  const other = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 }).privateKey;
  const claims = { sub: 'a3', email: 'hello+ios3@amux.io', email_verified: 'true' };
  eq((await api(w, '/api/apple/servers', { identity_token: apple(w, claims, { key: other }) })).status, 401, 'forged signature');
  eq((await api(w, '/api/apple/servers', { identity_token: apple(w, { ...claims, aud: 'com.evil.app' }) })).status, 401, 'other app');
  eq((await api(w, '/api/apple/servers', { identity_token: 'x.y.z' })).status, 401, 'garbage');
  eq(w.logs.filter(l => l.event === 'apple.refused').length, 3, 'refusals logged');
  eq((await api(w, '/api/apple/servers', { identity_token: apple(w, { ...claims, email_verified: 'false' }) })).status, 404, 'unverified email finds nothing');
  eq((await api(w, '/api/apple/servers', { identity_token: apple(w, { sub: 'a4', email: 'stranger@example.com', email_verified: 'true' }) })).status, 404, 'no account');
});

await test('ops audit: books against servers, with the one safe fix made and the rest reported', async () => {
  const w = world(); w.env.OPS_KEY = 'ops';
  const price = (cents) => ({ data: [{ price: { unit_amount: cents }, quantity: 1 }] });
  const ok = paid(w, 'healthy'); await webhook(w, ok); w.subs[ok].items = price(4900);
  const thin = paid(w, 'thin'); await webhook(w, thin); w.subs[thin].items = price(2000);
  const gone = paid(w, 'gone'); await webhook(w, gone); w.subs[gone].status = 'canceled';
  const lost = paid(w, 'lost', 3 * 86400); delete w.subs[lost].metadata.pw_hash;
  for (const s of w.servers) s.server_type = { prices: [{ location: 'fsn1', price_monthly: { gross: '18.00' } }] };
  w.servers.push({ id: 99, name: 'amux-stray', labels: { 'amux-standalone': '1', 'amux-mode': 'test', 'amux-slug': 'stray', 'amux-sub': 'sub_nobody' } });
  eq((await api(w, '/api/ops')).status, 404, 'no key, no report');
  const rep = await (await call(w, () => worker.fetch(new Request('https://w.test/api/ops', { headers: { 'x-ops-key': 'ops' } }), w.env))).json();
  eq(rep.fixed, ['gone: subscription canceled, server snapshotted for deletion'], 'fixed');
  eq(rep.problems.sort(), ['lost: active subscription sub_lost has no server',
    'stray: server 99 has no subscription Stripe knows (sub_nobody)',
    'thin: plan team pays $20.00/mo for a server costing about 18.00/mo'], 'problems');
  eq([rep.mrr_usd, rep.funnel.measured], [69, false], 'mrr and unmeasured funnel');
  eq(w.images.some(i => i.labels?.['amux-sub'] === gone), true, 'final snapshot taken');
  eq(w.logs.some(l => l.event === 'ops.audit'), true, 'audit logged');
});

await test('a browser visit to cloud.amux.io goes to the concierge page; unknown API paths stay JSON 404', async () => {
  const w = world();
  for (const path of ['/', '/pricing']) {
    const r = await api(w, path);
    eq([r.status, r.headers.get('location')], [302, 'https://amux.io/concierge'], path);
  }
  eq((await api(w, '/api/nope')).status, 404, 'unknown api path');
  eq((await api(w, '/health')).status, 200, 'health');
});

async function ended(w, sub) {
  const body = JSON.stringify({ type: 'customer.subscription.deleted', data: { object: { id: sub, metadata: { ...w.subs[sub].metadata } } } });
  const t = Math.floor(Date.now() / 1000);
  const sig = crypto.createHmac('sha256', w.env.STRIPE_WEBHOOK_SECRET).update(`${t}.${body}`).digest('hex');
  w.subs[sub].status = 'canceled';
  return call(w, () => worker.fetch(new Request('https://w.test/webhooks/stripe', { method: 'POST', body, headers: { 'stripe-signature': `t=${t},v1=${sig}` } }), w.env));
}
const sameOwner = (w, from, to) => { w.subs[to].email = w.subs[from].email; w.subs[to].metadata.email = w.subs[from].email; };

await test('C13 canceled: snapshot kept 30 days, server and address go, then the snapshot expires', async () => {
  const w = world(); const sub = paid(w, 'gone30');
  await webhook(w, sub); await ended(w, sub);
  const img = w.images.find(i => i.labels['amux-sub'] === sub);
  const days = (Number(img.labels['amux-expires']) * 1000 - Date.now()) / 86400e3;
  eq([Math.round(days), img.labels['amux-cloud-final']], [30, '1'], 'final snapshot kept 30 days');
  await cron(w);
  eq([w.servers.length, w.dns.length, w.images.length], [0, 0, 1], 'server and address gone, snapshot kept');
  img.labels['amux-expires'] = String(Math.floor(Date.now() / 1000) - 1);    // 30 days later
  await cron(w);
  eq(w.images.length, 0, 'snapshot deleted at expiry');
});

await test('C10/C13 resubscribing within 30 days restores the last server, under a new name too', async () => {
  const w = world(); const old = paid(w, 'comeback');
  await webhook(w, old); await ended(w, old); await cron(w);
  const snap = w.images[0].id;
  const again = paid(w, 'comeback2'); sameOwner(w, old, again);
  await webhook(w, again);
  eq([w.servers.length, w.servers[0].image, w.servers[0].labels['amux-restored-from']], [1, snap, String(snap)], 'booted from the snapshot');
  eq(/AMUX_WEB_USERS='hello\+comeback@amux\.io:scrypt/.test(w.servers[0].user_data), true, 'the new signup password applies');
  w.up = true; await cron(w);
  eq(w.images.length, 0, 'snapshot deleted once the restored server is ready');
  const mail = w.mail.map(x => Buffer.from(x.raw, 'base64').toString()).find(t => /server is ready/.test(t)) || '';
  eq(/restored from your previous server/.test(mail), true, 'ready email says it was restored');
});

await test('C10 another email never gets someone else\'s snapshot', async () => {
  const w = world(); const old = paid(w, 'private1');
  await webhook(w, old); await ended(w, old); await cron(w);
  const other = paid(w, 'stranger1'); w.subs[other].email = 'stranger@example.com'; w.subs[other].metadata.email = 'stranger@example.com';
  await webhook(w, other);
  eq([w.servers.length, w.servers[0].image, w.images.length], [1, 1, 1], 'golden image, snapshot untouched');
});

await test('C10 a snapshot that cannot boot on the new plan falls back to a fresh server', async () => {
  const w = world(); const old = paid(w, 'shrink');
  await webhook(w, old); await ended(w, old); await cron(w);
  const again = paid(w, 'shrink2'); sameOwner(w, old, again);
  w.faults.bootImage = 1;
  await webhook(w, again);
  eq([w.servers.length, w.servers[0].image, w.servers[0].labels['amux-restored-from']], [1, 1, undefined], 'golden image');
  eq(w.logs.some(l => JSON.stringify(l).includes('provision.restore_failed')), true, 'logged');
});

await test('C9 a certificate that is not renewing reaches the operator before it expires', async () => {
  const w = world(); const sub = paid(w, 'tlsold');
  await webhook(w, sub);
  const s = w.servers[0]; s.labels['amux-ready'] = '1'; s.backup_window = '22-02';
  s.id = 10 - (new Date().getUTCMinutes() % 10);           // this minute is its monitor slot
  w.up = { tls_days_left: 5 };
  await cron(w); await cron(w);
  const subj = w.mail.map(x => Buffer.from(x.raw, 'base64').toString()).filter(t => /TLS certificate expires in 5 days/.test(t));
  eq(subj.length, 1, 'one operator email after two failed checks');
  w.up = { tls_days_left: 60 }; w.mail = [];
  await cron(w);
  eq(w.mail.map(x => Buffer.from(x.raw, 'base64').toString()).some(t => /recovered/.test(t)), true, 'recovery email');
});

await test('funnel: paid, ready and canceled reach PostHog under the visitor id, once per order', async () => {
  const w = world();
  const r = await api(w, '/api/checkout', { plan: 'team', slug: 'phtrack', email: 'a@example.com', pw_hash: 'scrypt$' + 'A'.repeat(24) + '$' + 'B'.repeat(44),
                                           ph: '0192-visitor', ab: 'exp146=b', source: 'ai:/guides/x/' }, { 'x-test-key': 'k' });
  eq(r.status, 200, 'checkout');
  const meta = w.sessions.at(-1).metadata;
  eq([meta.ph, meta.ab, meta.source], ['0192-visitor', 'exp146=b', 'ai:/guides/x/'], 'visitor id, variants and source kept on the order');
  const bad = await api(w, '/api/checkout', { plan: 'team', slug: 'phtrack2', email: 'a@example.com', pw_hash: 'scrypt$' + 'A'.repeat(24) + '$' + 'B'.repeat(44),
                                             ph: 'x y<script>', ab: 'a=b;c' }, { 'x-test-key': 'k' });
  eq([bad.status, 'ph' in w.sessions.at(-1).metadata, 'ab' in w.sessions.at(-1).metadata], [200, false, false], 'malformed ph/ab dropped');
  const sub = paid(w, 'phtrack'); Object.assign(w.subs[sub].metadata, { ph: '0192-visitor', ab: 'exp146=b' });
  const sent = { ...w.subs[sub].metadata };
  await webhook(w, sub); w.subs[sub].metadata = { ...sent }; await webhook(w, sub);   // Stripe redelivers the same event
  const paidEv = w.events.filter(e => e.event === 'cloud_paid');
  eq([paidEv.length, paidEv[0].distinct_id, paidEv[0].properties.ab_exp146, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(paidEv[0].uuid) && paidEv[0].uuid === paidEv[1].uuid], [2, '0192-visitor', 'b', true],
     'cloud_paid under the visitor, a redelivery reuses the event id');
  w.up = true; await cron(w); await cron(w);
  eq(w.events.filter(e => e.event === 'cloud_ready').length, 1, 'cloud_ready once');
  await ended(w, sub);
  eq(w.events.filter(e => e.event === 'cloud_canceled').map(e => e.distinct_id), ['0192-visitor'], 'cloud_canceled');
  const w2 = world(); w2.fetch = (orig => async (url, o) => new URL(String(url)).host === 'us.i.posthog.com' ? Promise.reject(new Error('blocked')) : orig(url, o))(w2.fetch);
  const sub2 = paid(w2, 'phdown'); await webhook(w2, sub2);
  eq([w2.servers.length, w2.logs.some(l => l.event === 'track.failed')], [1, true], 'PostHog down: the server is still created, the failure is logged');
});

// ------------------------------------------------------------------ activation report, friction, nudges
// A report the way door.py's activation_report() shapes it (counts only).
function report({ signed = true, model = false, ran = 0, active = 0, groups = 0, failed = 0, disk = 40, trackedH = 1000 } = {}) {
  const now = Math.floor(Date.now() / 1000), m = { measured: true };
  return { v: 1, measured: true, n_considered: 9, unmeasured: [],
    signin: { ...m, tracking_since: now - trackedH * 3600, ever: signed, count: signed ? 1 : 0, failed_24h: failed, last_seen_at: signed ? now : null },
    model: { ...m, any: model, providers: { claude: model, openai: false } },
    workers: { ...m, n_considered: ran, total: ran, ever_ran: ran, active_24h: active, running: active },
    tokens: { ...m, today: 0 }, board: { ...m, cards: 0 }, schedules: { ...m, total: 0, enabled: 0 },
    errors: { ...m, groups_24h: groups, total_24h: groups, n_considered: groups }, version: { ...m, commit: 'abc' }, disk: { ...m, used_pct: disk } };
}
// A paid, ready server of the given age, in this minute's monitor slot.
async function readyServer(w, slug, ageH) {
  w.env.DOOR_ENTRY_SECRET = 'entry-root';
  const sub = paid(w, slug); await webhook(w, sub);
  const s = w.servers[0];
  Object.assign(s.labels, { 'amux-ready': '1' }); s.backup_window = '22-02';
  s.created = new Date(Date.now() - ageH * 3600e3).toISOString();
  s.id = 10 - (new Date().getUTCMinutes() % 10);
  w.up = true;
  return { s, sub };
}
const customerMail = w => w.mail.map(x => Buffer.from(x.raw, 'base64').toString()).filter(t => !/^[\s\S]*To: hello@amux\.io/.test(t));

await test('activation: the monitor reads the report once an hour, labels the stage, and door.py takes the Worker\'s token', async () => {
  const w = world(); const { s } = await readyServer(w, 'act1', 30);
  w.report = report({ model: false });
  await cron(w); await cron(w);
  eq(w.reportCalls, 1, 'one report fetch in the hour');
  eq([s.labels['amux-act'], s.labels['amux-rep'] === String(Math.floor(Date.now() / 3600e3))], ['no_model', true], 'stage and hour labels');
  eq(Object.values(s.labels).every(v => /^[a-z0-9]([a-z0-9._-]{0,61}[a-z0-9])?$/.test(v)), true, 'every label value is a valid Hetzner value');
  // The real door accepts the token for its own server and refuses it as a sign-in link.
  const { execFileSync } = await import('node:child_process');
  const os = await import('node:os'), path = await import('node:path');
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'door-report-'));
  const secret = s.user_data.match(/AMUX_ENTRY_SECRET='([0-9a-f]+)'/)[1];
  fs.writeFileSync(path.join(dir, 'entry'), `AMUX_SLUG=act1\nAMUX_FLEET_SECRET=${secret}\n`);
  const py = 'import sys,json; sys.path.insert(0, sys.argv[1]); import door\nc=door.verify_report_token(sys.argv[2])\n' +
             'try:\n  door.verify_entry_token(sys.argv[2]); print("SIGNIN_ACCEPTED")\nexcept ValueError as e:\n  print(c["scope"], e)';
  try {
    const out = execFileSync('python3', ['-I', '-c', py, new URL('../desktop/', import.meta.url).pathname, w.lastReportToken],
      { env: { PATH: process.env.PATH, DOOR_ENTRY: path.join(dir, 'entry'), DOOR_FLEET_ENV: path.join(dir, 'none'), DOOR_ACTIVITY: path.join(dir, 'a.json') } }).toString().trim();
    eq(out, 'report not a sign-in token', 'door verdict');
  } finally { fs.rmSync(dir, { recursive: true }); }
});

await test('activation: an older door without the report is logged as unmeasured, not as a stuck customer', async () => {
  const w = world(); const { s } = await readyServer(w, 'olddoor', 100); w.env.OPS_KEY = 'ops';
  await cron(w);
  eq([w.reportCalls, s.labels['amux-act']], [1, undefined], 'fetched, no stage');
  eq(w.logs.some(l => l.event === 'activation.report' && l.fetched === false && /not updated/.test(l.why)), true, 'logged');
  const rep = await (await call(w, () => worker.fetch(new Request('https://w.test/api/ops', { headers: { 'x-ops-key': 'ops' } }), w.env))).json();
  eq([rep.activation.length, rep.activation[0].measured, rep.friction], [1, false, []], 'unmeasured row, no friction');
});

await test('nudges: held while NUDGES is off (nothing reaches the customer)', async () => {
  const w = world(); const { s } = await readyServer(w, 'quiet', 80);
  w.report = report({ model: false, ran: 0 });
  await cron(w);
  eq(customerMail(w).length, 0, 'customer emails');
  eq(s.labels['amux-nudged'], undefined, 'nothing recorded as sent');
  eq(w.logs.some(l => l.event === 'nudge.held' && l.why === 'NUDGES is not on' && l.kind === 'm'), true, 'held and logged');
});

await test('nudges: with NUDGES=on, day 1 no model sends the model email once; day 3 no worker sends the worker email once', async () => {
  const w = world(); const { s } = await readyServer(w, 'nudgy', 30); w.env.NUDGES = 'on';
  w.report = report({ model: false });
  await cron(w);
  let mail = customerMail(w);
  eq([mail.length, /Subject: Connect a model to your amux server/.test(mail[0]), /https:\/\/nudgy\.amux\.io/.test(mail[0]), /reply "stop"/.test(mail[0])],
     [1, true, true, true], 'model email');
  eq(s.labels['amux-nudged'], 'm', 'recorded');
  delete s.labels['amux-rep']; await cron(w);
  eq(customerMail(w).length, 1, 'not sent twice');
  // Day 3: model connected, still no worker. A day has passed since the last nudge.
  s.created = new Date(Date.now() - 80 * 3600e3).toISOString(); s.labels['amux-nudge-at'] = String(Math.floor(Date.now() / 1000) - 90000);
  w.report = report({ model: true, ran: 0 }); delete s.labels['amux-rep'];
  await cron(w);
  mail = customerMail(w);
  eq([mail.length, /Subject: Start your first worker on amux/.test(mail[1]), s.labels['amux-nudged']], [2, true, 'm-w'], 'worker email');
  delete s.labels['amux-rep']; await cron(w);
  eq(customerMail(w).length, 2, 'worker email not sent twice');
  for (const t of mail) eq([t.includes('\u2014'), /\bnot\b[^.]*,\s*it'?s\b/i.test(t)], [false, false], 'no em dash, no "not X, it is Y"');
});

await test('nudges: none when the condition does not hold, too early, or the customer opted out', async () => {
  const w = world(); const { s } = await readyServer(w, 'busy', 30); w.env.NUDGES = 'on';
  w.report = report({ model: true, ran: 1, active: 1 });
  await cron(w);
  eq([customerMail(w).length, s.labels['amux-act']], [0, 'working'], 'a working customer gets nothing');
  const w2 = world(); await readyServer(w2, 'early', 10); w2.env.NUDGES = 'on'; w2.report = report({ model: false });
  await cron(w2);
  eq(customerMail(w2).length, 0, 'under 24h: nothing yet');
  const w3 = world(); const r3 = await readyServer(w3, 'optout', 30); w3.env.NUDGES = 'on'; w3.report = report({ model: false });
  r3.s.labels['amux-nudge-off'] = '1';
  await cron(w3);
  eq([customerMail(w3).length, w3.logs.some(l => l.event === 'nudge.held' && l.why === 'customer opted out')], [0, true], 'opt-out');
});

await test('audit: activation table and friction for a stuck customer; the daily email goes out for it', async () => {
  const w = world(); const { s } = await readyServer(w, 'stuckco', 72); w.env.OPS_KEY = 'ops';
  w.report = report({ signed: false, model: false, ran: 0, groups: 12, failed: 11, disk: 91, trackedH: 72 });
  s.labels['amux-errg'] = '3';
  const rep = await (await call(w, () => worker.fetch(new Request('https://w.test/api/ops', { headers: { 'x-ops-key': 'ops' } }), w.env))).json();
  eq(rep.activation.map(a => [a.slug, a.stage, a.signed_in, a.model, a.workers_ever_ran, a.disk_used_pct]),
     [['stuckco', 'not_signed_in', false, false, 0, 91]], 'activation row');
  eq(rep.friction, ['stuckco: never signed in (72h since created)', 'stuckco: no model connected after 72h',
    'stuckco: no worker has ever run after 72h', 'stuckco: 12 error groups in 24h (yesterday 3)',
    'stuckco: 11 failed sign-ins in 24h', 'stuckco: disk 91% full'], 'friction');
  eq([s.labels['amux-errg'], s.labels['amux-act']], ['12', 'not_signed_in'], 'labels updated');
  eq(rep.problems, [], 'nothing else is wrong');
  // The 13:00 UTC run on a Wednesday with no other problem: the stuck customer alone sends the email.
  const RealDate = Date, at = RealDate.UTC(2026, 9, 21, 13, 0, 20);
  globalThis.Date = class extends RealDate { constructor(...a) { super(...(a.length ? a : [at])); } static now() { return at; } };
  s.created = new RealDate(at - 72 * 3600e3).toISOString();
  try {
    await call(w, async () => { const ps = []; await worker.scheduled({}, w.env, { waitUntil: p => ps.push(p) }); await Promise.all(ps); });
  } finally { globalThis.Date = RealDate; }
  const ops = w.mail.map(x => Buffer.from(x.raw, 'base64').toString()).filter(t => /To: hello@amux\.io/.test(t));
  eq(ops.length, 1, 'one operator email');
  eq([/Subject: \[amux cloud test\] 1 stuck customer\(s\)/.test(ops[0]), /Customers stuck in setup:/.test(ops[0]), /- stuckco \(72h, not_signed_in\): signed in no/.test(ops[0])],
     [true, true, true], 'subject, friction and activation table');
});

process.exitCode = failures;
console.log(failures ? `${failures} FAILED` : 'all passed');
