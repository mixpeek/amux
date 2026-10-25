import { test, expect } from './fixtures';

const NAME = 'ate-44-worker';
const ROOT = '/tmp/';
const SESSIONS_ROUTE = '**/api/sessions';

const SAMPLE = {
  name: NAME,
  dir: ROOT,
  desc: 'ATE-44 menu parity fixture',
  tags: ['e2e'],
  provider: 'claude',
  flags: '--model claude-sonnet-4 --effort high',
  active_model: 'claude-sonnet-4',
  task_override: '',
  pinned: false,
  yolo: false,
  isolated: false,
  auto_drain_backlog: true,
  spans_groups: true,
  spans_groups_value: '*',
  spans_groups_own: true,
  running: true,
  status: 'idle',
};

const stubbedPages = new WeakSet<import('@playwright/test').Page>();

async function boot(page: import('@playwright/test').Page) {
  // Keep the app's own startup/poll fetches on the same worker as the fixture.
  // Seeding the lexical `sessions` binding once was racy: a later real fetch
  // could replace it with [] between rendering the menu and clicking Browse
  // files. CI then saw the first two canonical-entry beacons, lost the worker,
  // and never emitted the third. A route fixture models the durable API source
  // instead of briefly overwriting one consumer's local snapshot.
  if (!stubbedPages.has(page)) {
    await page.route(SESSIONS_ROUTE, (route) => route.fulfill({ json: [SAMPLE] }));
    stubbedPages.add(page);
  }
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._renderWorkerActionMenu === 'function');
  await page.waitForFunction(({ name, root }) => {
    const rows = JSON.parse(eval('JSON.stringify(sessions)'));
    return rows.length === 1 && rows[0].name === name && rows[0].dir === root;
  }, { name: NAME, root: ROOT });
  await page.evaluate(({ name, root }) => {
    // These are top-level lexical bindings in the classic app bundle, not
    // window properties. Seed the exact open-peek context used by both handlers.
    eval(`peekSession = ${JSON.stringify(name)}; peekSessionDir = ${JSON.stringify(root)}`);
  }, { name: NAME, root: ROOT });
}

test('worker card and peek share all worker actions, plus both peek-only actions', async ({ page }) => {
  await boot(page);
  const state = await page.evaluate((sample) => {
    const w = window as any;
    const card = document.createElement('div');
    card.innerHTML = w._renderWorkerActionMenu(sample, 'card');
    w._renderPeekWorkerActions(sample);
    const peek = document.getElementById('peek-more-dropdown')!;
    { // Open it the way openPeek does: since 90bedca6 the overlay starts hidden, inert and aria-hidden.
      const ov = document.getElementById('peek-overlay')!; ov.hidden = false; ov.inert = false;
      ov.setAttribute('aria-hidden', 'false'); ov.classList.add('active'); }
    peek.classList.add('open');
    const keys = (root: ParentNode) => Array.from(root.querySelectorAll('[data-worker-action]'))
      .map((el) => (el as HTMLElement).dataset.workerAction);
    const semanticLabel = (el: Element) => {
      const copy = el.cloneNode(true) as HTMLElement;
      copy.querySelectorAll('.mi').forEach((icon) => icon.remove());
      return (copy.textContent || '').trim();
    };
    // Focus mode is the FIRST item, visible with the menu unscrolled (Ethan,
    // 2026-10-20: it sat below the whole action list, off-screen on a phone).
    // Measured in the same render snapshot: holding a locator across an await
    // races the app's normal session-poll rerender (iOS WebKit, full suite).
    const focus = peek.querySelector<HTMLElement>('#peek-focus-btn')!;
    peek.scrollTop = 0;
    const firstItem = peek.querySelector('.peek-more-item');
    const focusRect = focus.getBoundingClientRect();
    const peekRect = peek.getBoundingClientRect();
    const style = getComputedStyle(peek);
    return {
      card: keys(card),
      peek: keys(peek),
      peekOnly: Array.from(peek.querySelectorAll('[data-peek-action], #peek-focus-btn'))
        .map(semanticLabel),
      overflowY: style.overflowY,
      maxHeight: style.maxHeight,
      scrollHeight: peek.scrollHeight,
      clientHeight: peek.clientHeight,
      focusFirst: firstItem === focus,
      focusReachable: focus.isConnected
        && focusRect.top >= peekRect.top - 1
        && focusRect.bottom <= peekRect.bottom + 1,
      headerIds: document.querySelectorAll('#peek-worker-menu-btn').length,
      composerIds: document.querySelectorAll('#peek-composer-more-btn').length,
      legacyDuplicateIds: document.querySelectorAll('#peek-more-btn').length,
    };
  }, SAMPLE);

  // 28 since 9af1c88b added Pause/Resume to the shared worker menu (AMUX-4632).
  expect(state.card).toHaveLength(28);
  expect(state.card).toContain('task-queue');
  expect(state.card).toContain('copy-directory-link');
  expect(state.peek).toEqual(state.card);
  // 64f864c6 made the peek item toggle the split file browser, and the
  // Chat tab's on/off moved into this menu (AMUX-5350).
  expect(state.peekOnly).toEqual(['Focus mode', 'File browser (split)', 'Turn on chat']);
  expect(state.focusFirst).toBe(true);
  expect(state.overflowY).toBe('auto');
  expect(state.maxHeight).not.toBe('none');
  expect(state.scrollHeight).toBeGreaterThan(state.clientHeight);
  expect(state.focusReachable).toBe(true);
  expect(state.headerIds).toBe(1);
  expect(state.composerIds).toBe(1);
  expect(state.legacyDuplicateIds).toBe(0);
});

async function enterFiles(page: import('@playwright/test').Page, source: 'peek-file-browser' | 'peek-directory' | 'browse-files') {
  await boot(page);
  await page.evaluate(({ sample, source }) => {
    const w = window as any;
    w._renderPeekWorkerActions(sample);
    if (source === 'peek-directory') {
      document.getElementById('peek-dir-text')!.click();
    } else {
      document.querySelector<HTMLElement>(source === 'peek-file-browser'
        ? '[data-peek-action="file-browser"]' : '[data-worker-action="browse-files"]')!.click();
    }
  }, { sample: SAMPLE, source });
  await expect(page).toHaveURL(/#path=\/tmp\/$/);
  return page.evaluate(() => JSON.parse(eval(`JSON.stringify({
    session: _exploreSession,
    root: _filesPath,
    activeView,
    filesVisible: getComputedStyle(document.getElementById('files-view')).display !== 'none',
    filesTabSelected: document.getElementById('tab-files').classList.contains('active')
  })`)));
}

test('peek file entries produce the exact same canonical Files route state', async ({ page }) => {
  const directoryPath = await enterFiles(page, 'peek-directory');
  const sharedBrowse = await enterFiles(page, 'browse-files');

  const expected = {
    session: NAME,
    root: ROOT,
    activeView: 'files',
    filesVisible: true,
    filesTabSelected: true,
  };
  expect(directoryPath).toEqual(expected);
  expect(sharedBrowse).toEqual(directoryPath);
});

// 64f864c6: the peek menu's File browser opens the split pane beside the
// terminal instead of leaving it. At phone width there is no room for a split,
// so it takes the same Files route as the other two entries.
test('the peek File browser item splits the terminal on desktop and routes to Files on a phone', async ({ page }) => {
  await boot(page);
  const narrow = await page.evaluate(() => window.innerWidth <= 600);
  const before = page.url();
  await page.evaluate((sample) => {
    const w = window as any;
    w._renderPeekWorkerActions(sample);
    document.querySelector<HTMLElement>('[data-peek-action="file-browser"]')!.click();
  }, SAMPLE);
  if (narrow) {
    await expect(page).toHaveURL(/#path=\/tmp\/$/);
  } else {
    await expect(page.locator('#peek-split-wrap')).toHaveClass(/split-active/);
    expect(page.url()).toBe(before);
  }
});
