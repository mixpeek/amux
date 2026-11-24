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

  // A refresh whose fetch is still in flight when the owner starts typing
  // must not land over the field either (CI 45299042749, mobile).
  await notes.blur();
  let release!: () => void;
  const held = new Promise<void>(r => { release = r; });
  await page.route('**/api/crm/contacts/' + made.body.id, async route => { await held; await route.continue(); });
  await api(page, 'POST', '/contacts', { name: 'Later ' + R, email: `later.${R}@example.com` });
  await expect(page.locator('#crm-list .crm-row', { hasText: 'Later ' + R })).toBeVisible({ timeout: 10_000 });
  await notes.click();
  await notes.fill('typed mid-fetch');
  release();
  await page.waitForTimeout(500);
  await expect(notes).toHaveValue('typed mid-fetch');
  await page.unroute('**/api/crm/contacts/' + made.body.id);
});

// Owner, 2026-11-23: "a very agentic-oriented CRM": channels an agent reaches
// and watches, touches it plans and does, and the owner approves what goes out.
test('a worker plans a touch, the owner approves it, the worker records it done, and a watched channel reports into the timeline', async ({ page }, info) => {
  test.setTimeout(90_000);
  const R = info.project.name + '-' + Math.random().toString(36).slice(2, 6);
  await openCrm(page);
  const made = await api(page, 'POST', '/contacts', { name: 'Agent Target ' + R, email: `target.${R}@example.com` });
  const id = made.body.id;
  // A worker: plans a LinkedIn DM with a draft (its own session header).
  const asWorker = (m: string, p: string, b?: any) => page.evaluate(async ([mm, pp, bb]) => {
    const r = await fetch('/api/crm' + pp, { method: mm, headers: (window as any)._authHeaders({ 'Content-Type': 'application/json', 'X-Amux-Session': 'gtm-engine' }), body: bb ? JSON.stringify(bb) : undefined });
    return { status: r.status, body: await r.json().catch(() => ({})) };
  }, [m, p, b] as const);
  const t = await asWorker('POST', '/touches', { contact_id: id, channel: 'linkedin', action: 'dm', body: 'Hi, saw your post on video search ' + R });
  expect(t.status).toBe(201);
  expect((await asWorker('POST', '/touches/' + t.body.id + '/done', { result: 'sent' })).body.code).toBe('not_approved');
  // The owner's queue shows it; Approve hands it to the worker.
  await page.locator('#crm-chips .cx-filter', { hasText: 'To approve' }).click();
  const card = page.locator('#crm-list .crm-touch', { hasText: 'video search ' + R });
  await expect(card).toBeVisible();
  await card.locator('button', { hasText: 'Approve' }).click();
  await expect(card).toHaveCount(0);
  const ev = await page.evaluate(async () => (await fetch('/api/subscriptions/events?type=crm.touch.approved', { headers: (window as any)._authHeaders() })).json().catch(() => ({})));
  void ev;
  expect((await asWorker('POST', '/touches/' + t.body.id + '/done', { result: 'sent', result_url: 'https://www.linkedin.com/messaging/' })).status).toBe(200);
  // A watched careers page: the first check records, the second reports what is new.
  await page.locator('#crm-chips .cx-filter', { hasText: 'People' }).click();
  await page.locator('#crm-q').fill('Agent Target ' + R);
  await page.locator('#crm-list .crm-row', { hasText: 'Agent Target ' + R }).click();
  const drawer = page.locator('#crm-drawer .cx-drawer');
  await drawer.locator('#crm-ch-kind').selectOption('careers');
  await drawer.locator('#crm-ch-where').fill('https://example.com/careers-' + R);
  await drawer.locator('#crm-ch-w').check();
  await drawer.locator('.crm-form button', { hasText: 'Add' }).first().click();
  await expect(drawer.locator('.crm-ch', { hasText: 'careers' })).toContainText('watched');
  const chans = await api(page, 'GET', '/channels?owner_kind=contact&owner_id=' + id);
  const ch = chans.body[0].id;
  await asWorker('POST', '/channels/' + ch + '/observed', { items: [{ title: 'Video Engineer' }] });
  const second = await asWorker('POST', '/channels/' + ch + '/observed', { items: [{ title: 'Video Engineer' }, { title: 'Head of Search ' + R }] });
  expect(second.body.changed).toBe(true);
  // The timeline has the done DM and the new posting, without a reload.
  await expect(drawer.locator('.crm-ix', { hasText: 'Head of Search ' + R })).toBeVisible({ timeout: 10_000 });
  await expect(drawer.locator('.crm-touch', { hasText: 'video search ' + R })).toContainText('done');
  await expect(drawer.locator('.crm-ix', { hasText: 'linkedin dm' })).toBeVisible();
  await page.screenshot({ path: test.info().outputPath('crm-agent.png') });
});

// The voice (owner 2026-11-24: "a voice that we're able to capture and adhere
// to throughout each of these engagements and touchpoints"): a worker's draft
// that breaks it is refused with what broke; sent anyway with a reason, the
// owner's card says what broke and why, before Approve.
test('a draft that breaks the voice is refused, and one sent anyway shows why on the card', async ({ page }, info) => {
  const R = info.project.name + '-' + Math.random().toString(36).slice(2, 6);
  await openCrm(page);
  const asWorker = (m: string, p: string, b?: any) => page.evaluate(async ([mm, pp, bb]) => {
    const r = await fetch('/api/crm' + pp, { method: mm, headers: (window as any)._authHeaders({ 'Content-Type': 'application/json', 'X-Amux-Session': 'gtm-engine' }), body: bb ? JSON.stringify(bb) : undefined });
    return { status: r.status, body: await r.json().catch(() => ({})) };
  }, [m, p, b] as const);
  const v = await api(page, 'POST', '/voices', { sender: 'e2e-' + R, name: 'e2e', rules: { channels: { 'slack/dm': { max_chars: 30 } } } });
  expect(v.status).toBe(201);
  const made = await api(page, 'POST', '/contacts', { name: 'Voice Target ' + R, email: `voice.${R}@example.com` });
  const text = 'A slack answer that runs well past thirty characters ' + R;
  const draft = { contact_id: made.body.id, channel: 'slack', action: 'dm', voice: v.body.id, body: text };
  const refused = await asWorker('POST', '/touches', draft);
  expect(refused.status).toBe(422);
  expect(refused.body.code).toBe('crm_touch_voice');
  expect(refused.body.error).toContain('slack/dm is at most 30');
  const kept = await asWorker('POST', '/touches', { ...draft, voice_override: 'their thread asked for detail' });
  expect(kept.status).toBe(201);
  await page.locator('#crm-chips .cx-filter', { hasText: 'To approve' }).click();
  const line = page.locator('#crm-list .crm-touch', { hasText: R }).locator('.crm-voice');
  await expect(line).toContainText('slack/dm is at most 30');
  await expect(line).toContainText('sent as written because: their thread asked for detail');
});

// Outbound (owner 2026-11-24: "use every crm feature for amux outbound ...
// effective personal timely", "multiple touch points too offline and online").
test('a sequence mixes online and offline steps: step 1 drafted from a template, approved, the next planned, a letter recorded done', async ({ page }, info) => {
  test.setTimeout(90_000);
  const R = info.project.name + '-' + Math.random().toString(36).slice(2, 6);
  await openCrm(page);
  const made = await api(page, 'POST', '/contacts', { name: 'Seq Target ' + R, email: `seq.${R}@example.com`, company: 'Acme ' + R, role: 'CTO' });
  const id = made.body.id;
  const asWorker = (m: string, p: string, b?: any) => page.evaluate(async ([mm, pp, bb]) => {
    const r = await fetch('/api/crm' + pp, { method: mm, headers: (window as any)._authHeaders({ 'Content-Type': 'application/json', 'X-Amux-Session': 'amux-gtm' }), body: bb ? JSON.stringify(bb) : undefined });
    return { status: r.status, body: await r.json().catch(() => ({})) };
  }, [m, p, b] as const);
  const seq = await asWorker('POST', '/sequences', { name: 'Outbound ' + R, goal: 'book a demo', steps: [
    { day: 0, channel: 'email', action: 'email', template: 'Hi {first_name}, a note for the {role} of {company} ' + R },
    { day: 0, channel: 'post', action: 'letter' }] });
  expect(seq.status).toBe(201);
  const en = await asWorker('POST', '/sequences/' + seq.body.id + '/enroll', { contact_ids: [id] });
  expect(en.body.enrolled.length).toBe(1);
  // A second open email to the same person is refused, with the open one named.
  const dup = await asWorker('POST', '/touches', { contact_id: id, channel: 'email', action: 'email', body: 'again' });
  expect([dup.status, dup.body.code]).toEqual([409, 'crm_touch_duplicate']);
  // The owner sees the filled draft, with its sequence and step, and approves it.
  await page.locator('#crm-chips .cx-filter', { hasText: 'To approve' }).click();
  const card = page.locator('#crm-list .crm-touch', { hasText: 'Seq Target ' + R });
  await expect(card).toContainText(seq.body.id + ' step 1');
  await expect(card.locator('textarea')).toHaveValue('Hi Seq, a note for the CTO of Acme ' + R + ' ' + R);
  await card.locator('button', { hasText: 'Approve' }).click();
  await expect(card).toHaveCount(0);
  expect((await asWorker('POST', '/touches/' + en.body.enrolled[0].touch_id + '/done', { result: 'sent' })).body.touch.next_touch).toBeTruthy();
  // Step 2, a letter by post, is planned; drafted, approved, then recorded done from the tab.
  const touches = await api(page, 'GET', '/touches?contact_id=' + id);
  const letter = touches.body.find((t: any) => t.step === 2);
  expect([letter.channel, letter.action, letter.status]).toEqual(['post', 'letter', 'planned']);
  await asWorker('PATCH', '/touches/' + letter.id, { body: 'Handwritten card with the amux sticker ' + R });
  await api(page, 'POST', '/touches/' + letter.id + '/approve');
  await page.locator('#crm-chips .cx-filter', { hasText: 'People' }).click();
  await page.locator('#crm-q').fill('Seq Target ' + R);
  await page.locator('#crm-list .crm-row', { hasText: 'Seq Target ' + R }).click();
  const drawer = page.locator('#crm-drawer .cx-drawer');
  const lc = drawer.locator('.crm-touch', { hasText: 'amux sticker ' + R });
  await lc.locator('button', { hasText: 'Done' }).click();
  await expect(lc).toContainText('done', { timeout: 10_000 });
  // The Sequences view shows it with its steps.
  await page.locator('#crm-drawer .cx-drawer-close, #crm-drawer [aria-label="Close"]').first().click().catch(() => {});
  await page.locator('#crm-chips .cx-filter', { hasText: 'Sequences' }).click();
  const row = page.locator('#crm-list .crm-row', { hasText: 'Outbound ' + R });
  await expect(row).toContainText('email email · day 0 post letter');
  // The brief: one read with the person, activity and touches.
  const brief = await api(page, 'GET', '/contacts/' + id + '/brief');
  expect([brief.status, brief.body.can_reach.ok, brief.body.touches.length]).toEqual([200, true, 2]);
  await page.screenshot({ path: test.info().outputPath('crm-sequence.png') });
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
