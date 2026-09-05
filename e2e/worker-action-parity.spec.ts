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

test('worker card and peek share all 25 worker actions, plus both peek-only actions', async ({ page }) => {
  await boot(page);
  const state = await page.evaluate((sample) => {
    const w = window as any;
    const card = document.createElement('div');
    card.innerHTML = w._renderWorkerActionMenu(sample, 'card');
    w._renderPeekWorkerActions(sample);
    const peek = document.getElementById('peek-more-dropdown')!;
    document.getElementById('peek-overlay')!.classList.add('active');
    peek.classList.add('open');
    const keys = (root: ParentNode) => Array.from(root.querySelectorAll('[data-worker-action]'))
      .map((el) => (el as HTMLElement).dataset.workerAction);
    const semanticLabel = (el: Element) => {
      const copy = el.cloneNode(true) as HTMLElement;
      copy.querySelectorAll('.mi').forEach((icon) => icon.remove());
      return (copy.textContent || '').trim();
    };
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
      headerIds: document.querySelectorAll('#peek-worker-menu-btn').length,
      composerIds: document.querySelectorAll('#peek-composer-more-btn').length,
      legacyDuplicateIds: document.querySelectorAll('#peek-more-btn').length,
    };
  }, SAMPLE);

  expect(state.card).toHaveLength(25);
  expect(state.peek).toEqual(state.card);
  expect(state.peekOnly).toEqual(['File browser', 'Focus mode']);
  expect(state.overflowY).toBe('auto');
  expect(state.maxHeight).not.toBe('none');
  expect(state.scrollHeight).toBeGreaterThan(state.clientHeight);
  await page.locator('#peek-focus-btn').scrollIntoViewIfNeeded();
  await expect(page.locator('#peek-focus-btn')).toBeVisible();
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
  const fileBrowser = await enterFiles(page, 'peek-file-browser');
  const directoryPath = await enterFiles(page, 'peek-directory');
  const sharedBrowse = await enterFiles(page, 'browse-files');

  const expected = {
    session: NAME,
    root: ROOT,
    activeView: 'files',
    filesVisible: true,
    filesTabSelected: true,
  };
  expect(fileBrowser).toEqual(expected);
  expect(directoryPath).toEqual(fileBrowser);
  expect(sharedBrowse).toEqual(fileBrowser);
});
