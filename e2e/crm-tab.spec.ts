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

// A section: the rail on a desktop, the first chips of the one row on a phone.
async function crmView(page: Page, label: string) {
  const item = page.locator('#crm-nav .crm-nav-item:visible, #crm-chips .crm-vchip:visible').filter({ hasText: label }).first();
  await item.click();
  await expect(item).toHaveClass(/\bon\b/);
}

async function openCrm(page: Page, view = 'People') {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await page.locator('#tab-crm').click();
  await expect(page.locator('#crm-view')).toBeVisible();
  await crmView(page, view);
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
  await crmView(page, 'Deals');
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
  await crmView(page, 'People');
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
  await crmView(page, 'Follow-ups');
  await expect(page.locator('#crm-list .crm-row', { hasText: person })).toBeVisible();

  // The same email again opens the existing person instead of a duplicate.
  await crmView(page, 'People');
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
  await crmView(page, 'Outbound');
  await page.locator('#crm-q').fill('video search ' + R);
  const card = page.locator('#crm-list .crm-row', { hasText: 'Agent Target ' + R });
  await card.click();
  const review = page.locator('#crm-drawer .crm-touch-detail');
  await expect(review.locator('.crm-msg')).toContainText('video search ' + R);
  await review.locator('.crm-approve').click();
  await expect(card).toHaveCount(0);
  await page.locator('#crm-q').fill('');
  const ev = await page.evaluate(async () => (await fetch('/api/subscriptions/events?type=crm.touch.approved', { headers: (window as any)._authHeaders() })).json().catch(() => ({})));
  void ev;
  expect((await asWorker('POST', '/touches/' + t.body.id + '/done', { result: 'sent', result_url: 'https://www.linkedin.com/messaging/' })).status).toBe(200);
  // A watched careers page: the first check records, the second reports what is new.
  await crmView(page, 'People');
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
  await crmView(page, 'Outbound');
  await page.locator('#crm-q').fill(R);
  // Kept past the voice with a reason: it waits in Pending (not Blocked), and the review says what broke and why.
  await page.locator('#crm-list .crm-row', { hasText: 'Voice Target ' + R }).click();
  const review = page.locator('#crm-drawer .crm-touch-detail');
  await expect(review.locator('.crm-check.bad', { hasText: 'Voice' })).toContainText('slack/dm is at most 30');
  await expect(review).toContainText('Sent as written because: their thread asked for detail');
  await expect(review.locator('.crm-approve')).toBeEnabled();
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
  await crmView(page, 'Outbound');
  await page.locator('#crm-q').fill('Seq Target ' + R);
  const card = page.locator('#crm-list .crm-row', { hasText: 'Seq Target ' + R });
  await card.click();
  const review = page.locator('#crm-drawer .crm-touch-detail');
  await expect(review).toContainText('Outbound ' + R + ' · Step 1 of 2 · drafted by amux-gtm');
  await expect(review.locator('.crm-msg')).toHaveText('Hi Seq, a note for the CTO of Acme ' + R + ' ' + R);
  await expect(review.locator('.crm-check', { hasText: 'Channel verified' })).toHaveClass(/\bok\b/);
  await review.locator('.crm-approve').click();
  await expect(card).toHaveCount(0);
  await page.locator('#crm-q').fill('');
  expect((await asWorker('POST', '/touches/' + en.body.enrolled[0].touch_id + '/done', { result: 'sent' })).body.touch.next_touch).toBeTruthy();
  // Step 2, a letter by post, is planned; drafted, approved, then recorded done from the tab.
  const touches = await api(page, 'GET', '/touches?contact_id=' + id);
  const letter = touches.body.find((t: any) => t.step === 2);
  expect([letter.channel, letter.action, letter.status]).toEqual(['post', 'letter', 'planned']);
  await asWorker('PATCH', '/touches/' + letter.id, { body: 'Handwritten card with the amux sticker ' + R });
  await api(page, 'POST', '/touches/' + letter.id + '/approve');
  await crmView(page, 'People');
  await page.locator('#crm-q').fill('Seq Target ' + R);
  await page.locator('#crm-list .crm-row', { hasText: 'Seq Target ' + R }).click();
  const drawer = page.locator('#crm-drawer .cx-drawer');
  const lc = drawer.locator('.crm-touch', { hasText: 'amux sticker ' + R });
  await lc.locator('button', { hasText: 'Done' }).click();
  await expect(lc).toContainText('done', { timeout: 10_000 });
  // The Sequences view shows it with its steps.
  await page.locator('#crm-drawer .cx-drawer-close, #crm-drawer [aria-label="Close"]').first().click().catch(() => {});
  await crmView(page, 'Sequences');
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

// Owner, 2026-11-27: "Enhance the crm in amux ux ... the goal is agentic crm
// powered by workers". Outbound is the review queue of what workers drafted:
// Pending, Blocked (cannot go out as it stands) and History, each draft with
// its message, the person's brief and the checks it passed. The owner edits,
// approves, declines with a note the worker reads, and the next draft opens.
// Activity lists the events workers subscribe to.
test('outbound review: a draft is read with its brief and checks, edited, approved once on a double tap, the next declined with a note, and both land in history and activity', async ({ page }, info) => {
  test.setTimeout(120_000);
  const R = info.project.name + '-' + Math.random().toString(36).slice(2, 6);
  const W = 'crm-e2e-' + R;
  const phone = (page.viewportSize()?.width || 1280) < 1000;
  await openCrm(page, 'Outbound');
  const asWorker = (m: string, p: string, b?: any) => page.evaluate(async ([mm, pp, bb, w]) => {
    const r = await fetch('/api/crm' + pp, { method: mm, headers: (window as any)._authHeaders({ 'Content-Type': 'application/json', 'X-Amux-Session': w }), body: bb ? JSON.stringify(bb) : undefined });
    return { status: r.status, body: await r.json().catch(() => ({})) };
  }, [m, p, b, W] as const);
  const person = async (name: string, extra: any = {}) => (await api(page, 'POST', '/contacts', { name, email: `${name.split(' ')[0].toLowerCase()}.${R}@renderwell-${R}.io`, company: 'Renderwell ' + R, role: 'Founder', icp_grade: 'A', notes: 'Starred amux after exploring tools for concurrent coding sessions ' + R, ...extra })).body.id;
  const nina = await person('Nina Park'), jon = await person('Jon Bell'), sam = await person('Sam Reed');
  const draft = async (id: string, text: string) => { const t = await asWorker('POST', '/touches', { contact_id: id, channel: 'linkedin', action: 'dm', body: text }); expect(t.status).toBe(201); return t.body.id; };
  const tNina = await draft(nina, 'Hey Nina, saw you starred amux ' + R);
  const tJon = await draft(jon, 'Hey Jon, how do you hand off agent work ' + R);
  await draft(sam, 'Hey Sam ' + R);
  // Sam opts out after the draft was written: it can no longer go out.
  expect((await api(page, 'PATCH', '/contacts/' + sam, { unsubscribed: true })).status).toBe(200);

  // Pending holds the two that can go out; Blocked the one whose person opted out.
  await crmView(page, 'Outbound');   // seeded before the page's live stream may be up: open the section afresh
  await page.locator('#crm-q').fill(R);
  const rows = page.locator('#crm-list .crm-row');
  await expect(rows).toHaveCount(2);
  await expect(rows.filter({ hasText: 'Sam Reed' })).toHaveCount(0);
  await page.locator('#crm-chips .cx-filter', { hasText: 'Blocked' }).click();
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText('Sam Reed');
  await expect(rows.first().locator('.cx-pill')).toHaveText('Blocked');
  await rows.first().click();
  const review = page.locator('#crm-drawer .crm-touch-detail');
  await expect(review.locator('.crm-blocked')).toContainText('They unsubscribed');
  await expect(review.locator('.crm-approve')).toBeDisabled();
  if (phone) await review.locator('.crm-back').click();
  await page.locator('#crm-chips .cx-filter', { hasText: 'Pending' }).click();

  // The counts are the server's, for every touch.
  if (!phone) {
    const s = (await api(page, 'GET', '/summary')).body;
    await expect(page.locator('.crm-stat', { hasText: 'Pending review' }).locator('.crm-stat-n')).toHaveText(String(s.to_approve - s.blocked));
    await expect(page.locator('.crm-stat', { hasText: 'Needs attention' }).locator('.crm-stat-n')).toHaveText(String(s.blocked));
    await expect(page.locator('#crm-nav .crm-nav-item', { hasText: 'Outbound' }).locator('.crm-nav-n')).toHaveText(String(s.to_approve));
  }

  // Nina's draft: the message, who she is, and what it passed.
  await rows.filter({ hasText: 'Nina Park' }).click();
  await expect(rows.filter({ hasText: 'Nina Park' })).toHaveClass(/\bon\b/);
  await expect(review.locator('.crm-dtitle')).toHaveText('Hey Nina, saw you starred amux ' + R);
  await expect(review.locator('.crm-card', { hasText: 'Contact brief' })).toContainText('Renderwell ' + R);
  await expect(review.locator('.crm-card', { hasText: 'Contact brief' })).toContainText('concurrent coding sessions');
  await expect(review.locator('.crm-card', { hasText: 'Sequence and checks' })).toContainText('One-off touch · drafted by ' + W);
  await expect(review.locator('.crm-check', { hasText: 'Channel verified' })).toHaveClass(/\bok\b/);

  // Edit the draft; the saved text is what the worker sends.
  await review.locator('button', { hasText: 'Edit draft' }).click();
  await review.locator('#crm-edit-body').fill('Hey Nina, thanks for the star. What do you run in amux? ' + R);
  await review.locator('button', { hasText: 'Save draft' }).click();
  await expect(review.locator('.crm-msg')).toHaveText('Hey Nina, thanks for the star. What do you run in amux? ' + R);
  expect((await api(page, 'GET', '/touches?contact_id=' + nina)).body[0].body).toBe('Hey Nina, thanks for the star. What do you run in amux? ' + R);

  // A double tap on Approve approves Nina once; Jon's draft opens next and stays held.
  await review.locator('.crm-approve').dblclick();
  await expect(review.locator('.crm-dtitle')).toHaveText('Hey Jon, how do you hand off agent work ' + R);
  await page.waitForTimeout(1000);
  const after = (await api(page, 'GET', '/touches?limit=1000')).body;
  expect(after.find((t: any) => t.id === tNina).status).toBe('approved');
  expect(after.find((t: any) => t.id === tJon).status).toBe('drafted');

  // Decline Jon's with a note: the worker reads why.
  await review.locator('button', { hasText: 'Decline' }).click();
  await page.locator('#modal-prompt-input').fill('too generic ' + R);
  await page.locator('#modal-prompt-input').press('Enter');
  await expect(rows).toHaveCount(0);
  const jt = (await api(page, 'GET', '/touches?contact_id=' + jon)).body[0];
  expect([jt.status, jt.result]).toEqual(['skipped', 'declined by the owner: too generic ' + R]);

  // History has both, by what happened.
  await page.locator('#crm-chips .cx-filter', { hasText: 'History' }).click();
  await expect(rows.filter({ hasText: 'Nina Park' }).locator('.cx-pill')).toHaveText('Approved');
  await expect(rows.filter({ hasText: 'Jon Bell' }).locator('.cx-pill')).toHaveText('Declined');
  // Survives a reload: the section and its data come back.
  await page.reload();
  await page.waitForFunction(() => typeof (window as any).switchView === 'function');
  await page.locator('#tab-crm').click();
  await expect(page.locator('#crm-nav .crm-nav-item.on:visible, #crm-chips .crm-vchip.on:visible').filter({ hasText: 'Outbound' })).toBeVisible();

  // Activity: the approval is an event a worker can subscribe to; tapping it opens Nina.
  await crmView(page, 'Activity');
  await page.locator('#crm-q').fill(W);   // this run's worker: repeats share one server
  const ev = page.locator('#crm-list .crm-act', { hasText: 'crm.touch.approved' }).filter({ hasText: 'Nina Park' });
  await expect(ev.first()).toContainText(W + ' does it');
  await ev.first().click();
  await expect(page.locator('#crm-drawer .cx-dhead .crm-name')).toHaveText('Nina Park');
  await expect(page.locator('#crm-drawer .cx-sec', { hasText: /^Sequences$/ })).toBeVisible();
  await page.screenshot({ path: test.info().outputPath('crm-activity.png') });
});

// Phone: Outbound keeps one row of controls, and a draft opens over the list with a way back.
test('on a phone Outbound has one row of controls, and a draft opens full width with a way back', async ({ page }, info) => {
  test.skip(info.project.name === 'desktop', 'the phone layout; desktop is covered by the review test');
  const R = info.project.name + '-' + Math.random().toString(36).slice(2, 6);
  await openCrm(page, 'Outbound');
  const made = await api(page, 'POST', '/contacts', { name: 'Phone Target ' + R, email: `phone.${R}@example.com` });
  const t = await page.evaluate(async ([id, r]) => (await fetch('/api/crm/touches', { method: 'POST', headers: (window as any)._authHeaders({ 'Content-Type': 'application/json', 'X-Amux-Session': 'crm-e2e-phone' }), body: JSON.stringify({ contact_id: id, channel: 'email', action: 'email', body: 'Subject: hello ' + r + '\n\nHi there' }) })).status, [made.body.id, R] as const);
  expect(t).toBe(201);
  await crmView(page, 'Outbound');
  await page.locator('#crm-q').fill(R);
  const bar = (await page.locator('#crm-bar').boundingBox())!;
  expect(bar.height, 'one row').toBeLessThanOrEqual(64);
  await expect(page.locator('#crm-head')).toBeHidden();
  await expect(page.locator('#crm-nav')).toBeHidden();
  const list = (await page.locator('#crm-list').boundingBox())!;
  expect(list.height, 'the list gets at least half the screen').toBeGreaterThanOrEqual(page.viewportSize()!.height / 2);
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(overflow, 'nothing pushes the page sideways').toBeLessThanOrEqual(0);
  await page.locator('#crm-list .crm-row', { hasText: 'Phone Target ' + R }).click();
  const review = page.locator('#crm-drawer .crm-touch-detail');
  await expect(review.locator('.crm-dtitle')).toHaveText('hello ' + R);
  expect(Math.round((await review.boundingBox())!.width), 'full width').toBe(page.viewportSize()!.width);
  for (const b of ['.crm-approve', '.crm-back']) expect((await review.locator(b).boundingBox())!.height, b + ' touch target').toBeGreaterThanOrEqual(44);
  await page.screenshot({ path: test.info().outputPath('crm-phone-review.png') });
  await review.locator('.crm-back').click();
  await expect(page.locator('#crm-drawer')).toHaveCount(0);
});

// Closing the loop (docs/design/crm-outbound-acceptance.md sections 10 to 16;
// owner 2026-11-27: "it barely covers closing, and closing is the point").
// A worker reads a reply as interested, a booking moves the deal to demo and
// plans the prep, the owner signs it won only with its contract value and close
// date (a contract, not a subscription), and the Funnel shows the value signed.
test('closing: a reply read as interested, a booking to demo, won only with its numbers, and the funnel counts it', async ({ page }, info) => {
  test.setTimeout(120_000);
  const R = info.project.name + '-' + Math.random().toString(36).slice(2, 6);
  const W = 'crm-close-' + R;
  const phone = (page.viewportSize()?.width || 1280) < 1000;
  await openCrm(page, 'Deals');
  const asWorker = (m: string, p: string, b?: any) => page.evaluate(async ([mm, pp, bb, w]) => {
    const r = await fetch('/api/crm' + pp, { method: mm, headers: (window as any)._authHeaders({ 'Content-Type': 'application/json', 'X-Amux-Session': w }), body: bb ? JSON.stringify(bb) : undefined });
    return { status: r.status, body: await r.json().catch(() => ({})) };
  }, [m, p, b, W] as const);
  const acct = (await api(page, 'POST', '/accounts', { name: 'Closeco ' + R, domain: `closeco-${R}.io` })).body.id;
  const nina = (await api(page, 'POST', '/contacts', { name: 'Nina Close ' + R, email: `nina.${R}@closeco.io`, linkedin: 'linkedin.com/in/nina-' + R, account_id: acct })).body.id;
  const seq = (await asWorker('POST', '/sequences', { name: 'Close ' + R, steps: [{ channel: 'linkedin', action: 'dm', template: 'Hi {first_name}' }, { day: 3, channel: 'linkedin', action: 'comment' }] })).body.id;
  const en = await asWorker('POST', '/sequences/' + seq + '/enroll', { contact_ids: [nina], cohort: 'cohort-' + R });
  const t1 = en.body.enrolled[0].touch_id;
  expect((await api(page, 'POST', '/touches/' + t1 + '/approve')).status).toBe(200);
  expect((await asWorker('POST', '/touches/' + t1 + '/done', { result: 'sent' })).status).toBe(200);
  // Their reply, then the worker's reading of it.
  const ix = (await asWorker('POST', '/contacts/' + nina + '/interactions', { type: 'dm', direction: 'in', notes: 'sounds great, tell me more ' + R })).body.id;
  const read = await asWorker('POST', '/replies/' + ix + '/intent', { intent: 'interested', why: 'asked for more' });
  expect(read.status).toBe(200);
  // A booking: the deal moves to demo and a prep touch is planned.
  expect((await asWorker('POST', '/contacts/' + nina + '/interactions', { type: 'meeting', notes: 'calendly ' + R })).status).toBe(201);
  const deal = (await api(page, 'GET', '/deals?account_id=' + acct)).body[0];
  expect([deal.stage, deal.next_step.startsWith('prep the demo')]).toEqual(['demo', true]);
  // A worker cannot sign it.
  expect((await asWorker('PATCH', '/deals/' + deal.id, { stage: 'won', amount: 1, mrr_usd: 1, close_date: '2026-12-20' })).body.code).toBe('crm_deal_owner_stage');

  // The owner opens the deal: its history, then won is refused until the numbers are in.
  await crmView(page, 'Deals');
  await page.locator('#crm-list .crm-row', { hasText: deal.title }).click();
  const rec = page.locator('#crm-drawer .cx-drawer');
  await expect(rec.locator('.crm-stage-row')).toHaveCount(3);
  await expect(rec.locator('.crm-stage-row').last()).toContainText('qualification to demo');
  await rec.locator('#crmf-stage').selectOption('won');
  await expect(page.locator('#toast')).toContainText('a won deal needs');
  await expect.poll(async () => (await api(page, 'GET', '/deals/' + deal.id)).body.stage).toBe('demo');
  await rec.locator('#crmf-amount').fill('12000'); await rec.locator('#crmf-amount').blur();
  await expect.poll(async () => (await api(page, 'GET', '/deals/' + deal.id)).body.amount).toBe(12000);
  await rec.locator('[data-path$="' + deal.id + '"][type="date"]').fill('2026-12-20');
  await rec.locator('[data-path$="' + deal.id + '"][type="date"]').blur();
  await expect.poll(async () => (await api(page, 'GET', '/deals/' + deal.id)).body.close_date).toBe('2026-12-20');
  await rec.locator('#crmf-stage').selectOption('won');
  await expect.poll(async () => (await api(page, 'GET', '/deals/' + deal.id)).body.stage).toBe('won');
  await expect(rec.locator('.crm-stage-row').last()).toContainText('demo to won', { timeout: 10_000 });
  if (phone) await page.locator('#crm-drawer [aria-label="Close"]').first().click();

  // The person's timeline shows what the reply meant.
  await crmView(page, 'People');
  await page.locator('#crm-q').fill('Nina Close ' + R);
  await page.locator('#crm-list .crm-row', { hasText: 'Nina Close ' + R }).click();
  await expect(page.locator('#crm-drawer .crm-ix', { hasText: 'tell me more ' + R }).locator('.cx-pill')).toHaveText('interested');
  await page.locator('#crm-drawer [aria-label="Close"]').first().click();

  // The funnel: this cohort's one person at every step, and the contract value signed.
  await page.locator('#crm-q').fill('');
  await crmView(page, 'Funnel');
  await page.locator('#crm-chips .cx-filter', { hasText: 'By cohort' }).click();
  const card = page.locator('#crm-list .crm-funnel[data-key="cohort-' + R + '"]');
  for (const step of ['enrolled', 'contacted', 'replied', 'booked', 'demoed', 'won']) await expect(card.locator('.crm-step[data-step="' + step + '"] .crm-step-n')).toHaveText('1');
  await expect(card).toContainText('$12,000 signed');
  const f = await api(page, 'GET', '/funnel?group=cohort&cohort=cohort-' + R);
  expect([f.body.contract, f.body.groups[0].won, f.body.groups[0].won_amount_usd, f.body.groups[0].mrr_usd]).toEqual(['crm.funnel.v1', 1, 12000, 0]);
  await page.screenshot({ path: test.info().outputPath('crm-funnel.png') });
});

// Desktop: several drafts decided at once; each still goes through its own approve.
test('bulk: the owner approves two drafts at once and declines one with a note', async ({ page }, info) => {
  test.skip(info.project.name !== 'desktop', 'a phone decides one draft at a time (no bulk row there)');
  const R = Math.random().toString(36).slice(2, 7);
  await openCrm(page, 'Outbound');
  const ids: string[] = [];
  for (const n of ['A', 'B', 'C']) {
    const p = (await api(page, 'POST', '/contacts', { name: 'Bulk ' + n + ' ' + R, email: `bulk${n}.${R}@example.com` })).body.id;
    const t = await page.evaluate(async ([id, r]) => (await (await fetch('/api/crm/touches', { method: 'POST', headers: (window as any)._authHeaders({ 'Content-Type': 'application/json', 'X-Amux-Session': 'crm-bulk' }), body: JSON.stringify({ contact_id: id, channel: 'linkedin', action: 'dm', body: 'Hi there ' + r }) })).json()).id, [p, R] as const);
    ids.push(t);
  }
  await crmView(page, 'Outbound');
  await page.locator('#crm-q').fill(R);
  const rows = page.locator('#crm-list .crm-row');
  await expect(rows).toHaveCount(3);
  await rows.filter({ hasText: 'Bulk A' }).locator('.crm-pick').check();
  await rows.filter({ hasText: 'Bulk B' }).locator('.crm-pick').check();
  await page.locator('.crm-bulk .btn.primary', { hasText: 'Approve 2' }).click();
  await page.locator('#modal-btns').getByRole('button', { name: 'Approve 2' }).click();
  await expect(rows).toHaveCount(1);
  await rows.first().locator('.crm-pick').check();
  await page.locator('.crm-bulk .btn', { hasText: 'Decline 1' }).click();
  await page.locator('#modal-prompt-input').fill('off-target ' + R);
  await page.locator('#modal-prompt-input').press('Enter');
  await expect(rows).toHaveCount(0);
  const all = (await api(page, 'GET', '/touches?limit=1000')).body.filter((t: any) => ids.includes(t.id));
  expect(all.map((t: any) => t.status).sort()).toEqual(['approved', 'approved', 'skipped']);
  expect(all.find((t: any) => t.status === 'skipped').result).toBe('declined by the owner: off-target ' + R);
});
