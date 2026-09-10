import { test, expect, Page } from './fixtures';

async function sourceFilter(page: Page, kind: string) {
  await page.getByRole('button', {name:'Filter messages', exact:true}).click();
  await page.locator(`[name="peek-filter-source"][value="${kind}"]`).check();
  await page.getByRole('dialog', {name:'Filter worker messages'}).getByRole('button', {name:'Done', exact:true}).click();
}


// Exercise the shipped renderer and actual header buttons, including ANSI spans
// that used to cross block boundaries and Codex's different prompt glyph.
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, route => route.fulfill({json:[{name:'nav-probe',dir:'/tmp/toolbar-probe',running:true,status:'working'}]}));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).highlightPrompts === 'function');
  await page.evaluate(() => {
    document.querySelector('.wt-overlay')?.remove();
    eval("peekSession = 'nav-probe'; _peekMsgRowsFor = peekSession; _peekMsgRows = []; _peekMsgNavKind = 'all'; _peekMsgIndex = -1;");
    document.getElementById('peek-overlay')!.classList.add('active');
    (window as any)._stopPeekPoll();
  });
});

test('separate Codex/Claude blocks, multiline origin matching, and late scoped history', async ({ page }) => {
  const result = await page.evaluate(() => {
    const w = window as any;
    const body = document.getElementById('peek-body')!;
    const raw = '\x1b[34m› Review the first paragraph and verify the implementation\n  with a second line.\n\n  Keep this paragraph together.\n❯ [amux-origin:backend] inspect the build\n› standalone request\nAssistant reply\n❯ \n❯ 1. Yes\n› Ask Codex to do anything\n\n  gpt-6-astra xhigh · ~/Dev/amux';
    body.innerHTML = w._peekHtml(raw);
    const before = [...body.querySelectorAll('.peek-prompt')].map(el => (el as HTMLElement).dataset.msgKind);
    eval("_peekMsgRows = [{session:'nav-probe',type:'direct',text:'Review the first paragraph and verify the implementation with a second line. Keep this paragraph together.'}, {session:'other-worker',type:'direct',text:'standalone request'}]");
    w._peekReclassifyPrompts();
    return { before, after: [...body.querySelectorAll('.peek-prompt')].map(el => (el as HTMLElement).dataset.msgKind),
      nested: body.querySelectorAll('.peek-prompt .peek-prompt').length,
      first: body.querySelector('.peek-prompt')!.textContent,
      leakedReply: body.querySelector('.peek-prompt:last-of-type')?.textContent?.includes('Assistant reply'),
      colors: [...body.querySelectorAll('.peek-prompt')].map(el => getComputedStyle(el).borderLeftColor) };
  });
  expect(result.before).toEqual(['unknown', 'session', 'unknown']);
  expect(result.after).toEqual(['human', 'session', 'unknown']);
  expect(result.nested).toBe(0);
  expect(result.first).toContain('Keep this paragraph together.');
  expect(result.leakedReply).toBeFalsy();
  expect(new Set(result.colors).size).toBe(3);
});

test('header arrows land at the start of a long message and hold through refresh', async ({ page }) => {
  await page.evaluate(() => {
    const w = window as any;
    const raw = 'intro\n'.repeat(35) + '› first human message\n  ' + 'long paragraph '.repeat(500)
      + '\nAssistant\n' + 'output\n'.repeat(35) + '❯ [Scheduled] run checks\nAssistant\n' + 'tail\n'.repeat(40);
    eval('lastPeekHTML = _peekHtml(' + JSON.stringify(raw) + '); _lastLiveHTML = lastPeekHTML; _peekHistoryHTML = "";');
    w.applyPeekSearch(false, false);
    document.getElementById('peek-body')!.scrollTop = 0;
  });
  await page.getByRole('button', { name: 'Next message', exact: true }).click();
  const landing = await page.evaluate(() => {
    const body = document.getElementById('peek-body')!;
    const target = body.querySelector('.peek-msg-current')!;
    return { offset: target.getBoundingClientRect().top - body.getBoundingClientRect().top,
      top: body.scrollTop, locked: eval('_peekScrollLocked'), height: target.getBoundingClientRect().height,
      paddingTop: parseFloat(getComputedStyle(body).paddingTop),
      targetTop: target.getBoundingClientRect().top,
      controlsBottom: document.querySelector('.peek-output-controls')!.getBoundingClientRect().bottom };
  });
  expect(landing.height).toBeGreaterThan(400);
  expect(landing.offset).toBeGreaterThanOrEqual(0);
  // The compact controls float inside the terminal, so navigation deliberately
  // lands at the scroller's padded content start and never underneath them.
  expect(landing.offset).toBeGreaterThanOrEqual(landing.paddingTop - 2);
  expect(landing.offset).toBeLessThan(landing.paddingTop + 20);
  expect(landing.targetTop).toBeGreaterThanOrEqual(landing.controlsBottom);
  expect(landing.top).toBeGreaterThan(100);
  expect(landing.locked).toBe(true);
  await page.evaluate(() => (window as any)._peekReclassifyPrompts());
  expect(await page.locator('#peek-body').evaluate(el => el.scrollTop)).toBe(landing.top);
  await page.getByRole('button', { name: 'Next message', exact: true }).click();
  await expect(page.locator('.peek-msg-current')).toContainText('[Scheduled]');
  await page.getByRole('button', { name: 'Previous message', exact: true }).click();
  await expect(page.locator('.peek-msg-current')).toContainText('first human message');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: test.info().outputPath('terminal-navigation.png') });
});

test('search navigation shares real matches and reports an empty filter', async ({ page }) => {
  await expect(page.locator('#peek-search-wrap')).toBeHidden();
  const beacons: any[] = [];
  await page.route('**/api/client-debug', async route => {
    beacons.push(route.request().postDataJSON());
    await route.fulfill({ json: { ok: true } });
  });
  await page.evaluate(() => {
    eval('lastPeekHTML = "needle\\n" + "output\\n".repeat(60) + "needle";');
  });
  await page.getByRole('button', { name: 'Find in terminal', exact: true }).click();
  await page.getByRole('searchbox', { name: 'Find in terminal', exact: true }).fill('needle');
  await expect(page.locator('[name="peek-filter-source"][value="all"]')).toBeChecked();
  await expect(page.getByRole('button', {name:'Filter messages', exact:true})).toBeEnabled();
  await page.getByRole('button', { name: 'Next message', exact: true }).click();
  await expect(page.locator('#peek-msg-count')).toHaveText('2/2');
  await expect.poll(() => beacons.filter(b => b.verdict === 'landed').length).toBe(1);
  await page.locator('#peek-search').press('Enter');
  await expect(page.locator('#peek-msg-count')).toHaveText('1/2');
  await page.getByRole('button', { name: 'Next message', exact: true }).click();
  await expect(page.locator('#peek-msg-count')).toHaveText('2/2');
  await page.locator('#peek-search').press('Escape');
  await expect(page.locator('#peek-search-wrap')).toBeHidden();
  await expect(page.locator('#peek-overlay')).toBeVisible();
  await expect(page.getByRole('button', {name:'Filter messages', exact:true})).toBeEnabled();
  await sourceFilter(page, 'human');
  await page.evaluate(() => {
    document.getElementById('peek-body')!.innerHTML = '';
  });
  await page.route('**/api/sessions/nav-probe/log?*', route => route.fulfill({ status: 404, json: { error: 'missing' } }));
  await page.getByRole('button', { name: 'Next message', exact: true }).click();
  await expect.poll(() => beacons.filter(b => b.verdict === 'no-targets').length).toBe(1);
  await expect(page.locator('#peek-msg-count')).toHaveText('0');
  await expect(page.locator('#toast')).toContainText('This worker has no saved earlier output.');
});

test('search retains the selected message type and filters matches when it changes', async ({ page }, testInfo) => {
  await page.evaluate(() => {
    eval("_peekMsgRows = [{session:'nav-probe',type:'direct',text:'needle from the owner'}, {session:'nav-probe',type:'direct',text:'another needle from the owner'}]");
    const raw = '› needle from the owner\nAssistant needle output\n❯ [amux-origin:peer] needle from a worker\nAssistant\n› another needle from the owner\nAssistant\n❯ [Scheduled] needle from the scheduler\n';
    eval('lastPeekHTML = _peekHtml(' + JSON.stringify(raw) + '); _lastLiveHTML=lastPeekHTML; _peekHistoryHTML="";');
    (window as any).applyPeekSearch(false, false);
  });
  await sourceFilter(page, 'human');
  await page.getByRole('button', { name: 'Find in terminal', exact: true }).click();
  await page.locator('#peek-search').fill('needle');
  await expect(page.locator('[name="peek-filter-source"][value="human"]')).toBeChecked();
  await expect(page.getByRole('button', {name:'Filter messages', exact:true})).toBeEnabled();
  await expect(page.locator('#peek-msg-count')).toHaveText('1/2');
  expect(await page.locator('.peek-highlight').evaluateAll(nodes => nodes.map(n => (n.closest('.peek-prompt') as HTMLElement)?.dataset.msgKind))).toEqual(['human', 'human']);
  await page.getByRole('button', { name: 'Next message', exact: true }).click();
  await expect(page.locator('#peek-msg-count')).toHaveText('2/2');
  await page.screenshot({ path: testInfo.outputPath('human-search-filter.png') });
  await sourceFilter(page, 'session');
  await expect(page.locator('#peek-search')).toHaveValue('needle');
  await expect(page.locator('#peek-msg-count')).toHaveText('1/1');
  await expect(page.locator('.peek-highlight')).toHaveCount(1);
  expect(await page.locator('.peek-highlight').evaluate(el => (el.closest('.peek-prompt') as HTMLElement)?.dataset.msgKind)).toBe('session');
  await sourceFilter(page, 'all');
  await expect(page.locator('#peek-msg-count')).toHaveText('1/5');
});

test('file actions stay beside the breadcrumb at phone width', async ({ page }, testInfo) => {
  await page.route('**/api/ls?*', route => route.fulfill({ json: {
    path: '/Users/example/Vault/Self', entries: [{ name: 'Journal', type: 'dir' }, { name: 'Vision', type: 'dir' }],
  } }));
  await page.evaluate(() => (window as any).openExplore('/Users/example/Vault/Self', 'self'));
  await expect(page.locator('#files-body')).toContainText('Journal');
  const layout = await page.locator('#files-view > .fe-toolbar').evaluate(el => {
    const first = el.querySelector('#files-back-session-btn')!.getBoundingClientRect();
    const more = el.querySelector('#files-overflow-btn')!.getBoundingClientRect();
    return { delta: Math.abs(first.top - more.top), right: more.right, edge: el.getBoundingClientRect().right, overflow: el.scrollWidth > el.clientWidth + 1 };
  });
  expect(layout.delta).toBeLessThan(4);
  expect(layout.right).toBeLessThanOrEqual(layout.edge);
  expect(layout.overflow).toBe(false);
  await page.locator('#files-overflow-btn').click();
  await expect(page.locator('#files-overflow-menu')).toBeVisible();
  await page.screenshot({ path: testInfo.outputPath('file-toolbar.png') });
});

test('toolbar has one horizontal row, explicit filters and reachable named actions', async ({ page }) => {
  await page.evaluate(() => {
    eval("sessions.push({name:'nav-probe',dir:'/tmp/toolbar-probe',running:true}); peekSessionDir='/tmp/toolbar-probe';");
    (window as any)._peekMsgCount([]);
  });
  const geometry = await page.locator('.peek-toolbar').evaluate(el => ({
    height: el.getBoundingClientRect().height,
    overflow: el.scrollWidth > el.clientWidth + 1,
    controls: [...el.querySelectorAll('button,select')].filter(c => c.getClientRects().length && !c.closest('.peek-filter-panel')).map(c => {
      const r = c.getBoundingClientRect();
      // WebKit may return 43.999996 for a 44px control. Keep subpixel precision.
      return { width: Number(r.width.toFixed(3)), height: Number(r.height.toFixed(3)), visible: c.contains(document.elementFromPoint(r.x + r.width/2, r.y + r.height/2)) };
    }),
  }));
  expect(geometry.height).toBeLessThanOrEqual(48);
  expect(geometry.overflow).toBe(false);
  for (const c of geometry.controls) {
    expect(c.width).toBeGreaterThanOrEqual(44);
    expect(c.height).toBeGreaterThanOrEqual(44);
    expect(c.visible).toBe(true);
  }
  await sourceFilter(page, 'session');
  expect(await page.evaluate(() => eval('_peekMsgNavKind'))).toBe('session');
  await expect(page.locator('#peek-msg-count')).toHaveText('0');
  // Empty loaded output may still have earlier messages; these remain actions.
  await expect(page.getByRole('button', { name: 'Previous message', exact: true })).not.toHaveAttribute('aria-disabled', 'true');
  await page.locator('#peek-overlay').getByRole('button', { name: 'Worker actions', exact: true }).click();
  await expect(page.locator('#peek-more-dropdown [data-worker-action="directory"]')).toHaveText('📁Change directory');
  await expect(page.locator('#peek-more-dropdown [data-worker-action="copy-directory-link"]')).toHaveText('🔗Copy directory link');
  await expect(page.locator('.peek-dir-bar .card-dir-edit')).toHaveCount(0);
  // An action stops event propagation. Its outside-click listener must still
  // be retired, or the next opening click immediately dismisses the menu.
  const workerMenu = page.locator('#peek-overlay').getByRole('button', { name: 'Worker actions', exact: true });
  await page.locator('#peek-more-dropdown [data-worker-action="copy-directory-link"]').click();
  await expect(page.locator('#peek-more-dropdown')).not.toBeVisible();
  await workerMenu.click();
  await expect(workerMenu).toHaveAttribute('aria-expanded', 'true');
  await expect(page.locator('#peek-more-dropdown')).toBeVisible();

  for (let attempt = 0; attempt < 2; attempt++) {
    await page.locator('#peek-more-dropdown [data-worker-action="directory"]').click();
    await expect(page.locator('#edit-title')).toHaveText('Change directory');
    await expect(page.locator('#edit-input')).toHaveValue('/tmp/toolbar-probe');
    await page.locator('#edit-overlay').getByRole('button', { name: 'Cancel', exact: true }).click();
    await workerMenu.click();
    await expect(workerMenu).toHaveAttribute('aria-expanded', 'true');
    await expect(page.locator('#peek-more-dropdown')).toBeVisible();
  }

  await page.locator('#peek-overlay').getByRole('button', { name: 'Worker actions', exact: true }).click();
  const tabs = page.getByRole('button', { name: 'Customize worker tabs' });
  await expect(tabs).toContainText('Tabs');
  const box = await tabs.boundingBox();
  expect(box!.width).toBeGreaterThanOrEqual(44);
  expect(box!.height).toBeGreaterThanOrEqual(44);
  expect(box!.x + box!.width).toBeLessThanOrEqual(page.viewportSize()!.width);
  await tabs.click();
  await expect(page.locator('#peek-tab-customizer-menu')).toBeVisible();
  await expect(tabs).toHaveAttribute('aria-expanded', 'true');
});

test('a toolbar layout regression announces itself to client diagnostics', async ({ page }) => {
  const beacons: any[] = [];
  await page.route('**/api/client-debug', async route => {
    beacons.push(route.request().postDataJSON());
    await route.fulfill({ json: { ok: true } });
  });
  await page.evaluate(async () => {
    document.body.style.zoom = '0.8';
    (window as any)._peekToolbarCheck();
    await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
  });
  expect(beacons.filter(b => b.kind === 'peek-toolbar-layout')).toHaveLength(0);
  await page.locator('#peek-msg-nav').evaluate(el => { (el as HTMLElement).style.flexDirection = 'column'; });
  await page.evaluate(() => (window as any)._peekToolbarCheck());
  await expect.poll(() => beacons.filter(b => b.kind === 'peek-toolbar-layout').length).toBe(1);
  const signal = beacons.find(b => b.kind === 'peek-toolbar-layout');
  expect(signal.verdict).toBe('unusable-controls');
  expect(signal.measured).toBe(true);
  expect(signal.n_considered).toBeGreaterThan(0);
});

test('a scroll gesture ending over a message arrow is inert', async ({ page }) => {
  const beacons: any[] = [];
  await page.route('**/api/client-debug', async route => {
    beacons.push(route.request().postDataJSON());
    await route.fulfill({ json: { ok: true } });
  });
  await page.evaluate(() => {
    const body = document.getElementById('peek-body')!;
    body.innerHTML = '<div style="height:4000px">plain output without prompts</div>';
    body.scrollTop = 0;
    document.getElementById('toast')!.classList.remove('visible');
  });
  const next = page.getByRole('button', { name: 'Next message', exact: true });
  const box = await next.boundingBox();
  expect(box).not.toBeNull();
  await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
  await page.mouse.down();
  await page.evaluate(() => {
    const body = document.getElementById('peek-body')!;
    body.scrollTop = 500;
    body.dispatchEvent(new Event('scroll'));
  });
  await page.mouse.up();

  await expect.poll(() => beacons.filter(b => b.verdict === 'suppressed-scroll-gesture').length).toBe(1);
  await expect(page.locator('#toast')).not.toHaveClass(/visible/);
  expect(await page.locator('#peek-body').evaluate(el => el.scrollTop)).toBe(500);
});

test('an explicit empty navigation loads earlier output and lands on its message', async ({ page }) => {
  const beacons: any[] = [];
  await page.route('**/api/client-debug', async route => {
    beacons.push(route.request().postDataJSON());
    await route.fulfill({ json: { ok: true } });
  });
  await page.route('**/api/sessions/nav-probe/log?*', route => route.fulfill({
    status: 200,
    headers: { 'Content-Type': 'text/plain', 'X-Log-Remaining': '0', 'X-Amux-Session': 'nav-probe' },
    body: '› older human request\nassistant response\n',
  }));
  await page.evaluate(() => {
    eval('peekSearchQuery = ""; _peekMsgNavKind = "all"; _peekEarlier = { chunks: [], loadedKb: 0, done: false, hidden: false, loading: false }; _peekHistoryHTML = ""; _lastLiveHTML = ""; lastPeekHTML = "";');
    document.getElementById('peek-body')!.innerHTML = '';
    document.getElementById('toast')!.classList.remove('visible');
  });
  await page.getByRole('button', { name: 'Next message', exact: true }).click();

  await expect(page.locator('.peek-msg-current')).toContainText('older human request');
  await expect.poll(() => beacons.filter(b => b.verdict === 'loaded-earlier').length).toBe(1);
  await expect.poll(() => beacons.filter(b => b.verdict === 'landed').length).toBe(1);
  await expect(page.locator('#toast')).not.toHaveClass(/visible/);
});

test('rapid worker switch and reconnect cannot cross output, draft, status, card, or earlier-log identity', async ({ page }) => {
  const beacons: any[] = [];
  await page.route('**/api/client-debug', async route => {
    beacons.push(route.request().postDataJSON());
    await route.fulfill({ json: { ok: true } });
  });
  await page.route('**/api/sessions/identity-a/peek?*', async route => {
    await new Promise(resolve => setTimeout(resolve, 180));
    await route.fulfill({ json: { name: 'identity-a', output: 'FOREIGN A LIVE OUTPUT', history: '' } });
  });
  await page.route('**/api/sessions/identity-b/peek?*', async route => {
    await route.fulfill({ json: { name: 'identity-b', output: 'CURRENT B LIVE OUTPUT', history: '' } });
  });
  await page.route('**/api/sessions/identity-a/log?*', async route => {
    await new Promise(resolve => setTimeout(resolve, 220));
    await route.fulfill({
      status: 200,
      headers: { 'Content-Type': 'text/plain', 'X-Log-Remaining': '0', 'X-Amux-Session': 'identity-a' },
      body: 'FOREIGN A EARLIER OUTPUT',
    });
  });

  await page.evaluate(async () => {
    const a = { name: 'identity-a', dir: '/tmp/a', running: true, status: 'active',
      task_name: 'Foreign active task', task_board_id: 'A-1',
      runtime_board: { measured: true, status: 'linked', card_id: 'A-1', card_count: 1 } };
    const b = { name: 'identity-b', dir: '/tmp/b', running: true, status: 'active',
      task_name: 'Selected active task', task_board_id: 'B-2',
      runtime_board: { measured: true, status: 'linked', card_id: 'B-2', card_count: 1 } };
    eval('sessions = [' + JSON.stringify(a) + ',' + JSON.stringify(b) + ']; boardItems = ['
      + JSON.stringify({ id: 'A-1', title: 'Foreign active task', status: 'doing', session: 'identity-a', archived: false }) + ','
      + JSON.stringify({ id: 'B-2', title: 'Selected active task', status: 'doing', session: 'identity-b', archived: false }) + '];');
    (window as any)._draftSave('identity-a', 'FOREIGN A UNSENT DRAFT');
    (window as any)._draftSave('identity-b', 'CURRENT B UNSENT DRAFT');
    await eval(`_idb.set('peek_identity-a', {
      output: 'FOREIGN A CACHED OUTPUT', history: '', time: Date.now(), offline: true
    })`);
    (window as any).openPeek('identity-a');
    // Start an earlier page for A, then switch without waiting for either A
    // response. This is the exact stale-response race from the live overlay.
    (window as any)._peekLoadEarlier();
    (window as any).openPeek('identity-b');
  });

  await expect(page.locator('#peek-title')).toHaveText('identity-b');
  await expect(page.locator('#peek-cmd-input')).toHaveValue('CURRENT B UNSENT DRAFT');
  await expect(page.locator('#peek-task-label')).toHaveText('Selected active task');
  await expect(page.locator('#peek-body')).toContainText('CURRENT B LIVE OUTPUT');

  // A reconnect creates a new generation even for the same worker. Old A and
  // old-B callbacks must still be unable to paint into the reopened B overlay.
  await page.evaluate(() => {
    (window as any).closePeek();
    (window as any).openPeek('identity-b');
  });
  await page.waitForTimeout(350);
  await expect(page.locator('#peek-title')).toHaveText('identity-b');
  await expect(page.locator('#peek-cmd-input')).toHaveValue('CURRENT B UNSENT DRAFT');
  await expect(page.locator('#peek-task-label')).toHaveAttribute('data-worker', 'identity-b');
  await expect(page.locator('#peek-task-label')).toHaveAttribute('data-card', 'B-2');
  await expect(page.locator('#peek-body')).toContainText('CURRENT B LIVE OUTPUT');
  await expect(page.locator('#peek-body')).not.toContainText('FOREIGN A');
  expect(await page.evaluate(() => localStorage.getItem('amux_draft_identity-a'))).toContain('FOREIGN A UNSENT DRAFT');
  expect(beacons.some(b => b.kind === 'peek-identity-discard')).toBe(true);

  // A filtered worker card's click closure is the rendered worker+card pair,
  // not whichever globals happen to be selected by click time.
  const clicked = await page.evaluate(() => {
    const host = document.createElement('div');
    host.innerHTML = (window as any)._activeTaskLink('identity-b', 'B-2', 'Selected active task');
    document.body.appendChild(host);
    let opened = '';
    (window as any)._openIssue = (id: string) => { opened = id; };
    (host.querySelector('button') as HTMLButtonElement).click();
    host.remove();
    return opened;
  });
  expect(clicked).toBe('B-2');
});

test('one source message renders every durable task link and child opens exact detail', async ({ page }) => {
  await page.route('**/api/board/TUBES-2501', route => route.fulfill({ json: {
    id: 'TUBES-2501', title: 'Build offline serving-coverage planner and receipt verifier',
    desc: 'child detail', status: 'doing', session: 'tubescience', archived: false,
    deleted: null, gate: [], tags: [], log: '', due: '', due_time: '',
  } }));
  await page.evaluate(() => {
    (window as any).closePeek();
    eval('boardItems = [];');
    const message = {
      id: 46222, text: 'Decompose the TubeScience acceptance work', type: 'direct',
      session: 'tubescience', ts: Date.now(), card_id: 'TUBES-2474',
      card_title: 'TubeScience acceptance epic', card_status: 'backlog',
      linked_cards: [
        { id: 'TUBES-2474', title: 'TubeScience acceptance epic', status: 'backlog', archived: false },
        { id: 'TUBES-2501', title: 'Build offline serving-coverage planner and receipt verifier', status: 'doing', archived: false },
      ],
    };
    const host = document.createElement('div');
    host.id = 'lineage-message-probe';
    host.innerHTML = (window as any)._cmdHistItemHTML(message, {
      sel: new Set(), key: () => 'MSG-46222', toggle: '_msgSelToggle', resend: '_msgResend',
      searchId: 'msgs-search', rowClass: 'msg-row', target: () => 'tubescience',
    });
    document.body.appendChild(host);
  });

  await expect(page.getByRole('button', { name: 'Open task TUBES-2474, backlog' })).toHaveCount(1);
  const child = page.getByRole('button', { name: 'Open task TUBES-2501, doing' });
  await expect(child).toHaveCount(1);
  await child.click();
  await expect(page.locator('#board-detail-overlay')).toHaveClass(/active/);
  await expect(page.locator('#bd-key')).toHaveText('TUBES-2501');
  await expect(page.locator('#bd-title')).toHaveValue('Build offline serving-coverage planner and receipt verifier');
});


test('a worker menu that loses its opening announces the failure', async ({ page }) => {
  const beacons: any[] = [];
  await page.route('**/api/client-debug', async route => {
    beacons.push(route.request().postDataJSON());
    await route.fulfill({ json: { ok: true } });
  });
  await page.evaluate(() => {
    eval("sessions.push({name:'nav-probe',dir:'/tmp/toolbar-probe',running:true});");
    (window as any).togglePeekMoreMenu();
    // Positive diagnostic control: lose the opening before the first paint.
    (window as any)._closePeekMore();
  });
  await expect.poll(() => beacons.filter(b => b.kind === 'worker-action-menu').length).toBe(1);
  const signal = beacons.find(b => b.kind === 'worker-action-menu');
  expect(signal).toMatchObject({ verdict: 'open-lost', measured: true, session: 'nav-probe' });
  expect(signal.n_considered).toBeGreaterThan(20);
});


async function filterSpecimen(page: Page) {
  await page.evaluate(() => {
    eval("peekSessionDir='/tmp/toolbar-probe'; _peekMsgRows=[{session:'nav-probe',type:'direct',text:'Review AMUX-4242 needle in docs/result.md'}, {session:'nav-probe',type:'direct',text:'Check needle at https://example.com/report'}, {session:'nav-probe',type:'direct',text:'Plain needle request'}];");
    const raw = '› Review AMUX-4242 needle in docs/result.md\nAssistant needle reply\n'
      + '❯ [amux-origin:peer] Check AMUX-4242 needle in docs/worker.md\nAssistant\n'
      + '› Check needle at https://example.com/report\nAssistant\n'
      + '› Plain needle request\nAssistant\n'
      + '❯ [Scheduled] needle routine\nAssistant\n';
    eval('lastPeekHTML = _peekHtml(' + JSON.stringify(raw) + '); _lastLiveHTML=lastPeekHTML; _peekHistoryHTML="";');
    (window as any).applyPeekSearch(false, false);
  });
}

test('filter button combines source and content with Find, then resets both without losing the query', async ({page}) => {
  await filterSpecimen(page);
  await page.getByRole('button', {name:'Filter messages', exact:true}).click();
  const panel=page.getByRole('dialog', {name:'Filter worker messages'});
  await panel.getByRole('radio', {name:'Human', exact:true}).check();
  await panel.getByRole('radio', {name:'Board references', exact:true}).check();
  await expect(page.locator('#peek-filter-summary')).toHaveText('Human · Board references');
  await expect(page.locator('#peek-msg-count')).toHaveText('1');
  await panel.getByRole('button', {name:'Done',exact:true}).click();
  await page.getByRole('button', {name:'Next message',exact:true}).click();
  await expect(page.locator('.peek-msg-current')).toContainText('Review AMUX-4242');
  await page.getByRole('button', {name:'Find in terminal',exact:true}).click();
  await page.locator('#peek-search').fill('needle');
  await expect(page.locator('.peek-highlight')).toHaveCount(1);
  await expect(page.locator('#peek-msg-count')).toHaveText('1/1');
  await page.getByRole('button', {name:'Filter messages',exact:true}).click();
  await panel.getByRole('radio', {name:'Workers',exact:true}).check();
  await expect(page.locator('#peek-filter-summary')).toHaveText('Workers · Board references');
  await expect(page.locator('.peek-highlight')).toHaveCount(1);
  await expect(page.locator('.peek-highlight')).toHaveJSProperty('textContent','needle');
  expect(await page.locator('.peek-highlight').evaluate(el => (el.closest('.peek-prompt') as HTMLElement).dataset.msgKind)).toBe('session');
  await panel.getByRole('button', {name:'Reset',exact:true}).click();
  await expect(page.locator('#peek-search')).toHaveValue('needle');
  await expect(page.locator('.peek-highlight')).toHaveCount(6);
  await expect(page.locator('#peek-filter-summary')).toHaveText('All messages');
  await expect(page.locator('#peek-filter-btn')).not.toHaveClass(/active/);
  await expect(panel.getByRole('button',{name:'Reset',exact:true})).toBeDisabled();
});

test('file and link filters use rendered references, and empty combinations emit measured evidence', async ({page}) => {
  const signals:any[]=[];
  await page.route('**/api/client-debug',async route => {signals.push(route.request().postDataJSON());await route.fulfill({json:{ok:true}});});
  await filterSpecimen(page);
  await page.getByRole('button',{name:'Filter messages',exact:true}).click();
  const panel=page.getByRole('dialog',{name:'Filter worker messages'});
  await panel.getByRole('radio',{name:'Files',exact:true}).check();
  await expect(page.locator('#peek-msg-count')).toHaveText('2');
  await panel.getByRole('radio',{name:'Human',exact:true}).check();
  await expect(page.locator('#peek-msg-count')).toHaveText('1');
  await panel.getByRole('radio',{name:'Links',exact:true}).check();
  await expect(page.locator('#peek-msg-count')).toHaveText('1');
  await panel.getByRole('radio',{name:'Workers',exact:true}).check();
  await expect(page.locator('#peek-msg-count')).toHaveText('0');
  await expect.poll(() => signals.find(s=>s.kind==='peek-message-filter' && s.source_filter==='session' && s.content_filter==='links')).toMatchObject({verdict:'no-matches',measured:true,n_considered:5,matched_messages:0});
  // A repaint must keep the combination; a zero is scoped, not the full history.
  await page.evaluate(()=>(window as any).applyPeekSearch(false,false));
  await expect(panel.getByRole('radio',{name:'Workers',exact:true})).toBeChecked();
  await expect(panel.getByRole('radio',{name:'Links',exact:true})).toBeChecked();
  await expect(page.locator('#peek-msg-count')).toHaveText('0');
});

test('filter popover stays within the phone and supports Escape, outside click and reopening',async ({page},testInfo)=>{
  await filterSpecimen(page);
  await page.evaluate(()=>(window as any)._applyTheme(true));
  const button=page.getByRole('button',{name:'Filter messages',exact:true});
  const panel=page.getByRole('dialog',{name:'Filter worker messages'});
  await button.click();
  await expect(button).toHaveAttribute('aria-expanded','true');
  await panel.getByRole('radio',{name:'Human',exact:true}).check();
  await panel.getByRole('radio',{name:'Files',exact:true}).check();
  await page.screenshot({path:testInfo.outputPath('worker-message-filters.png')});
  const bounds=await panel.boundingBox();
  expect(bounds!.x).toBeGreaterThanOrEqual(0);
  expect(bounds!.x+bounds!.width).toBeLessThanOrEqual(page.viewportSize()!.width);
  expect(bounds!.y+bounds!.height).toBeLessThanOrEqual(page.viewportSize()!.height);
  expect(await panel.locator('.peek-filter-option').evaluateAll(els=>els.every(el=>el.getBoundingClientRect().height>=43.9))).toBe(true);
  await panel.getByRole('radio',{name:'Files',exact:true}).press('Escape');
  await expect(panel).toBeHidden();
  await expect(button).toBeFocused();
  await expect(page.locator('#peek-overlay')).toBeVisible();
  await button.click();
  await page.locator('#peek-title').click();
  await expect(panel).toBeHidden();
  await button.click();
  await expect(panel).toBeVisible();
  await expect(panel.getByRole('radio',{name:'Human',exact:true})).toBeChecked();
  await expect(panel.getByRole('radio',{name:'Files',exact:true})).toBeChecked();
  await panel.getByRole('button',{name:'Done',exact:true}).click();
  await page.screenshot({path:testInfo.outputPath('worker-message-filter-button.png')});
});

test('earlier output uses complete conversation pages and a stable cursor', async ({ page }) => {
  const requests: URL[] = [];
  await page.route('**/api/sessions/nav-probe/log?*', async route => {
    const url = new URL(route.request().url());
    requests.push(url);
    expect(url.searchParams.get('source')).toBe('conversation');
    const older = url.searchParams.has('before');
    await route.fulfill({ status: 200, headers: {
      'Content-Type': 'text/plain', 'X-Amux-Session': 'nav-probe',
      'X-Log-Source': 'conversation', 'X-Log-Conversation': 'conversation-one',
      'X-Log-Remaining': older ? '0' : '4200',
    }, body: older ? '❯ Earlier complete request\n\n⏺ Earlier complete answer'
      : '❯ Review the import rails\n\n⏺ Complete TubeScience response without spinner fragments' });
  });
  await page.evaluate(() => {
    eval('_peekHistoryRaw = "⏺ Complete TubeScience response without spinner fragments"; _peekHistoryHTML = _peekHtml(_peekHistoryRaw); _lastLiveHTML = "";');
  });
  expect(await page.evaluate(() => (window as any)._peekLoadEarlier())).toBe('loaded');
  let content = await page.locator('#peek-body').innerText();
  expect(content.match(/Complete TubeScience response/g)).toHaveLength(1);
  expect(content).not.toMatch(/\* d i|\+ e n 4/);
  expect(await page.evaluate(() => (window as any)._peekLoadEarlier())).toBe('loaded');
  expect(requests[1].searchParams.get('before')).toBe('4200');
  expect(requests[1].searchParams.get('conversation')).toBe('conversation-one');
  content = await page.locator('#peek-body').innerText();
  expect(content.indexOf('Earlier complete request')).toBeLessThan(content.indexOf('Review the import rails'));
  expect(content).toContain('beginning of saved output');
  await page.screenshot({ path: test.info().outputPath('readable-conversation-history.png') });
});

test('conversation overlap removes only the exact shared tail and keeps new output', async ({ page }) => {
  const result = await page.evaluate(() => {
    const trim = (window as any)._peekAfterConversation;
    const saved = '❯ Earlier request\n⏺ Complete response\n  Second line';
    return [trim(saved, '⏺ Complete response\n  Second line'),
      trim(saved, '⏺ Complete response\n  Second line\n\n❯ A new request'),
      trim(saved, '⏺ Complete response\n  A different second line'),
      trim('❯ Earlier request\n⏺ Complete response\n\x1b[0m\n\n  Second line', '⏺ Complete response\n\n  Second line'),
      trim('❯ Earlier request\n⏺ Complete response\n\x1b[0m\n\n  Second line', '⏺ Complete response\n\n  Second line\n\n❯ New after blank lines')];
  });
  expect(result).toEqual(['', '❯ A new request', '⏺ Complete response\n  A different second line', '', '❯ New after blank lines']);
});

test('live worker composer is a preserved draft, never a delivered message', async ({ page }) => {
  const raw = '⏺ Completed response\n\x1b[38;5;114mUpdate installed · Restart to update\x1b[39m\n'
    + '\x1b[38;5;244m──────────────── tubescience ─\n\x1b[39m❯\u00a0[Pasted text #401 +11 lines][Pasted text #402 +11 lines] A partial message\n'
    + '  with an internal marker. [AMUX-INJECT-END]\n\n────────────────────────\n⏵⏵ bypass permissions on · 1 shell · 1 feedback draft\n/rc failed';
  await page.route('**/api/sessions/nav-probe/peek?*', route => route.fulfill({json:{name:'nav-probe',live:raw,history:'❯ Delivered message mentioning [Pasted text #9 +2 lines]\n\n⏺ Saved answer'}}));
  await page.evaluate(async () => {
    eval('_lastPeekRaw = ""; _peekScrollLocked = false;');
    await (window as any).refreshPeek();
  });
  const draft = page.locator('.peek-worker-input');
  await expect(draft).toBeVisible();
  await expect(page.locator('#peek-overlay')).toHaveCSS('opacity', '1');
  await expect(draft.locator('summary')).toHaveText('Unsent worker input · 2 pasted blocks');
  await expect(draft.locator('pre')).toBeHidden();
  await expect(page.locator('#peek-body .peek-prompt')).toHaveCount(1);
  await expect(page.locator('#pk-live')).not.toContainText('Unclassified');
  await page.screenshot({path:test.info().outputPath('worker-input-collapsed.png')});
  await draft.locator('summary').click();
  await expect(draft.locator('pre')).toContainText('[Pasted text #401 +11 lines]');
  await expect(draft.locator('pre')).toContainText('[AMUX-INJECT-END]');
  await expect(page.locator('#peek-body .peek-prompt')).toHaveCount(1);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({path:test.info().outputPath('worker-input-expanded.png')});
});
