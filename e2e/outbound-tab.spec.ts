// The Outbound tab (api/outbound_feed.rs): every message a worker wants to send
// outside, on every channel, waiting for the owner (Ethan, 2026-11-26: "Make a
// dedicated tab in amux for outbound comms approval flow. It should rely on the
// connectors too. Some can be executed via browser if there is no direct api").
//
// REAL PATH. Two browser-typed requests (an X DM and a LinkedIn comment) are
// filed through POST /api/outbound as a worker, exactly as a worker files them.
// The tab lists them from the real feed, the owner approves one and discards
// the other through the visible buttons, and the server's own records are read
// back: the request's state, and that the approval card was decided.
//
// ONE MOCK, DISCLOSED. The email-only controls (Edit, Schedule) need a held
// email. A real held email shows the approvals banner on every page this test
// server serves and broke a neighbouring spec once (email-approval-schedule's
// header), so the email item and its release route are mocked in this page
// only. The server half of email (a draft read in full from its file) is
// covered by outbound_feed's unit tests.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

test('Outbound tab: list every channel, approve one, discard one, edit and schedule an email, one row on a phone', async ({ page, request }, testInfo) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  // Unique per run: projects and repeats share a server, and the server
  // treats the same worker asking the same text again as the same request.
  const run = `${testInfo.repeatEachIndex}-${Math.random().toString(36).slice(2, 8)}`;
  const worker = `ob-e2e-${testInfo.project.name}-${run}`;
  const stamp = `${Date.now()}-${run}`;
  const ids: string[] = [];
  try {
    const file = async (data: Record<string, unknown>) => {
      const r = await request.post('/api/outbound', { headers: { ...auth, 'X-Amux-Session': worker }, data });
      expect(r.status(), await r.text()).toBe(202);
      const id = (await r.json()).id as string;
      ids.push(id);
      return id;
    };
    const xDm = await file({
      channel: 'x', action: 'dm', to: '@rio_longacre', target_url: 'https://x.com/messages/compose',
      text: `Following up on the MVS release ${stamp}`, reason: 'Rio asked when the release ships',
      context: { parent: { author: '@rio_longacre', text: 'When does MVS ship?' } },
    });
    const liComment = await file({
      channel: 'linkedin', action: 'comment', target_url: 'https://www.linkedin.com/feed/update/urn:li:activity:1',
      text: `Congrats on the launch ${stamp}`, reason: 'A design partner launched',
    });

    // A held email beside them, mocked (see the header).
    const emailId = 'apr_0b0b0b0b0b0b0b0b';
    const emailRef = 'email:' + emailId;
    let released: any = null;
    await page.route(/\/api\/outbound\/feed(?:\?.*)?$/, async (route) => {
      const real = await (await route.fetch()).json();
      if (!released) {
        real.items.unshift({
          ref: emailRef, kind: 'email', id: emailId, channel: 'email', state: 'needs', worker, created: Date.now() / 1000 - 3 * 86400,
          text: 'Max, the metadata of all 96 files is below.', action: 'reply all', account: 'ethan@example.com',
          to: 'max@example.org, denise@example.org', subject: 'Re: where each asset is live', reason: 'Max asked before the call',
          parent: { author: 'Max', text: 'Where is each asset live?' }, via: 'api', connector: 'google-gmail', connector_label: 'Gmail',
          decidable: true, editable: true, attachments: [], crm: null, card: null,
        });
        real.counts.needs = (real.counts.needs || 0) + 1;
        real.channels.email = { total: 1, needs: 1, via: 'api', connector: 'google-gmail', connector_label: 'Gmail' };
      }
      await route.fulfill({ json: real });
    });
    await page.route(new RegExp(`/api/email/approve/${emailId}$`), async (route) => {
      released = { body: route.request().postDataJSON(), approver: route.request().headers()['x-amux-approver'] };
      await route.fulfill({ json: { ok: true, scheduled: true, send_at: released.body.send_at, approval_id: emailId } });
    });

    await page.waitForFunction("typeof switchView === 'function' && typeof _obEnter === 'function'");
    const t0 = Date.now();
    await page.evaluate("switchView('outbound')");
    const phone = page.viewportSize()!.width <= 600;
    const row = (ref: string) => page.locator(`#ob-list .ob-row[data-ref="${ref}"]`);
    // A phone shows one message at a time; its back button returns to the list.
    const toList = async () => { if (await page.locator('.ob-back').isVisible()) await page.locator('.ob-back').click(); };
    await expect(row('outbound:' + xDm)).toBeVisible();
    testInfo.annotations.push({ type: 'latency', description: `tab to first row ${Date.now() - t0} ms` });
    await expect(row('outbound:' + liComment)).toBeVisible();
    await expect(row(emailRef)).toBeVisible();
    // Projects and repeats share a server, so other runs' items are listed
    // too: count this run's own through the worker filter.
    const feed = await (await request.get('/api/outbound/feed', { headers: auth })).json();
    expect(feed.measured).toBe(true);
    // All counts every message on record (the feed's n_considered), not the
    // capped list. Read from the page's own feed: other runs file meanwhile.
    const shown = await page.evaluate(`({ n: _obData.n_considered, all: document.querySelector('.ob-view[data-v="all"] .ob-count').textContent })`) as { n: number; all: string };
    expect(Number(shown.all)).toBe(shown.n);
    expect(Number(await page.locator('.ob-view[data-v="needs"] .ob-count').textContent())).toBeGreaterThanOrEqual(3);
    await page.locator('#ob-worker').selectOption(worker);
    await expect(page.locator('#ob-list .ob-row')).toHaveCount(3);
    await expect(page.locator('#ob-list-head .ob-list-n')).toHaveText(phone ? /.*/ : '3 to review');
    await expect(page.locator('#tab-outbound .ob-tab-count')).toBeVisible();
    // Channels name how each goes out: X and LinkedIn through the browser.
    await expect(page.locator('#ob-channels .ob-chip[data-c="x"] .ob-dot-browser')).toHaveCount(1);
    await expect(page.locator('#ob-channels .ob-chip[data-c="linkedin"]')).toBeVisible();

    if (phone) {
      // One row of controls above the list, and the list keeps most of the screen.
      const bar = (await page.locator('.ob-bar').boundingBox())!;
      expect(bar.height, 'the bar is one row').toBeLessThanOrEqual(64);
      expect(await page.locator('.ob-bar').evaluate((e) => getComputedStyle(e).flexWrap)).toBe('nowrap');
      expect(await page.locator('.ob-bar').evaluate((e) => getComputedStyle(e).overflowX)).toBe('auto');
      for (const c of await page.locator('.ob-bar .ob-view, .ob-bar .ob-chip').all()) {
        const b = await c.boundingBox();
        if (!b) continue;
        expect(b.height, 'touch targets').toBeGreaterThanOrEqual(44);
        const mid = b.y + b.height / 2;
        expect(mid).toBeGreaterThanOrEqual(bar.y);
        expect(mid).toBeLessThanOrEqual(bar.y + bar.height);
      }
      const list = (await page.locator('.ob-list-pane').boundingBox())!;
      expect(list.height).toBeGreaterThan(page.viewportSize()!.height * 0.5);
      await expect(page.locator('#ob-detail')).toBeHidden();
      const doc = await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1);
      expect(doc, 'nothing overflows a phone sideways').toBe(true);
    }
    // The approvals banner would repeat the held emails above the tab.
    const banner = page.locator('#email-approvals-banner');
    if (await banner.count()) await expect(banner).toBeHidden();
    await page.screenshot({ path: testInfo.outputPath('outbound-list.png') });

    // Open the X DM: the full text, why, and what it answers.
    await row('outbound:' + xDm).click();
    const detail = page.locator('#ob-detail');
    await expect(detail).toBeVisible();
    await expect(detail.locator('.ob-body')).toHaveText(`Following up on the MVS release ${stamp}`);
    await expect(detail).toContainText('Rio asked when the release ships');
    await expect(detail).toContainText('When does MVS ship?');
    await expect(page.locator('#ob-side')).toContainText('in the amux browser');
    await page.screenshot({ path: testInfo.outputPath('outbound-detail.png') });
    // A browser item has no email-only controls.
    await expect(detail.locator('.ob-schedule')).toHaveCount(0);
    await expect(detail.locator('.ob-edit-btn')).toHaveCount(0);

    // Approve it, double-clicked: one decision, no error.
    const approve = detail.locator('.ob-approve');
    await approve.dblclick();
    await expect(page.locator('#toast, .toast').filter({ hasText: /Approved/ }).first()).toBeVisible();
    await expect.poll(async () => (await (await request.get(`/api/outbound/${xDm}`, { headers: auth })).json()).state).toBe('approved');
    const cards = await (await request.get('/api/approvals/cards', { headers: auth })).json();
    const card = cards.items.find((c: any) => c.approval_ref === 'outbound:' + xDm);
    expect(card?.decision, 'the Needs-you card was decided by the same click').toBe('approved');
    await expect(row('outbound:' + xDm)).toHaveCount(0);
    // The double click decided one message: the next one is still waiting.
    expect((await (await request.get(`/api/outbound/${liComment}`, { headers: auth })).json()).state).toBe('held');
    if (phone) {
      // A phone moves straight to the next waiting message after a decision.
      await expect(detail.locator('.ob-detail-title')).toBeVisible();
      await expect(page.locator('#ob-list')).toBeHidden();
    }

    // Discard the LinkedIn comment with a note.
    await toList();
    await row('outbound:' + liComment).click();
    await detail.locator('.ob-discard').click();
    await page.locator('#modal-prompt-input').fill('Not this week');
    await page.locator('#modal-prompt-input').press('Enter');
    await expect.poll(async () => (await (await request.get(`/api/outbound/${liComment}`, { headers: auth })).json()).state).toBe('rejected');
    await expect(row('outbound:' + liComment)).toHaveCount(0);
    await toList();

    // The Approved view holds the approved one; All shows the declined one too.
    await page.locator('.ob-view[data-v="approved"]').click();
    await expect(row('outbound:' + xDm)).toBeVisible();
    await page.locator('.ob-view[data-v="all"]').click();
    await expect(row('outbound:' + liComment)).toBeVisible();
    await expect(row('outbound:' + liComment).locator('.cx-pill')).toHaveText('declined');

    // A reload keeps the record: it lives on the server.
    await page.reload();
    await page.waitForFunction("typeof _obEnter === 'function'");
    await page.evaluate("switchView('outbound')");
    await page.locator('#ob-worker').selectOption(worker);
    await page.locator('.ob-view[data-v="all"]').click();
    await expect(row('outbound:' + xDm)).toBeVisible();

    // The email: Edit, then Schedule sends the edited text with the approver marker.
    await page.locator('.ob-view[data-v="needs"]').click();
    await toList();
    await row(emailRef).click();
    await expect(detail).toContainText('to everyone on the thread');
    await expect(page.locator('#ob-side')).toContainText(/Gmail/);
    await detail.locator('.ob-edit-btn').click();
    const box = page.locator('#ob-edit-box');
    await expect(box).toBeVisible();
    await box.fill('Max, edited before it goes out.');
    // A poll while editing does not throw the edit away.
    await page.evaluate('_obLoad(true)');
    await expect(box).toHaveValue('Max, edited before it goes out.');
    await detail.locator('.ob-schedule').click();
    await expect(page.locator('#ob-sched-at')).toBeVisible();
    // A past time is refused before anything is sent.
    await page.locator('#ob-sched-at').evaluate((e: HTMLInputElement) => { e.min = ''; e.value = '2020-01-01T09:00'; });
    await page.locator('#ob-sched-row .btn.primary').click();
    await expect(page.locator('#toast, .toast').filter({ hasText: 'Pick a time in the future' }).first()).toBeVisible();
    expect(released).toBeNull();
    await page.locator('#ob-sched-at').evaluate((e: HTMLInputElement) => {
      const d = new Date(Date.now() + 86400_000); const z = (n: number) => String(n).padStart(2, '0');
      e.value = `${d.getFullYear()}-${z(d.getMonth() + 1)}-${z(d.getDate())}T09:00`;
    });
    await page.locator('#ob-sched-row .btn.primary').click();
    await expect.poll(() => released).not.toBeNull();
    expect(released.approver).toBe('dashboard');
    expect(released.body.body).toBe('Max, edited before it goes out.');
    expect(released.body.send_at).toBeGreaterThan(Date.now() / 1000);
    await page.screenshot({ path: testInfo.outputPath('outbound-after.png') });
  } finally {
    for (const id of ids) {
      await cleanup('reject a seeded outbound request', () => request.post(`/api/outbound/${id}/reject`, { headers: auth, data: {} }), testInfo);
    }
  }
});

// The approvals banner and the Outbound tab show the same held emails, so the
// banner steps aside on the tab and offers a way into it from everywhere else.
// The held email is mocked in this page only (see the header).
test('the approvals banner links to the Outbound tab and hides on it', async ({ page }) => {
  const held = {
    id: 'apr_0c0c0c0c0c0c0c0c', session: 'gtm-engine', age_s: 120, expires: false, expires_in_s: null, requests: 1,
    preview: { endpoint: 'send', from: 'info@example.com', to: 'buyer@example.org', subject: 'Pricing follow-up' },
    review: { body: 'Hello from the banner test.', body_complete: true, attachment_count: 0, complete: true },
  };
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); localStorage.removeItem('amuxApprCollapsed'); });
  await page.route(/\/api\/email\/approvals(?:\?.*)?$/, (r) => r.fulfill({ json: { pending: [held], scheduled: [] } }));
  await page.route(/\/api\/grants(?:\?.*)?$/, (r) => r.fulfill({ json: { pending: [] } }));
  await page.goto('/');
  await page.waitForFunction("typeof _approvalsRefresh === 'function' && typeof switchView === 'function'");
  await page.evaluate("switchView('board')");
  await page.evaluate('_approvalsRefresh()');
  const banner = page.locator('#email-approvals-banner');
  await expect(banner).toBeVisible();
  const open = banner.locator('.appr-open-outbound');
  await expect(open).toBeVisible();
  await open.click();
  await expect(page.locator('#tab-outbound')).toHaveClass(/active/);
  await expect(page.locator('#outbound-view')).toBeVisible();
  await expect(banner).toBeHidden();
  // Leaving the tab brings the banner back.
  await page.evaluate("switchView('board')");
  await expect(banner).toBeVisible();
});
