// The Vault page end to end in a real browser against the real server
// (AMUX-5375): add a secret at every scope, search and filter, edit, reveal
// (audited, masked again on a timer), rotate, see who has access, read the
// audit log, delete behind a confirmation, and fit a 375px phone. Every value
// is a synthetic canary; the page must never show one it was not asked to.
import { test, expect, Page } from './fixtures';
import { cleanup } from './teardown';
import { boot } from './lifecycle/evidence';

const VALUES = {
  global: 'vp-canary-global-4b7e21',
  group: 'vp-canary-group-90ad3f',
  worker: 'vp-canary-worker-1c66e8',
  rotated: 'vp-canary-rotated-ee0412',
};

async function token(page: Page): Promise<string> {
  const t = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  expect(t, 'served bootstrap must provide the API token').toBeTruthy();
  return t;
}

async function addSecret(page: Page, key: string, scope: 'global' | 'group' | 'worker', target: string, value: string, note: string) {
  await page.locator('#vault-add-btn').click();
  const dialog = page.locator('#vault-edit');
  await expect(dialog.getByRole('heading', { name: 'Add Secret' })).toBeVisible();
  await page.fill('#vault-e-key', key);
  await page.fill('#vault-e-note', note);
  await expect(page.locator('#vault-e-note-count')).toHaveText(`${note.length}/200`);
  await page.check(`input[name="vault-e-scope"][value="${scope}"]`);
  if (scope === 'group') {
    // Groups are checkboxes: exactly the comma-separated targets end up checked.
    const want = target.split(',');
    for (const box of await page.locator('#vault-e-group input[type=checkbox]').all()) {
      const v = await box.getAttribute('value');
      if (want.includes(v || '')) await box.check(); else await box.uncheck();
    }
  }
  if (scope === 'worker') await page.selectOption('#vault-e-worker', target);
  await page.fill('#vault-e-value', value);
  await page.locator('#vault-e-save').click();
  await expect(dialog).toHaveCount(0);
  await expect(page.locator('.vault-row', { hasText: key })).toHaveCount(1);
}

async function rowAction(page: Page, key: string, action: string) {
  const row = page.locator('.vault-row', { hasText: key });
  await row.locator('.vault-more').click();
  await page.getByRole('menuitem', { name: action }).click();
}

test('Vault page: the full secret lifecycle without leaking a value', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  const suffix = info.project.name.replace(/[^a-z0-9]/gi, '').toLowerCase();
  const wa = `vpa-${suffix}`, wb = `vpb-${suffix}`;
  await boot(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  const created: string[] = [];
  const removeFixtures = async () => {
    const list = await (await request.get('/api/vault/secrets', { headers: auth })).json();
    for (const i of list.items || []) if (/^VP_/.test(i.key)) await request.delete(`/api/vault/secrets/${i.id}`, { headers: auth });
    for (const n of created) await request.delete(`/api/sessions/${n}`, { headers: auth });
  };
  try {
    for (const [name, tags] of [[wa, ['research']], [wb, ['devops']]] as const) {
      const r = await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp', tags, start: false } });
      expect(r.status(), await r.text()).toBeLessThan(300);
      created.push(name);
    }
    await page.reload();
    await boot(page);

    // Reached the way an owner would: the Vault tab in the main tab bar.
    await page.locator('#tab-vault').click();
    // A tab view like the others, not a dialog over the page.
    const vault = page.locator('#vault-view');
    await expect(vault).toBeVisible();
    await expect(page.locator('#tab-vault')).toHaveClass(/active/);
    await expect(page.locator('#vault-view [role=dialog], #vault-view .modal')).toHaveCount(0);
    await expect(vault.getByRole('heading', { name: 'Vault' })).toBeVisible();
    await expect(vault.getByRole('tab', { name: 'Secrets' })).toHaveAttribute('aria-selected', 'true');
    await expect(vault.locator('.vault-banner')).toContainText('Secure by design');
    await expect(vault.locator('#vault-secrets-table .vault-empty')).toContainText('No secrets yet');

    // Name validation: typing normalises to an env name; harness names refused.
    await page.locator('#vault-add-btn').click();
    await page.fill('#vault-e-key', 'my-key');
    await expect(page.locator('#vault-e-key')).toHaveValue('MY_KEY');
    await page.fill('#vault-e-key', 'AMUX_URL');
    await page.locator('#vault-e-key').dispatchEvent('input');
    await expect(page.locator('#vault-e-key-err')).toContainText('harness or shell variable');
    await page.locator('#vault-edit').getByRole('button', { name: 'Cancel' }).click();
    await expect(page.locator('#vault-edit')).toHaveCount(0);

    await addSecret(page, `VP_GLOBAL_${suffix.toUpperCase()}`, 'global', '', VALUES.global, 'Shared by everyone');
    await addSecret(page, `VP_GROUP_${suffix.toUpperCase()}`, 'group', 'research', VALUES.group, 'Research only');
    await addSecret(page, `VP_WORKER_${suffix.toUpperCase()}`, 'worker', wb, VALUES.worker, 'One worker');
    const G = `VP_GLOBAL_${suffix.toUpperCase()}`, R = `VP_GROUP_${suffix.toUpperCase()}`, K = `VP_WORKER_${suffix.toUpperCase()}`;
    await expect(page.locator('.vault-row', { hasText: G })).toContainText('All workers');
    await expect(page.locator('.vault-row', { hasText: R })).toContainText('research');
    await expect(page.locator('.vault-row', { hasText: K })).toContainText(wb);

    // One secret, two groups: it reaches a worker in either one, and the row
    // names both (Ethan, 2026-10-17: "make sure we can add multiple groups").
    const M = `VP_MULTI_${suffix.toUpperCase()}`;
    await page.locator('#vault-add-btn').click();
    await page.fill('#vault-e-key', M);
    await page.check('input[name="vault-e-scope"][value="group"]');
    for (const box of await page.locator('#vault-e-group input[type=checkbox]').all()) await box.uncheck();
    await expect(page.locator('#vault-e-group-chips')).toHaveText('No groups selected');
    // Search narrows the list; Enter picks the first match and clears the box.
    const q = page.locator('#vault-e-group-q');
    await q.fill('resea');
    await expect(page.locator('#vault-e-group-list .vault-check:visible')).toHaveCount(1);
    await q.press('Enter');
    await expect(q).toHaveValue('');
    await expect(page.locator('#vault-e-group-list input[value="research"]')).toBeChecked();
    await q.fill('zz-no-such-group');
    await expect(page.locator('#vault-e-group-none')).toBeVisible();
    await q.fill('devo');
    await q.press('Enter');
    await expect(page.locator('#vault-e-group-chips .vault-ms-name')).toHaveText(['devops', 'research']);
    // A chip's x removes that group, and only that group.
    await page.getByRole('button', { name: 'Remove research' }).click();
    await expect(page.locator('#vault-e-group-list input[value="research"]')).not.toBeChecked();
    await expect(page.locator('#vault-e-group-chips .vault-ms-name')).toHaveText(['devops']);
    await q.fill('research');
    await q.press('Enter');
    await expect(page.locator('#vault-e-reach')).toContainText(wa);
    await expect(page.locator('#vault-e-reach')).toContainText(wb);
    await page.fill('#vault-e-value', VALUES.group);
    await page.locator('#vault-e-save').click();
    await expect(page.locator('#vault-edit')).toHaveCount(0);
    await expect(page.locator('.vault-row', { hasText: M })).toContainText('devops, research');
    const multi = (await (await request.get('/api/vault/secrets', { headers: auth })).json()).items.find((i: any) => i.key === M);
    expect(multi.scope).toBe('group');
    expect(multi.target).toBe('devops,research');

    // Search and filters.
    await page.fill('#vault-q', 'research only');
    await expect(page.locator('.vault-row')).toHaveCount(1);
    await page.fill('#vault-q', '');
    await page.selectOption('#vault-f-scope', 'worker');
    await expect(page.locator('.vault-row')).toHaveCount(1);
    await page.selectOption('#vault-f-scope', '');
    await page.selectOption('#vault-f-group', 'research');
    await expect(page.locator('.vault-row')).toHaveCount(2);
    await page.selectOption('#vault-f-group', '');
    // The per-worker view: what wa receives and from where.
    await page.selectOption('#vault-f-worker', wa);
    await expect(page.locator('.vault-worker-card')).toContainText(`${wa} receives 3 secrets`);
    await expect(page.locator('.vault-row')).toHaveCount(3);
    await expect(page.locator('.vault-worker-card')).toContainText('At next start');
    await page.selectOption('#vault-f-worker', '');
    await page.selectOption('#vault-sort', 'name-desc');
    await expect(page.locator('.vault-row').first()).toContainText(K);
    await page.selectOption('#vault-sort', 'name');

    // Edit: description, and move the group secret to one worker.
    await rowAction(page, R, 'Edit');
    await expect(page.locator('#vault-edit').getByRole('heading', { name: 'Edit Secret' })).toBeVisible();
    await expect(page.locator('#vault-e-value'), 'edit never shows the stored value').toHaveValue('');
    await page.fill('#vault-e-note', 'Moved to one worker');
    await page.check('input[name="vault-e-scope"][value="worker"]');
    await page.selectOption('#vault-e-worker', wa);
    await expect(page.locator('#vault-e-reach')).toContainText(`Reaches 1 worker: ${wa}`);
    await page.locator('#vault-e-save').click();
    await expect(page.locator('#vault-edit')).toHaveCount(0);
    await expect(page.locator('.vault-row', { hasText: R })).toContainText('Moved to one worker');
    await expect(page.locator('.vault-row', { hasText: R }).locator('.vault-chip')).toHaveText('Worker');

    // Reveal: shown on request, masked again by the timer.
    await page.clock.install();
    const gRow = page.locator('.vault-row', { hasText: G });
    await gRow.getByRole('button', { name: `Reveal ${G}` }).click();
    await expect(gRow.locator('.vault-mask')).toHaveText(VALUES.global);
    await page.clock.fastForward(21_000);
    await expect(gRow.locator('.vault-mask')).not.toContainText(VALUES.global);
    await expect(gRow.getByRole('button', { name: `Reveal ${G}` })).toBeVisible();

    // Rotate: a new value, version 2.
    await rowAction(page, K, 'Rotate value');
    await page.fill('#vault-e-value', VALUES.rotated);
    await page.locator('#vault-e-eye').click();
    await expect(page.locator('#vault-e-value')).toHaveAttribute('type', 'text');
    await page.locator('#vault-e-save').click();
    await expect(page.locator('#vault-edit')).toHaveCount(0);
    await expect(page.locator('.vault-row', { hasText: K })).toContainText('v2');

    // Who has access.
    await rowAction(page, G, 'Who has access');
    const access = page.locator('#vault-access');
    await expect(access).toContainText(wa);
    await expect(access).toContainText(wb);
    await expect(access).toContainText('worker > group > global');
    await access.getByRole('button', { name: 'Close' }).last().click();

    // Delete behind a confirmation: Cancel keeps it, Delete removes it.
    await rowAction(page, K, 'Delete');
    await expect(page.locator('#modal-msg')).toContainText('cannot be undone');
    await page.locator('#modal-btns').getByRole('button', { name: 'Cancel' }).click();
    await expect(page.locator('.vault-row', { hasText: K })).toHaveCount(1);
    await rowAction(page, K, 'Delete');
    await page.locator('#modal-btns').getByRole('button', { name: 'Delete' }).click();
    await expect(page.locator('.vault-row', { hasText: K })).toHaveCount(0);

    // Audit log: each action, no value.
    await page.locator('#vault-audit-btn').click();
    const audit = page.locator('#vault-audit');
    for (const ev of ['Added', 'Edited', 'Revealed', 'Rotated', 'Deleted']) await expect(audit).toContainText(ev);
    const auditText = await audit.innerText();
    for (const v of Object.values(VALUES)) expect(auditText).not.toContain(v);
    await info.attach(`vault-audit-${info.project.name}`, { body: await page.screenshot(), contentType: 'image/png' });
    await audit.getByRole('button', { name: 'Close' }).last().click();

    // Nothing on the page holds a value now.
    const html = await page.content();
    for (const v of Object.values(VALUES)) expect(html, 'a value stayed in the DOM').not.toContain(v);

    // Layout: no horizontal overflow, phone-size controls, both themes.
    const fit = await page.evaluate(() => {
      const body = document.querySelector('#vault-view .vault-body') as HTMLElement;
      const pageEl = document.querySelector('#vault-view .vault-page') as HTMLElement;
      const r = pageEl.getBoundingClientRect();
      return { over: Math.max(body.scrollWidth - body.clientWidth, document.documentElement.scrollWidth - innerWidth), left: r.left, right: r.right, vw: innerWidth, check: (window as any)._uiComponentCheck(document.getElementById('vault-view')) };
    });
    expect(fit.over, 'no horizontal scroll in the vault body').toBeLessThanOrEqual(1);
    expect(fit.left).toBeGreaterThanOrEqual(0);
    expect(fit.right).toBeLessThanOrEqual(fit.vw);
    expect(fit.check.measured === undefined || fit.check.n_considered > 0).toBeTruthy();
    expect(fit.check.issues || [], 'shared-control issues on the vault page').toEqual([]);
    await info.attach(`vault-${info.project.name}-dark`, { body: await page.screenshot(), contentType: 'image/png' });
    await page.evaluate(() => document.body.classList.add('light'));
    await info.attach(`vault-${info.project.name}-light`, { body: await page.screenshot(), contentType: 'image/png' });
    await page.evaluate(() => document.body.classList.remove('light'));

    // Payment Methods is the existing card store, not a new one.
    await vault.getByRole('tab', { name: 'Payment Methods' }).click();
    await expect(page.locator('#vault-panel-payments')).toBeVisible();
    await expect(page.locator('#vault-items')).not.toHaveText('Loading…');

    // API Keys moved here from Settings (b15e61d9): the tab borrows the
    // settings section, returns it when you leave Vault, and can borrow it
    // again on the next visit.
    for (let pass = 0; pass < 2; pass++) {
      if (pass) await page.locator('#tab-vault').click();
      await vault.getByRole('tab', { name: 'API Keys' }).click();
      await expect(page.locator('#vault-panel-apikeys #settings-anthropic-key')).toBeVisible();
      await page.evaluate(() => (window as any).switchView('sessions'));
      await expect(vault).toBeHidden();
      await expect(page.locator('#settings-moved-out #settings-apikeys-section')).toHaveCount(1);
    }
    // The tab is remembered across a reload like every other view.
    await page.locator('#tab-vault').click();
    await page.reload();
    await expect(page.locator('#vault-view')).toBeVisible();
    await expect(page.locator('#tab-vault')).toHaveClass(/active/);
  } finally {
    await cleanup('remove vault fixtures', removeFixtures, info);
  }
});

// Every Vault row shows its id and copies it on click (Ethan, 2026-10-16: "in
// vault list make sure each has an id I can copy"). Both lists: a secret
// (sec_…) and a payment method (vlt_…). The clipboard is captured in the page,
// so the check is the same on every browser project, and it re-checks after a
// re-render because the rows are rebuilt from a template.
// Same file as the lifecycle test, so it runs after it in one worker: that test
// asserts an empty vault first, and a parallel file's secret would break it.
test('Vault: each secret and payment method shows a copyable id', async ({ page, request }, info) => {
  test.setTimeout(90_000);
  const suffix = info.project.name.replace(/[^a-z0-9]/gi, '').toUpperCase();
  const key = `VID_${suffix}`;
  await page.addInitScript(() => {
    (window as any).__copied = [];
    const w = (t: string) => { (window as any).__copied.push(t); return Promise.resolve(); };
    try { Object.defineProperty(navigator, 'clipboard', { value: { writeText: w }, configurable: true }); } catch {}
  });
  await boot(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  let secretId = '', itemId = '';
  const removeIds = async () => {
    if (secretId) await request.delete(`/api/vault/secrets/${secretId}`, { headers: auth });
    if (itemId) await request.delete(`/api/vault/${itemId}`, { headers: auth });
  };
  try {
    const s = await request.post('/api/vault/secrets', { headers: auth, data: { key, value: 'vid-canary-7f31', scope: 'global' } });
    expect(s.status(), await s.text()).toBeLessThan(300);
    secretId = ((await s.json()).item || {}).id || (await s.json()).id;
    expect(secretId).toMatch(/^sec_/);
    const v = await request.post('/api/vault', { headers: auth, data: { name: `Card ${suffix}`, fields: { number: '4242424242424242', exp: '12/30', name: 'Test' } } });
    expect(v.status(), await v.text()).toBeLessThan(300);
    const vj = await v.json();
    itemId = vj.id || (vj.item || {}).id;
    expect(itemId).toMatch(/^vlt_/);

    await page.reload();
    await boot(page);
    await page.locator('#tab-vault').click();

    const secretBadge = page.locator('.vault-row', { hasText: key }).locator('.vault-id');
    await expect(secretBadge).toHaveText(secretId);
    const box = await secretBadge.boundingBox();
    if (info.project.name !== 'desktop') expect(box!.height, 'touch target on a phone').toBeGreaterThanOrEqual(44);
    await secretBadge.click();
    await expect(page.locator('.vault-menu'), 'copying must not open the row menu').toHaveCount(0);
    await expect.poll(() => page.evaluate(() => (window as any).__copied.slice(-1)[0])).toBe(secretId);
    await expect(page.getByText(`${secretId} copied`).first()).toBeVisible();

    // A re-render rebuilds the rows; the id must survive it.
    await page.evaluate(() => (window as any)._vaultRender());
    await expect(page.locator('.vault-row', { hasText: key }).locator('.vault-id')).toHaveText(secretId);

    await page.locator('#vault-tab-payments').click();
    const itemBadge = page.locator(`.vault-item[data-id="${itemId}"] .vault-id`);
    await expect(itemBadge).toHaveText(itemId);
    await itemBadge.click();
    await expect.poll(() => page.evaluate(() => (window as any).__copied.slice(-1)[0])).toBe(itemId);

    // Fits a phone: the badge stays inside the viewport.
    const vw = page.viewportSize()!.width;
    const ib = await itemBadge.boundingBox();
    expect(ib!.x + ib!.width).toBeLessThanOrEqual(vw);
    await page.screenshot({ path: info.outputPath('vault-ids.png') });
  } finally {
    await cleanup('remove vault id fixtures', removeIds, info);
  }
});

// Payment methods with spending limits, and API keys, both added through the
// UI (Ethan, 2026-10-18: "payment methods i should be able to add new ones,
// and each one have a spending limit (monthly, daily, yearly) ... api keys i
// should able to create api keys (key value pair)"). The limits are then
// checked where money moves, /api/browser/complete-order, and the API key is
// checked against the same scope rules every secret obeys. Same file as the
// lifecycle test so it never runs beside it; it removes what it adds.
test('Vault: add a payment method with limits and an API key from the UI', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  const suffix = info.project.name.replace(/[^a-z0-9]/gi, '').toLowerCase();
  const inW = `vlk-in-${suffix}`, outW = `vlk-out-${suffix}`, group = `vlk${suffix}`;
  const keyName = `VPK_${suffix.toUpperCase()}`, keyValue = `vpk-canary-${suffix}-5d1e`;
  const cardName = `Limits card ${suffix}`, number = '4242424242424242';
  await boot(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  const sessions: string[] = [];
  let cardId = '', secretId = '';
  const removeFixtures = async () => {
    if (secretId) await request.delete(`/api/vault/secrets/${secretId}`, { headers: auth });
    if (cardId) await request.delete(`/api/vault/${cardId}`, { headers: auth });
    for (const n of sessions) await request.delete(`/api/sessions/${n}`, { headers: auth });
  };
  try {
    for (const [name, tags] of [[inW, [group]], [outW, ['vlkother']]] as const) {
      const r = await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp', tags, start: false } });
      expect(r.status(), await r.text()).toBeLessThan(300);
      sessions.push(name);
    }
    await page.reload();
    await boot(page);
    await page.locator('#tab-vault').click();
    const vault = page.locator('#vault-view');

    // 1. Add a card with all three limits.
    await vault.getByRole('tab', { name: 'Payment Methods' }).click();
    await page.locator('#vault-card-add-btn').click();
    const dialog = page.locator('#vault-card');
    await expect(dialog.getByRole('heading', { name: 'Add Payment Method' })).toBeVisible();
    await page.fill('#vault-c-name', cardName);
    await page.fill('#vault-c-number', number);
    await page.fill('#vault-c-exp', '12/30');
    await page.fill('#vault-c-cvc', '123');
    await page.fill('#vault-c-zip', '10001');
    await page.fill('#vault-c-approval', '100');
    await page.fill('#vault-c-daily', '25');
    await page.fill('#vault-c-monthly', '300');
    await page.fill('#vault-c-yearly', '2000');
    const fit = await page.evaluate(() => {
      const m = document.querySelector('#vault-card .modal') as HTMLElement;
      return { over: m.scrollWidth - m.clientWidth, right: m.getBoundingClientRect().right, vw: innerWidth };
    });
    expect(fit.over, 'the add dialog has no horizontal scroll').toBeLessThanOrEqual(1);
    expect(fit.right).toBeLessThanOrEqual(fit.vw);
    await page.locator('#vault-c-save').click();
    await expect(dialog).toHaveCount(0);
    const row = page.locator('.vault-item', { hasText: cardName });
    await expect(row).toHaveCount(1);
    await expect(row).toContainText('ending 4242');
    cardId = (await row.getAttribute('data-id')) || '';
    expect(cardId).toMatch(/^vlt_/);
    await expect(row.locator('.vault-spend-limit[data-window="daily"]')).toHaveValue('25');
    await expect(row.locator('.vault-spent[data-window="monthly"]')).toContainText('of $300');
    expect(await page.content(), 'the card number stayed in the page').not.toContain(number);

    let list = await (await request.get('/api/vault', { headers: auth })).json();
    let item = (list.items || []).find((i: any) => i.id === cardId);
    expect(item.rules.spend_limits_usd).toEqual({ daily: 25, monthly: 300, yearly: 2000 });
    expect(JSON.stringify(list)).not.toContain(number);

    // 2. The limit holds where money moves: over the daily limit is refused
    // before anything is filled; under it gets past the limit check.
    const order = (amount: number) => request.post('/api/browser/complete-order', { headers: { ...auth, 'X-Amux-Session': inW },
      data: { amount_usd: amount, currency: 'USD', merchant: 'E2E Shop', purpose: 'limit e2e', session: inW, item_name: cardName.toLowerCase() } });
    const over = await order(30);
    expect(over.status()).toBe(403);
    const ob = await over.json();
    expect(ob.limit_exceeded, JSON.stringify(ob)).toBe(true);
    expect(ob.window).toBe('daily');
    const under = await (await order(20)).json();
    expect(under.limit_exceeded, JSON.stringify(under)).toBeUndefined();

    // 3. Raise the daily limit in the UI.
    await row.locator('.vault-spend-limit[data-window="daily"]').fill('40');
    await row.locator('.vault-spend-limit[data-window="yearly"]').fill('');
    await row.locator('.vault-save-limits').click();
    await expect.poll(async () => {
      const l = await (await request.get('/api/vault', { headers: auth })).json();
      return (l.items || []).find((i: any) => i.id === cardId)?.rules?.spend_limits_usd;
    }).toEqual({ daily: 40, monthly: 300 });
    expect((await (await order(30)).json()).limit_exceeded, 'after the raise $30 fits').toBeUndefined();

    // 4. Add an API key for one group from the API Keys tab.
    await vault.getByRole('tab', { name: 'API Keys' }).click();
    await expect(page.locator('#vault-panel-apikeys #settings-anthropic-key'), 'the existing provider keys stay').toBeVisible();
    await page.locator('#vault-apikey-add-btn').click();
    const kd = page.locator('#vault-edit');
    await expect(kd.getByRole('heading', { name: 'Add API Key' })).toBeVisible();
    await page.fill('#vault-e-key', keyName);
    await page.check('input[name="vault-e-scope"][value="group"]');
    for (const box of await page.locator('#vault-e-group input[type=checkbox]').all()) {
      if ((await box.getAttribute('value')) === group) await box.check(); else await box.uncheck();
    }
    await page.fill('#vault-e-value', keyValue);
    await page.locator('#vault-e-save').click();
    await expect(kd).toHaveCount(0);
    const keyRow = page.locator('#vault-apikeys-table .vault-row', { hasText: keyName });
    await expect(keyRow).toHaveCount(1);
    await expect(keyRow.locator('.vault-chip-group')).toBeVisible();
    expect(await page.content(), 'the API key value stayed in the page').not.toContain(keyValue);
    secretId = (await keyRow.getAttribute('data-id')) || '';
    expect(secretId).toMatch(/^sec_/);

    // 5. Same scope rules as any secret: the worker outside the group does not get it.
    const all = await (await request.get('/api/vault/secrets', { headers: auth })).json();
    const sec = (all.items || []).find((i: any) => i.id === secretId);
    expect(sec.kind).toBe('api_key');
    expect(sec.scope).toBe('group');
    expect(sec.target).toBe(group);
    const resolvedFor = async (w: string) => ((await (await request.get(`/api/vault/secrets?worker=${w}`, { headers: auth })).json()).resolved || []).map((r: any) => r.key);
    expect(await resolvedFor(inW)).toContain(keyName);
    expect(await resolvedFor(outW)).not.toContain(keyName);
    const asOut = await (await request.get('/api/vault/secrets', { headers: { ...auth, 'X-Amux-Session': outW } })).json();
    expect((asOut.items || []).map((i: any) => i.key)).not.toContain(keyName);
    // The Secrets tab lists it too, tagged as an API key.
    await vault.getByRole('tab', { name: 'Secrets' }).click();
    await expect(page.locator('#vault-secrets-table .vault-row', { hasText: keyName }).locator('.vault-chip-apikey')).toBeVisible();
    await page.screenshot({ path: info.outputPath('vault-limits-keys.png') });
  } finally {
    await cleanup('remove vault limit and API key fixtures', removeFixtures, info);
  }
});
