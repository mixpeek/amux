// The CRM tab, as the owner uses it (docs/design/crm.md; owner 2026-11-23:
// "build the CRM tab ... globally accessible in the amux worker list page ...
// clean on mobile ... all the capabilities needed to continue all our gtm
// shit, it should automatically sync", with contact info, ICP grade and
// lookalikes). Real server, real API, driven through the page.
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';

const api = (page: Page, method: string, path: string, body?: any) => page.evaluate(async ([m, p, b]) => {
  const r = await fetch('/api/crm' + p, { method: m, headers: (window as any)._authHeaders(b ? { 'Content-Type': 'application/json' } : {}), body: b ? JSON.stringify(b) : undefined });
  return { status: r.status, body: await r.json().catch(() => ({})) };
}, [method, path, body] as const);

async function openCrm(page: Page) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await page.locator('#tab-crm').click();
  await expect(page.locator('#crm-view')).toBeVisible();
  await expect(page.locator('#crm-chips .cx-filter', { hasText: 'People' })).toBeVisible();
}

test('a person is added, graded, linked to a company with lookalikes, a deal and a follow-up', async ({ page }, info) => {
  test.setTimeout(120_000);
  const R = info.project.name + '-' + Math.random().toString(36).slice(2, 6);
  const person = 'Ada Crm ' + R, company = 'Acme ' + R, email = `ada.${R}@acme-${R}.io`;
  await openCrm(page);

  // + New, as a person: contact info in, and the drawer opens on them.
  await page.locator('.crm-new').click();
  await page.locator('#crm-new-name').fill(person);
  await page.locator('#crm-new-email').fill(email);
  await page.locator('#crm-new-phone').fill('+1 (212) 555-0100');
  await page.locator('#crm-new-company').fill(company);
  await page.locator('#crm-new-linkedin').fill('linkedin.com/in/ada-' + R);
  await page.locator('#crm-drawer .btn.primary', { hasText: 'Add' }).click();
  const drawer = page.locator('#crm-drawer .cx-drawer');
  await expect(drawer.locator('.cx-dhead .crm-name')).toHaveText(person);
  await expect(drawer.locator('a[href^="mailto:"]')).toHaveAttribute('href', 'mailto:' + email);
  await expect(drawer.locator('a[href^="tel:"]')).toHaveAttribute('href', 'tel:+12125550100');
  await expect(drawer.locator('a', { hasText: 'LinkedIn' })).toHaveAttribute('href', 'https://linkedin.com/in/ada-' + R);
  await expect(drawer.locator('.crm-link', { hasText: company })).toBeVisible();   // the company was made and linked

  // ICP grade B, with why: the list says B.
  await drawer.locator('.crm-grades .cx-filter', { hasText: /^B$/ }).click();
  await expect(drawer.locator('.crm-grades .cx-filter.on')).toHaveText('B');
  await drawer.locator('textarea[aria-label="Why this grade"]').fill('Media team, 200 people, video search pain');
  await drawer.locator('textarea[aria-label="Why this grade"]').blur();
  await expect(page.locator('#crm-list .crm-row', { hasText: person }).locator('.crm-grade')).toHaveText('B');

  // Its company: add a lookalike, which reads back from the other side too.
  await drawer.locator('.crm-link', { hasText: company }).click();
  await expect(drawer.locator('.cx-dhead .crm-name')).toHaveText(company);
  await expect(drawer.locator('.crm-row', { hasText: person })).toBeVisible();
  await drawer.locator('#crm-look-name').fill('Beta ' + R);
  await drawer.locator('#crm-look-domain').fill(`beta-${R}.dev`);
  await drawer.locator('button', { hasText: 'Add lookalike' }).click();
  await expect(drawer.locator('.crm-row', { hasText: 'Beta ' + R })).toBeVisible();
  await drawer.locator('.crm-row', { hasText: 'Beta ' + R }).click();
  await expect(drawer.locator('.cx-dhead .crm-name')).toHaveText('Beta ' + R);
  await expect(drawer.locator('.crm-row', { hasText: company })).toBeVisible();
  await page.locator('#crm-drawer .cx-x').click();

  // A deal on the company, moved to demo.
  await page.locator('#crm-chips .cx-filter', { hasText: 'Deals' }).click();
  await page.locator('.crm-new').click();
  await page.locator('#crm-new-title').fill('Pilot ' + R);
  await page.locator('#crm-new-account').fill(company);
  await page.locator('#crm-new-amount').fill('12000');
  await page.locator('#crm-drawer .btn.primary', { hasText: 'Add' }).click();
  await expect(drawer.locator('.cx-dhead .crm-name')).toHaveText('Pilot ' + R);
  await drawer.locator('#crmf-stage').selectOption('demo');
  await page.locator('#crm-drawer .cx-x').click();
  await expect(page.locator('#crm-list .crm-stage-h', { hasText: 'demo' })).toBeVisible();
  await expect(page.locator('#crm-list .crm-row', { hasText: 'Pilot ' + R })).toContainText('USD 12,000');
  const deals = await api(page, 'GET', '/deals');
  const deal = deals.body.find((d: any) => d.title === 'Pilot ' + R);
  expect(deal.stage).toBe('demo');
  expect(deal.account_name).toBe(company);

  // A follow-up due today shows under Follow-ups.
  await page.locator('#crm-chips .cx-filter', { hasText: 'People' }).click();
  await page.locator('#crm-q').fill(person);
  await page.locator('#crm-list .crm-row', { hasText: person }).click();
  const today = await page.evaluate(() => (window as any)._crmToday());
  await drawer.locator('#crm-ix-type').selectOption('call');
  await drawer.locator('#crm-ix-notes').fill('Intro call, wants a demo');
  await drawer.locator('#crm-ix-fu').fill(today);
  await drawer.locator('button', { hasText: 'Log it' }).click();
  await expect(drawer.locator('.crm-ix', { hasText: 'Intro call, wants a demo' })).toBeVisible();
  await page.locator('#crm-drawer .cx-x').click();
  await page.locator('#crm-q').fill('');
  await page.locator('#crm-chips .cx-filter', { hasText: 'Follow-ups' }).click();
  await expect(page.locator('#crm-list .crm-row', { hasText: person })).toBeVisible();

  // The same email again opens the existing person instead of a duplicate.
  await page.locator('#crm-chips .cx-filter', { hasText: 'People' }).click();
  await page.locator('.crm-new').click();
  await page.locator('#crm-new-name').fill('Ada Again');
  await page.locator('#crm-new-email').fill(email.toUpperCase());
  await page.locator('#crm-drawer .btn.primary', { hasText: 'Add' }).click();
  await expect(drawer.locator('.cx-dhead .crm-name')).toHaveText(person);
  const people = await api(page, 'GET', '/contacts?q=' + encodeURIComponent('Ada'));
  expect(people.body.filter((p: any) => (p.email || '').toLowerCase() === email.toLowerCase()).length).toBe(1);
  await page.screenshot({ path: test.info().outputPath('crm-person.png') });
});

test('a change made elsewhere shows on an open CRM page without a reload', async ({ page, browser, baseURL }, info) => {
  test.skip(info.project.name !== 'desktop', 'the realtime path is the same on every width');
  const R = Math.random().toString(36).slice(2, 7);
  await openCrm(page);
  // A second device, or a worker: writes through the API.
  const ctx = await browser.newContext({ baseURL, ignoreHTTPSErrors: true,
    storageState: { cookies: [], origins: [{ origin: baseURL!, localStorage: [{ name: 'amux_walkthrough_done', value: '1' }, { name: 'amux_cloud_offer', value: '{"dismissed":1}' }] }] } });
  try {
    const other = await ctx.newPage();
    await other.goto('/');
    await other.waitForFunction(() => typeof (window as any)._authHeaders === 'function');
    const made = await api(other, 'POST', '/contacts', { name: 'Remote Person ' + R, email: `remote.${R}@example.com` });
    expect(made.status).toBe(201);
    await expect(page.locator('#crm-list .crm-row', { hasText: 'Remote Person ' + R })).toBeVisible({ timeout: 10_000 });
  } finally {
    await cleanup('close the second browser context', () => ctx.close());
  }
});

// gtm-engine imported 4,292 LinkedIn connections: the list comes a page at a time.
test('a long list comes 200 at a time, and Show more brings the next page', async ({ page }, info) => {
  test.skip(info.project.name !== 'desktop', 'paging is the same on every width');
  test.setTimeout(120_000);
  await openCrm(page);
  const R = Math.random().toString(36).slice(2, 7);
  await page.evaluate(async (r) => {
    const h = (window as any)._authHeaders({ 'Content-Type': 'application/json' });
    for (let i = 0; i < 205; i++) await fetch('/api/crm/contacts', { method: 'POST', headers: h, body: JSON.stringify({ name: 'Page ' + r + ' ' + i }) });
  }, R);
  await page.locator('#crm-q').fill('Page ' + R);
  await expect(page.locator('#crm-list .crm-row')).toHaveCount(200);
  await page.locator('.crm-more').click();
  await expect(page.locator('#crm-list .crm-row')).toHaveCount(205);
  await expect(page.locator('.crm-more')).toHaveCount(0);
});

// A worker updating the CRM while the owner types does not wipe the field.
test('a refresh from elsewhere does not rebuild a field being typed in', async ({ page }) => {
  const R = Math.random().toString(36).slice(2, 7);
  await openCrm(page);
  await page.locator('#crm-q').fill(R);   // this test's own rows, whatever else is in the CRM
  const made = await api(page, 'POST', '/contacts', { name: 'Typing ' + R, email: `typing.${R}@example.com` });
  await page.evaluate(id => (window as any)._crmOpen('person', id), made.body.id);
  const notes = page.locator('#crm-drawer textarea[aria-label="Why this grade"]');
  await notes.click();
  await notes.pressSequentially('half typed');
  await api(page, 'POST', '/contacts', { name: 'Elsewhere ' + R, email: `elsewhere.${R}@example.com` });   // a write elsewhere
  await expect(page.locator('#crm-list .crm-row', { hasText: 'Elsewhere ' + R })).toBeVisible({ timeout: 10_000 });
  await expect(notes).toHaveValue('half typed');
  await expect(notes).toBeFocused();
});

// Phone: one row of controls above the list, and the list gets the screen.
test('on a phone the CRM has one row of controls and the list fills the screen', async ({ page }, info) => {
  test.skip(info.project.name !== 'mobile', 'the 375 px layout');
  await openCrm(page);
  const bar = await page.locator('#crm-bar').boundingBox();
  const list = await page.locator('#crm-list').boundingBox();
  const vh = page.viewportSize()!.height;
  expect(bar!.height, 'one row').toBeLessThanOrEqual(64);
  for (const sel of ['#crm-q', '#crm-chips', '.crm-new']) {
    const b = await page.locator(sel).boundingBox();
    expect(b!.y + b!.height / 2, sel + ' sits in the one bar').toBeGreaterThan(bar!.y);
    expect(b!.y + b!.height / 2, sel + ' sits in the one bar').toBeLessThan(bar!.y + bar!.height);
  }
  expect(list!.height, 'the list gets at least half the screen').toBeGreaterThanOrEqual(vh / 2);
  const newBtn = await page.locator('.crm-new').boundingBox();
  expect(newBtn!.height, 'touch target').toBeGreaterThanOrEqual(44);
  await page.locator('.crm-new').click();
  const d = await page.locator('#crm-drawer .cx-drawer').boundingBox();
  expect(Math.round(d!.width), 'the drawer is the full width').toBe(page.viewportSize()!.width);
  await page.screenshot({ path: test.info().outputPath('crm-phone-new.png') });
});
