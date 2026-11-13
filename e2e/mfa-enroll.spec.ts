// The second-factor page (governance PR 20, api/mfa.rs): an invited member
// opens /mfa, adds the key to an authenticator, enters the code it shows, and
// gets ten recovery codes; on a reload the page says this session has proved
// its factor. The code is computed here the way an authenticator app does.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';
import crypto from 'node:crypto';

function totp(secretB32: string, t = Date.now() / 1000): string {
  const A = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
  let bits = 0, val = 0;
  const out: number[] = [];
  for (const ch of secretB32.toUpperCase()) {
    val = (val << 5) | A.indexOf(ch);
    bits += 5;
    if (bits >= 8) { out.push((val >>> (bits - 8)) & 255); bits -= 8; }
  }
  const ctr = Buffer.alloc(8);
  ctr.writeBigUInt64BE(BigInt(Math.floor(t / 30)));
  const h = crypto.createHmac('sha1', Buffer.from(out)).update(ctr).digest();
  const o = h[h.length - 1] & 15;
  return String((h.readUInt32BE(o) & 0x7fffffff) % 1e6).padStart(6, '0');
}

test('a member turns on a second factor at /mfa and gets recovery codes', async ({ page: owner, browser, request }, testInfo) => {
  test.setTimeout(90_000);
  await owner.goto('/');
  await owner.waitForFunction(() => '_AMUX_AUTH_TOKEN' in window);
  const ownerToken = await owner.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const ownerHeaders = { Authorization: `Bearer ${ownerToken}`, 'Content-Type': 'application/json' };
  const email = `mfa-${testInfo.project.name.replace(/[^a-z0-9]/gi, '').toLowerCase()}@example.com`;

  const inv = await request.post('/api/org/invites', {
    headers: ownerHeaders, data: { email, scope_level: 'global', grant_host_access: true },
  });
  expect(inv.ok(), `${inv.status()} ${await inv.text()}`).toBeTruthy();
  const invBody = await inv.json();
  const token = invBody.token || String(invBody.url || '').split('/invite/')[1];

  const ctx = await browser.newContext({ ignoreHTTPSErrors: true, serviceWorkers: 'block' });
  const guest = await ctx.newPage();
  const removeMember = async () => {
    await request.post('/api/mfa/reset', { headers: ownerHeaders, data: { subject: `member:${email}` } });
    const members = await (await request.get('/api/org/members', { headers: ownerHeaders })).json();
    const list = Array.isArray(members) ? members : members.members || [];
    for (const m of list.filter((m: any) => m.email === email)) {
      await request.delete(`/api/org/members/${m.id}`, { headers: ownerHeaders });
    }
    await ctx.close();
  };
  try {
    await guest.goto(`/invite/${token}`);
    await guest.locator('#name').fill('MFA Tester');
    await Promise.all([
      guest.waitForURL((url) => url.pathname === '/'),
      guest.getByRole('button', { name: 'Join workspace' }).click(),
    ]);

    await guest.goto('/mfa');
    await expect(guest.getByText(`member:${email}`)).toBeVisible();
    const secret = (await guest.locator('#secret').textContent())!.trim();
    expect(secret).toMatch(/^[A-Z2-7]{32}$/);
    await expect(guest.locator('#uri')).toHaveAttribute('href', /^otpauth:\/\/totp\/amux:/);

    await guest.locator('#code').fill('000000');
    await guest.locator('#go').click();
    await expect(guest.locator('#msg')).toContainText('not right');

    await guest.locator('#code').fill(totp(secret));
    await guest.locator('#go').click();
    await expect(guest.locator('#done')).toContainText('Second factor is on');
    await expect(guest.locator('#codes li')).toHaveCount(10);
    await guest.screenshot({ path: testInfo.outputPath('mfa-recovery-codes.png') });

    await guest.reload();
    await expect(guest.locator('#done')).toContainText('proved its second factor');
  } finally {
    await cleanup('remove the MFA test member', removeMember, testInfo);
  }
});
