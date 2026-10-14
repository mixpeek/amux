// Fake-mode journey: checkout, provisioning, heartbeat, door hand-off, redirects, replays,
// waitlist, kill switch. Run through run_e2e.sh, which starts the control plane on a temp DB.
import { chromium } from 'playwright';
import { execSync } from 'node:child_process';
const CP = 'http://127.0.0.1:18990', DB = process.argv[2];
const sql = q => execSync(`sqlite3 -json ${DB} "${q}"`).toString().trim();
const b = await chromium.launch(); const ctx = await b.newContext({ viewport: { width: 375, height: 800 } });
const p = await ctx.newPage(); const t0 = Date.now(); const out = {};
// J3: plan link from the site goes straight to checkout
await p.goto(`${CP}/?plan=solo&source=e2e`);
out.checkout_url = p.url().replace(CP, '');
await p.fill('input[name=email]', 'journey@example.com');
await Promise.all([p.waitForURL(/\/progress/), p.click('#pay')]);
out.after_pay = p.url().replace(CP, '');
await p.waitForTimeout(6500);                               // provisioning loop ticks every 5 s in fake mode
const srv = JSON.parse(sql("SELECT id, slug, fleet_secret, status, hetzner_id FROM servers"))[0];
out.server_before_heartbeat = { slug: srv.slug, status: srv.status, has_hetzner: !!srv.hetzner_id };
out.progress_steps = await p.$$eval('ol.steps li', ls => ls.map(l => l.className || '-').join(','));
const doorWait = new Promise(res => { p.on('request', r => { if (r.url().includes('/_door/enter')) res(r.url()); }); setTimeout(() => res(null), 9000); });
// The fleet agent's first heartbeat: serving, with a first backup and a CSR.
const hb = await fetch(`${CP}/api/fleet/${srv.id}/heartbeat`, { method: 'POST', headers: { Authorization: `Bearer ${srv.fleet_secret}`, 'Content-Type': 'application/json' },
  body: JSON.stringify({ version: '0.9.1300', serving: true, last_backup_at: Date.now() / 1000, csr: 'CSR' }) }).then(r => r.json());
out.heartbeat = { ready: hb.ready, cert: !!hb.origin_certificate };
out.wrong_fleet_token = (await fetch(`${CP}/api/fleet/${srv.id}/heartbeat`, { method: 'POST', headers: { Authorization: 'Bearer wrong' }, body: '{}' })).status;
const door = await doorWait;
out.door_url = door && door.replace(/t=.*/, 't=<token>');
out.seconds_paid_to_door = Math.round((Date.now() - t0) / 1000);
const root = await ctx.request.get(`${CP}/`, { maxRedirects: 0 });
out.root_signed_in = root.headers()['location'];
const anon = await (await b.newContext()).request.get(`${CP}/`, { maxRedirects: 0 });
out.root_signed_out = anon.headers()['location'];
const sid = JSON.parse(sql("SELECT checkout_session_id FROM signups"))[0].checkout_session_id;
await ctx.request.get(`${CP}/welcome?session_id=${sid}`, { maxRedirects: 0 });
await fetch(`${CP}/webhooks/stripe`, { method: 'POST', body: JSON.stringify({ id: 'evt_1', type: 'checkout.session.completed', data: { object: { id: sid, mode: 'subscription' } } }) });
out.counts = JSON.parse(sql("SELECT (SELECT COUNT(*) FROM servers) servers, (SELECT COUNT(*) FROM provision_jobs) jobs, (SELECT COUNT(*) FROM users) users, (SELECT COUNT(*) FROM outbox) emails"))[0];
out.notify = (await fetch(`${CP}/api/notify`, { method: 'POST', body: JSON.stringify({ email: 'w@example.com', source: 'e2e' }) })).status;
await fetch(`${CP}/api/admin/flags`, { method: 'POST', headers: { Authorization: 'Bearer adm' }, body: JSON.stringify({ provisioning_freeze: 1 }) });
out.start_when_frozen = (await ctx.request.get(`${CP}/start?plan=team`, { maxRedirects: 0 })).status();
out.admin_without_token = (await fetch(`${CP}/api/admin/flags`)).status;
await p.screenshot({ path: process.argv[3] });
console.log(JSON.stringify(out, null, 1));
await b.close();
const expect = {
  checkout_url: v => v.startsWith('/fake/checkout/'), after_pay: v => v === '/progress',
  heartbeat: v => v.ready === true && v.cert === true, wrong_fleet_token: v => v === 401,
  door_url: v => /^https:\/\/journey\.amux\.cloud\/_door\/enter\?t=/.test(v || ''),
  root_signed_in: v => (v || '').startsWith('/go/'), root_signed_out: v => (v || '').endsWith('/pricing/'),
  counts: v => v.servers === 1 && v.jobs === 1 && v.users === 1 && v.emails === 1,
  notify: v => v === 200, start_when_frozen: v => v === 503, admin_without_token: v => v === 401,
};
const failed = Object.entries(expect).filter(([k, ok]) => !ok(out[k])).map(([k]) => k);
console.log(failed.length ? `FAIL ${failed.join(', ')}` : `PASS ${Object.keys(expect).length} checks`);
process.exit(failed.length ? 1 : 0);
