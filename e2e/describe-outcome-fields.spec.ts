// Owner, 2026-11-21: in "Describe what you want", the working directory,
// groups and model fill from lists rather than a blank box. The directory and
// model are a combobox (type, or pick a recent value or a suggestion); groups
// are pills with the known groups in one sideways-scrolling row; Browse lists
// the amux host's folders on any device. Recent values are kept on the server
// (prefs key outcome_history), so every device sees one list, and x removes an
// entry everywhere. The dialog is wider on a desktop and fits a phone.
import { test, expect, Page, BrowserContext, allowUnusedRoute } from './fixtures';

const CATALOG = {
  measured: true, n_considered: 4, why_unmeasured: null,
  models: [
    { id: 'claude-opus-e2e', provider: 'claude', model_type: 'opus', worker_selectable: true },
    { id: 'claude-sonnet-e2e', provider: 'claude', model_type: 'sonnet', worker_selectable: true },
    { id: 'gpt-e2e', provider: 'codex', model_type: 'gpt', worker_selectable: true },
    { id: 'hidden-e2e', provider: 'codex', model_type: 'gpt', worker_selectable: false },
  ],
};
const FLEET = [
  { name: 'e2e-fleet-a', dir: '/srv/fleet-a', provider: 'claude', status: 'idle', running: true, tags: ['fleetgrp'], desc: '', pinned: false, isolated: false },
];
const LS: Record<string, any> = {
  '~': { path: '/home/u', parent: '/home', entries: [
    { name: 'proj', type: 'dir' }, { name: 'notes.txt', type: 'file', size: 3 }, { name: '.cache', type: 'dir' }] },
  '/home/u/proj': { path: '/home/u/proj', parent: '/home/u', entries: [{ name: 'web', type: 'dir' }] },
  '/home/u': { path: '/home/u', parent: '/home', entries: [{ name: 'proj', type: 'dir' }] },
};

// One fake server for both "devices": the prefs it keeps are shared.
async function boot(page: Page, store: Record<string, string>, creates: any[], wrapped = true) {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    for (const k of ['amux_outcome_draft', 'amux_outcome_defaults', 'amux_outcome_presets', 'amux_outcome_history']) localStorage.removeItem(k);
  });
  await page.route('**/api/sessions', async route => {
    if (route.request().method() !== 'POST') return route.fulfill({ json: FLEET });
    creates.push(route.request().postDataJSON());
    return route.fulfill({ status: 201, contentType: 'application/json', body: '{"ok":true}' });
  });
  await page.route('**/api/models', route => route.fulfill({ json: CATALOG }));
  await page.route('**/api/outcome/plan', route => route.fulfill({ status: 500, contentType: 'application/json', body: '{"error":"no helper"}' }));
  await page.route('**/api/intent/plan', route => route.fulfill({ status: 500, contentType: 'application/json', body: '{"error":"no router"}' }));
  if (wrapped) allowUnusedRoute(page, '**/api/intent/plan');   // a start with a directory set skips the router
  await page.route('**/api/autocomplete/dir**', route => {
    const q = new URL(route.request().url()).searchParams.get('q') || '';
    return route.fulfill({ json: ['/srv/app/', '/srv/api/', '/srv/fleet-a/'].filter(d => d.startsWith(q)) });
  });
  await page.route('**/api/ls**', route => {
    const p = new URL(route.request().url()).searchParams.get('path') || '';
    return route.fulfill(LS[p] ? { json: LS[p] } : { status: 404, json: { error: 'no such folder' } });
  });
  await page.route('**/api/prefs**', async route => {
    if (route.request().method() === 'POST') {
      const b = route.request().postDataJSON();
      if (!/^outcome_/.test(b.key)) return route.fallback();
      store[b.key] = b.value;
      return route.fulfill({ status: 200, body: '{"ok":true}' });
    }
    const m = /key=(outcome_[a-z]+)/.exec(route.request().url());
    if (m) return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ value: store[m[1]] ?? null }) });
    return route.fallback();
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openOutcome === 'function');
}
async function open(page: Page) {
  await page.locator('#add-btn').click();
  await page.locator('#add-menu .card-menu-item', { hasText: 'Describe what you want' }).click();
  await expect(page.locator('#outcome-text')).toBeVisible();
  await page.locator('.outcome-settings > summary').click();
  await expect(page.locator('#outcome-dir')).toBeVisible();
}
const start = (page: Page) => page.locator('#modal-btns .btn.primary', { hasText: 'Start' }).click();
const opts = (page: Page, id: string) => page.locator(`#${id}-list .oc-opt .oc-opt-label`);

test('groups, working directory and model fill from shared lists; x removes an entry on every device', async ({ page, browser }) => {
  test.setTimeout(120_000);
  const store: Record<string, string> = {};
  const creates: any[] = [];
  await boot(page, store, creates);
  await open(page);

  // Width: wider than the old 480px on a desktop; the full width of a phone.
  const box = await page.locator('#modal-backdrop .modal-box').boundingBox();
  const vw = page.viewportSize()!.width;
  if (vw > 800) expect(box!.width).toBeGreaterThan(600);
  else { expect(box!.width).toBeGreaterThanOrEqual(vw - 16 - 1); expect(box!.x + box!.width).toBeLessThanOrEqual(vw); }

  // Groups: the fleet's group is offered in the sideways row; tap adds a pill.
  await expect(page.locator('#outcome-group-known .oc-chip-add')).toHaveText(['+ fleetgrp']);
  await page.locator('#outcome-group-known [data-pick="fleetgrp"]').click();
  await expect(page.locator('#outcome-groups .oc-pill > span')).toHaveText(['fleetgrp']);
  await expect(page.locator('#outcome-group-known .oc-chip')).toHaveCount(0);
  // Typed groups, comma-separated, become pills; Backspace on an empty box removes the last.
  await page.locator('#outcome-group').fill('alpha, beta');
  await page.locator('#outcome-group').press(',');
  await expect(page.locator('#outcome-groups .oc-pill > span')).toHaveText(['fleetgrp', 'alpha', 'beta']);
  await page.locator('#outcome-group').press('Backspace');
  await expect(page.locator('#outcome-groups .oc-pill > span')).toHaveText(['fleetgrp', 'alpha']);
  // x on a pill un-picks it, back into the known row.
  await page.locator('#outcome-groups [data-unpick="alpha"]').click();
  await expect(page.locator('#outcome-groups .oc-pill > span')).toHaveText(['fleetgrp']);
  // The row scrolls sideways rather than wrapping (one row on a phone).
  expect(await page.locator('#outcome-group-known').evaluate(e => getComputedStyle(e).flexWrap)).toBe('nowrap');
  // A tap on a known group while text is still typed adds the group; the
  // typed text is kept and becomes a pill when focus leaves.
  await page.locator('#outcome-groups [data-unpick="fleetgrp"]').click();
  await page.locator('#outcome-group').fill('gamma');
  await page.locator('#outcome-group-known [data-pick="fleetgrp"]').click();
  await expect(page.locator('#outcome-groups .oc-pill > span')).toHaveText(['fleetgrp']);
  await expect(page.locator('#outcome-group')).toHaveValue('gamma');

  // Working directory: the fleet's folder is a recent; typed text gets the host's folders.
  await page.locator('#outcome-dir').click();
  await expect(page.locator('#outcome-groups .oc-pill > span')).toHaveText(['fleetgrp', 'gamma']);
  await expect(opts(page, 'outcome-dir')).toHaveText(['/srv/fleet-a']);
  await page.locator('#outcome-dir').fill('/srv/a');
  await expect(opts(page, 'outcome-dir')).toHaveText(['/srv/app', '/srv/api']);
  // The open list is on screen, inside the dialog, even when the field is
  // near the dialog's bottom edge (a phone).
  const inView = await page.locator('#outcome-dir-list').evaluate(l => {
    const r = l.getBoundingClientRect(), m = document.querySelector('#modal-backdrop .modal-box')!.getBoundingClientRect();
    return r.top >= m.top - 1 && r.bottom <= m.bottom + 1;
  });
  expect(inView).toBe(true);
  await page.locator('#outcome-dir').press('ArrowDown');
  await page.locator('#outcome-dir').press('ArrowDown');
  await page.locator('#outcome-dir').press('Enter');
  await expect(page.locator('#outcome-dir')).toHaveValue('/srv/api');
  await expect(page.locator('#outcome-dir-list')).toBeHidden();
  // Browse lists the host's folders (not files, not hidden ones), on any device.
  await page.locator('#outcome-browse-btn').click();
  await expect(page.locator('#outcome-browse')).toBeVisible();
  await page.locator('#outcome-dir').fill('');
  await page.locator('#outcome-browse-btn').click();   // close
  await expect(page.locator('#outcome-browse')).toBeHidden();
  await page.locator('#outcome-browse-btn').click();   // reopen at home (empty box)
  await expect(page.locator('#outcome-browse .fe-row')).toHaveCount(1);
  await expect(page.locator('#outcome-browse .fe-row')).toHaveText(/proj/);
  await page.locator('#outcome-browse .fe-row', { hasText: 'proj' }).click();
  await expect(page.locator('#outcome-browse .oc-browse-crumbs')).toContainText('proj');
  await expect(page.locator('#outcome-browse .fe-row')).toHaveText([/web/]);
  await page.locator('#outcome-browse .oc-browse-use').click();
  await expect(page.locator('#outcome-dir')).toHaveValue('/home/u/proj');
  await expect(page.locator('#outcome-browse')).toBeHidden();

  // Model: the catalog's worker-selectable models for the provider.
  await page.locator('#outcome-model').click();
  await expect(opts(page, 'outcome-model')).toHaveText(['claude-opus-e2e', 'claude-sonnet-e2e']);
  await page.locator('#outcome-model').fill('son');
  await expect(opts(page, 'outcome-model')).toHaveText(['claude-sonnet-e2e']);
  await page.locator('#outcome-model-list .oc-opt').first().click();
  await expect(page.locator('#outcome-model')).toHaveValue('claude-sonnet-e2e');
  // Switching provider clears the model and lists that provider's.
  await page.locator('#outcome-provider').selectOption('codex');
  await expect(page.locator('#outcome-model')).toHaveValue('');
  await page.locator('#outcome-model').click();
  await expect(opts(page, 'outcome-model')).toHaveText(['gpt-e2e']);
  await page.locator('#outcome-model-list .oc-opt').first().click();
  await page.screenshot({ path: test.info().outputPath('fields.png') });

  await page.locator('#outcome-text').fill('Tidy the release notes');
  await start(page);
  await expect.poll(() => creates.length).toBe(1);
  expect(creates[0].dir).toBe('/home/u/proj');
  expect(creates[0].tags).toEqual(['fleetgrp', 'gamma']);
  expect(creates[0].provider).toBe('codex');
  expect(creates[0].model).toBe('gpt-e2e');
  // The start went into the shared history.
  await expect.poll(() => JSON.parse(store.outcome_history || '{}').dirs).toEqual(['/home/u/proj']);
  const h = JSON.parse(store.outcome_history);
  expect(h.groups).toEqual(['fleetgrp', 'gamma']);
  expect(h.models).toEqual({ codex: ['gpt-e2e'] });

  // A second device on the same server sees the same lists.
  const other: BrowserContext = await browser.newContext({ ignoreHTTPSErrors: true, viewport: page.viewportSize()!, serviceWorkers: 'block' });
  const phone = await other.newPage();
  await boot(phone, store, creates, false);   // a second device: a context the fixture does not wrap
  await open(phone);
  await phone.locator('#outcome-dir').click();
  await expect(opts(phone, 'outcome-dir')).toHaveText(['/home/u/proj', '/srv/fleet-a']);
  await expect(phone.locator('#outcome-group-known .oc-chip-add')).toHaveText(['+ fleetgrp', '+ gamma']);
  // x removes a recent folder there...
  await phone.locator('#outcome-dir-list .oc-opt', { hasText: '/home/u/proj' }).locator('.oc-x').click();
  await expect(opts(phone, 'outcome-dir')).toHaveText(['/srv/fleet-a']);
  // ...and x on the fleet's group hides it (it is not in use by name any more here).
  await phone.locator('#outcome-group-known [data-forget-group="fleetgrp"]').click();
  await expect(phone.locator('#outcome-group-known .oc-chip-add')).toHaveText(['+ gamma']);
  await expect.poll(() => JSON.parse(store.outcome_history).hidden.groups).toEqual(['fleetgrp']);
  await other.close();

  // ...and the first device sees both removals on its next open.
  await page.locator('#add-btn').click();
  await page.locator('#add-menu .card-menu-item', { hasText: 'Describe what you want' }).click();
  await page.locator('.outcome-settings > summary').click();
  await page.locator('#outcome-dir').click();
  await expect(opts(page, 'outcome-dir')).toHaveText(['/srv/fleet-a']);
  await expect(page.locator('#outcome-group-known .oc-chip-add')).toHaveText(['+ gamma']);
  // Recent models are per provider, with x. Moving to another field closes
  // the open folder list (it would cover the fields below it).
  await page.locator('#outcome-provider').focus();
  await expect(page.locator('#outcome-dir-list')).toBeHidden();
  await page.locator('#outcome-provider').selectOption('codex');
  await page.locator('#outcome-model').click();
  await expect(page.locator('#outcome-model-list .oc-combo-head')).toHaveText(['Recent']);   // gpt-e2e is the only codex model, and it is recent
  await expect(opts(page, 'outcome-model')).toHaveText(['gpt-e2e']);
  await page.locator('#outcome-model-list .oc-opt .oc-x').first().click();
  await expect.poll(() => JSON.parse(store.outcome_history).models).toEqual({});
});

test('a folder Browse cannot open falls back to home and says why', async ({ page }) => {
  const store: Record<string, string> = {};
  await boot(page, store, []);
  // Nothing is started or typed into a suggesting field here.
  for (const u of ['**/api/outcome/plan', '**/api/autocomplete/dir**']) allowUnusedRoute(page, u);
  await open(page);
  await page.locator('#outcome-dir').fill('/gone');
  await page.locator('#outcome-browse-btn').click();
  await expect(page.locator('#outcome-browse .oc-browse-crumbs')).toContainText('u');
  await expect(page.locator('#outcome-browse .fe-row')).toHaveText([/proj/]);
  // The text box is still free text: a path nobody listed is kept as typed.
  await page.locator('#outcome-browse .oc-browse-btns .btn', { hasText: 'Close' }).click();
  await expect(page.locator('#outcome-dir')).toHaveValue('/gone');
});

test('a preset saved while another device saved one keeps both; a delete keeps the other device\'s', async ({ page }) => {
  const store: Record<string, string> = { outcome_presets: JSON.stringify([{ name: 'X', group: '', dir: '/x', provider: 'claude', model: '', isolated: false, constraints: '' }]) };
  await boot(page, store, []);
  for (const u of ['**/api/outcome/plan', '**/api/autocomplete/dir**', '**/api/ls**']) allowUnusedRoute(page, u);
  await open(page);
  await expect(page.locator('#outcome-preset option')).toHaveText(['None', 'X']);
  // Another device saves Y while this dialog is open.
  store.outcome_presets = JSON.stringify([...JSON.parse(store.outcome_presets), { name: 'Y', group: '', dir: '/y', provider: 'claude', model: '', isolated: false, constraints: '' }]);
  await page.locator('#outcome-dir').fill('/z');
  await page.locator('#outcome-save-preset').check();
  await page.locator('#outcome-preset-name').fill('Z');
  await start(page);   // an empty description: saves the preset, starts nothing
  await expect.poll(() => JSON.parse(store.outcome_presets).map((p: any) => p.name)).toEqual(['X', 'Y', 'Z']);
  // Another device deletes X; deleting Y here does not bring X back.
  store.outcome_presets = JSON.stringify(JSON.parse(store.outcome_presets).filter((p: any) => p.name !== 'X'));
  await open(page);
  await page.locator('#outcome-preset').selectOption('Y');
  await page.locator('#outcome-preset-delete').click();
  await page.locator('#outcome-preset-delete').click();
  await expect.poll(() => JSON.parse(store.outcome_presets).map((p: any) => p.name)).toEqual(['Z']);
});

// Owner, 2026-11-22: on a phone the pills were 44px tall, the settings sat in
// a second box, and the folder list was short and hard to reach. Desktop keeps
// its look.
test('on a phone the pills are small, the settings lose their box, and an open list fills the space below its field', async ({ page }) => {
  const store: Record<string, string> = {};
  await boot(page, store, []);
  for (const u of ['**/api/outcome/plan', '**/api/autocomplete/dir**', '**/api/ls**']) allowUnusedRoute(page, u);
  await open(page);
  await page.locator('#outcome-group').fill('alpha');
  await page.locator('#outcome-group').press(',');
  const phone = page.viewportSize()!.width <= 600;
  const m = await page.evaluate(() => {
    const h = (sel: string) => document.querySelector(sel)!.getBoundingClientRect().height;
    // Where a tap 8px outside a chip's x lands: the x itself on a phone.
    const x = document.querySelector('#outcome-group-known .oc-x')!.getBoundingClientRect();
    const hit = document.elementFromPoint(x.left + x.width / 2, x.top - 8);
    return { chip: h('#outcome-group-known .oc-chip'), pill: h('#outcome-groups .oc-pill'),
      hitX: !!hit?.closest('#outcome-group-known .oc-x'),
      settingsBorder: getComputedStyle(document.querySelector('.outcome-settings')!).borderLeftWidth };
  });
  if (phone) {
    expect(m.chip).toBeLessThanOrEqual(32); expect(m.pill).toBeLessThanOrEqual(32);
    expect(m.hitX).toBe(true);
    expect(m.settingsBorder).toBe('0px');
  } else {
    expect(m.chip).toBeLessThanOrEqual(32); expect(m.settingsBorder).toBe('1px');
  }
  // Open the folder list: on a phone its field moves to the dialog's top and
  // the list, in the flow, is taller than the desktop's 360px cap allows on a
  // 667px screen minus the field; on a desktop it floats, capped at 360px.
  await page.locator('#outcome-dir').click();
  await expect(page.locator('#outcome-dir-list')).toBeVisible();
  await page.waitForTimeout(100);
  const l = await page.evaluate(() => {
    const list = document.querySelector('#outcome-dir-list') as HTMLElement, box = document.querySelector('#modal-backdrop .modal-box')!;
    const f = document.querySelector('#outcome-dir')!.closest('.outcome-field')!.getBoundingClientRect();
    return { pos: getComputedStyle(list).position, maxH: parseFloat(getComputedStyle(list).maxHeight),
      fieldFromTop: f.top - box.getBoundingClientRect().top, vh: innerHeight,
      scrolled: box.scrollTop, atEnd: box.scrollTop >= box.scrollHeight - box.clientHeight - 1 };
  });
  if (phone) {
    expect(l.pos).toBe('static');
    // Scrolled toward the field: to the top, or as far as the dialog goes.
    expect(l.scrolled).toBeGreaterThan(0);
    expect(l.fieldFromTop < 30 || l.atEnd).toBe(true);
    expect(l.maxH).toBeGreaterThan(l.vh - 200);
  } else {
    expect(l.pos).toBe('absolute'); expect(l.maxH).toBe(360);
  }
});
