// Ethan, 2026-10-18: "On the plus button make it so that there is an easy way
// to have users describe what they want to build ... a single input box ...
// below there are settings or constraints ... an accordion ... not expanded by
// default ... it spins up the worker ... flags to the user when it's finished
// and it should keep going until it's finished, maybe via a /goal."
// Ethan, 2026-10-29: it named a CRM worker `need-worker-behave-crm`, started it
// in /home/amux, and "it shouldn't create a goal". The server now plans the
// name, directory and groups from the text and attachments; no /goal.
import { test, expect } from './fixtures';

test('Describe what you want starts a planned worker from one box, with remembered defaults and no /goal', async ({ page }) => {
  test.setTimeout(90_000);
  const creates: any[] = [];
  const prefs: any[] = [];
  let reply: 'refuse' | 'ok' | 'down' = 'refuse';
  const plans: any[] = [];
  let planReply: any = { name: 'dark-mode-toggle', dir: '/srv/app', groups: ['ui'], desc: 'Ships a dark mode toggle in settings.', attachments: [] };
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.removeItem('amux_outcome_draft');
    localStorage.removeItem('amux_outcome_defaults');
  });
  // No real worker: the create is answered here, and nothing else is written.
  await page.route('**/api/sessions', async route => {
    if (route.request().method() !== 'POST') return route.fallback();
    creates.push(route.request().postDataJSON());
    if (reply === 'refuse') { reply = 'ok'; return route.fulfill({ status: 400, contentType: 'application/json', body: '{"error":"bad dir"}' }); }
    // A 5xx is queued by the outbox for replay, so it answers the page with a local 202.
    if (reply === 'down') return route.fulfill({ status: 503, contentType: 'application/json', body: '{"error":"down"}' });
    return route.fulfill({ status: 201, contentType: 'application/json', body: '{"ok":true}' });
  });
  await page.route('**/api/outcome/plan', async route => {
    plans.push(route.request().postDataJSON());
    if (!planReply) return route.fulfill({ status: 500, contentType: 'application/json', body: '{"error":"no helper"}' });
    return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ measured: true, n_considered: 3, ...planReply }) });
  });
  await page.route('**/api/prefs**', async route => {
    if (route.request().method() === 'POST') {
      const b = route.request().postDataJSON();
      if (b.key === 'outcome_defaults') { prefs.push(b); return route.fulfill({ status: 200, body: '{"ok":true}' }); }
      return route.fallback();
    }
    if (route.request().url().includes('key=outcome_defaults')) return route.fulfill({ status: 200, contentType: 'application/json', body: '{"value":""}' });
    return route.fallback();
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openOutcome === 'function');

  const open = async () => {
    await page.locator('#add-btn').click();
    await page.locator('#add-menu .card-menu-item', { hasText: 'Describe what you want' }).click();
    await expect(page.locator('#outcome-text')).toBeVisible();
  };
  await open();
  await expect(page.locator('#outcome-text')).toBeFocused();
  // The settings are there but closed.
  await expect(page.locator('.outcome-settings')).not.toHaveAttribute('open', '');
  await expect(page.locator('#outcome-constraints')).toBeHidden();
  await page.locator('#outcome-text').fill('Add a dark mode toggle to settings and ship it');
  await page.locator('.outcome-settings > summary').click();
  await expect(page.locator('#outcome-constraints')).toBeVisible();
  await expect(page.locator('#outcome-constraints')).toHaveValue(/Do not spend money/);
  await expect(page.locator('#outcome-goal')).toHaveCount(0);
  await expect(page.locator('#outcome-dir')).toHaveValue('');
  await page.locator('#outcome-group').fill('ui-work');
  await page.locator('#outcome-dir').fill('/tmp/outcome-e2e');

  // Layout: fits the screen, touch targets on phones.
  const fit = await page.evaluate(() => {
    const r = document.querySelector('#modal-backdrop .outcome-form')!.getBoundingClientRect();
    const s = document.querySelector('.outcome-settings > summary')!.getBoundingClientRect();
    return { right: r.right, vw: window.innerWidth, summaryH: s.height };
  });
  expect(fit.right).toBeLessThanOrEqual(fit.vw);
  if (fit.vw <= 600) expect(fit.summaryH).toBeGreaterThanOrEqual(44);
  await page.screenshot({ path: test.info().outputPath('describe-outcome.png') });

  // A refused start keeps the description.
  await page.locator('#modal-btns .btn.primary', { hasText: 'Start' }).click();
  await expect.poll(() => creates.length).toBe(1);
  await open();
  await expect(page.locator('#outcome-text')).toHaveValue('Add a dark mode toggle to settings and ship it');
  // The remembered defaults came back too.
  await page.locator('.outcome-settings > summary').click();
  await expect(page.locator('#outcome-group')).toHaveValue('ui-work');
  await page.locator('#outcome-text').press(process.platform === 'darwin' ? 'Meta+Enter' : 'Control+Enter');
  await expect.poll(() => creates.length).toBe(2);

  const body = creates[1];
  // The plan names it; a directory and group typed in the dialog win over the plan's.
  expect(plans[plans.length - 1].text).toBe('Add a dark mode toggle to settings and ship it');
  expect(body.name).toBe('dark-mode-toggle');
  expect(body.desc).toBe('Ships a dark mode toggle in settings.');
  expect(body.dir).toBe('/tmp/outcome-e2e');
  expect(body.tags).toEqual(['ui-work']);
  expect(body.isolated).toBe(true);
  expect(body.prompt.startsWith('Add a dark mode toggle to settings and ship it Constraints: Do not spend money')).toBe(true);
  expect(body.prompt).not.toContain('/goal');
  expect(body.prompt).not.toContain('\n');
  expect(prefs.length).toBeGreaterThanOrEqual(1);
  expect(JSON.parse(prefs[prefs.length - 1].value).group).toBe('ui-work');
  // A started worker clears the kept draft.
  await expect.poll(() => page.evaluate(() => localStorage.getItem('amux_outcome_draft'))).toBeNull();

  // Finished: a /goal turning met fires a notification.
  const fired = await page.evaluate(() => {
    const w = window as any; const seen: string[] = [];
    const orig = w._fireSessionNotif;
    w._fireSessionNotif = (_n: string, title: string) => seen.push(title);
    try {
      const base = { name: 'add-dark-mode-toggle', status: 'idle', running: true };
      w._checkSessionTransitions([{ ...base, goal: { state: 'unmet', condition: 'ship it' } }]);
      w._checkSessionTransitions([{ ...base, goal: { state: 'met', condition: 'ship it' } }]);
      w._checkSessionTransitions([{ ...base, goal: { state: 'met', condition: 'ship it' } }]);
    } finally { w._fireSessionNotif = orig; }
    return seen;
  });
  expect(fired).toEqual(['add-dark-mode-toggle finished its goal']);

  // An attached recording reaches the plan by its uploaded path, the plan's
  // directory and groups are used when the dialog leaves them empty, and
  // the worker is told to read the recording's transcript.
  reply = 'ok';
  planReply = { name: 'gtm-crm', dir: '/home/u/amux-GTM/gtm', groups: ['gtm'], desc: 'Tracks outreach toward the pilot goal.',
    attachments: [{ path: '/up/memo.m4a', kind: 'audio', transcript: '/up/memo.m4a.transcript.txt', words: 1544 }] };
  await page.evaluate(() => localStorage.setItem('amux_outcome_defaults', JSON.stringify({ group: '', dir: '', isolated: true })));
  await page.route('**/api/prefs**', route => route.request().url().includes('key=outcome_defaults')
    ? route.fulfill({ status: 200, contentType: 'application/json', body: '{"value":""}' }) : route.fallback());
  await open();
  await page.locator('#outcome-text').fill('I need a worker to behave like a CRM that tracks my outreach and progress towards goal');
  await page.locator('#outcome-file').setInputFiles({ name: 'memo.m4a', mimeType: 'audio/mp4', buffer: Buffer.from('not really audio') });
  await expect(page.locator('#outcome-file-list')).toContainText('memo.m4a');
  await expect(page.locator('#outcome-file-list')).not.toContainText('uploading');
  await page.locator('#modal-btns .btn.primary', { hasText: 'Start' }).click();
  await expect.poll(() => creates.length).toBe(3);
  const planned = plans[plans.length - 1];
  expect(planned.files).toHaveLength(1);
  expect(planned.files[0]).toMatch(/memo\.m4a$/);
  const gtm = creates[2];
  expect(gtm.name).toBe('gtm-crm');
  expect(gtm.dir).toBe('/home/u/amux-GTM/gtm');
  expect(gtm.tags).toEqual(['gtm']);
  expect(gtm.prompt).toContain('/up/memo.m4a.transcript.txt');
  expect(gtm.prompt).not.toContain('/goal');

  // No plan (helper down): the computed name drops the request's scaffolding.
  planReply = null;
  await open();
  await page.locator('#outcome-text').fill('I need a worker to behave like a CRM that tracks my outreach');
  await page.locator('#modal-btns .btn.primary', { hasText: 'Start' }).click();
  await expect.poll(() => creates.length).toBe(4);
  expect(creates[3].name).toBe('crm-tracks-outreach');

  // Server down: the create is queued for replay, so the draft goes rather
  // than inviting a second press that would start a duplicate worker.
  reply = 'down';
  const before = creates.length;
  await open();
  await page.locator('#outcome-text').fill('Write a weekly summary of open cards');
  await page.locator('#modal-btns .btn.primary', { hasText: 'Start' }).click();
  await expect.poll(() => creates.length).toBeGreaterThan(before);
  await expect.poll(() => page.evaluate(() => localStorage.getItem('amux_outcome_draft'))).toBeNull();
  await expect(page.locator('#toast')).toContainText('starts on its own');
  await expect(page.locator('#toast')).toBeVisible();
});
