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

// Minimal boot for the FIRST test below: it calls _renderWorkerActionMenu/
// _renderPeekWorkerActions directly with an explicit `sample` argument and
// never touches peekSession/sessions/peekSessionDir at all — it doesn't need
// a real peek session, only the script loaded. Keep it that way: an earlier
// version of this fix made ALL boots open a real peek via openPeek(), and
// that function's own side effects (it schedules further async UI work —
// message badge, branch fetch, panel resets) raced this test's OWN DOM
// manipulation of the SAME elements, intermittently detaching
// `#peek-focus-btn` between the state.evaluate() call and the later
// `.scrollIntoViewIfNeeded()` ("Element is not attached to the DOM"). This
// test has no reason to pay for that; only enterFiles() below does.
//
// origin/main independently attempted a fix for this same file (a0775e33,
// "stabilize worker action parity") while this PR was in flight, converging
// on a similar session/route mock but applying it (and a still-broken nested
// eval() for peekSession/peekSessionDir) unconditionally to BOTH tests —
// which is exactly the regression this comment describes: confirmed still
// failing on main with the identical "Element is not attached to the DOM"
// error at the identical line, in the identical test, after that commit
// landed (run 33921870392). Keeping this file's own boot/bootWithPeek split
// on merge rather than origin/main's version, since main's own CI already
// shows it doesn't fully fix the bug.
async function boot(page: import('@playwright/test').Page) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._renderWorkerActionMenu === 'function');
}

// enterFiles() below calls this three times on the SAME page — each one a
// fresh page.goto('/'), but NOT a fresh origin, so localStorage survives
// across them, and drives a REAL peek session (openPeek) + real navigation
// (_browseWorkerFiles/openExplore), unlike the plain boot() above.
async function bootWithPeek(page: import('@playwright/test').Page) {
  // App boot restores the persisted view (_restoreScreen reads
  // `amux_ui_view`) BEFORE any of this file's own seeding/clicking runs, at
  // _filesPath's freshly-reset default of '/' — so the SECOND and THIRD
  // calls in a test can auto-navigate into Files at '/' first, then race the
  // click handler's own correct navigation to '/tmp/'. Whichever response
  // lands last wins, which is why an earlier attempt at this fix showed an
  // inconsistent final path (not the consistent "never navigated" shape the
  // seeding bug below produced). An addInitScript clears the one key that
  // persists this, before EVERY navigation on this page, so each call here
  // genuinely starts fresh.
  await page.addInitScript(() => { try { localStorage.removeItem('amux_ui_view'); } catch (e) {} });
  // `sessions`/`peekSession`/`peekSessionDir` are top-level lexical (`let`)
  // bindings in the classic app bundle, not window properties, and a nested
  // eval() inside a page.evaluate(fn, arg) callback cannot see or reassign
  // them (Playwright drives that callback via Runtime.callFunctionOn, its
  // own isolated scope) — an earlier attempt at this fix tried exactly that
  // and it silently no-op'd every time, so `sessions` stayed `[]`,
  // `_browseWorkerFiles` computed an empty root, and every click here just
  // toasted "This worker has no directory to browse" instead of navigating.
  //
  // Poking `sessions` once via page.evaluate's STRING form (which DOES reach
  // those bindings, like typing consecutive lines into the DevTools console)
  // was closer, but still not sufficient on its own: the app's own
  // fetchSessions() poll runs on load AND periodically, and across three
  // sequential calls in one test (real page loads, real navigation, real
  // assertion retries) enough wall-clock time elapses that a live poll can
  // land between the seed and the click, silently replacing the one-shot
  // fixture with whatever the real dev/CI server actually has running —
  // which has no worker named 'ate-44-worker', so the root computed empty
  // again, on whichever call happened to have the most elapsed time.
  //
  // The robust fix, and the one this whole suite already uses everywhere
  // else (see fixtures.ts's entire reason for existing): mock the REQUEST,
  // not the client variable. Every fetchSessions() call — first load and
  // every later poll — now gets the fixture, so `sessions` stays correct for
  // the entire test regardless of timing.
  //
  // Guarded to register the route only ONCE per test (this runs up to three
  // times on the same page): fixtures.ts's own AF-47 wrapper fails a stub
  // with zero hits at teardown, and a second page.route() on an identical
  // pattern shadows the first (last registered wins interception) — the
  // earlier registrations would each end up with zero hits.
  const routed = page as unknown as { _sessionsRouted?: boolean };
  if (!routed._sessionsRouted) {
    routed._sessionsRouted = true;
    await page.route(SESSIONS_ROUTE, (route) => route.fulfill({ json: [SAMPLE] }));
  }
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._renderWorkerActionMenu === 'function');
  // Wait for the mocked response to actually have landed in `sessions`
  // before opening the peek — a bare Array.isArray/some() check in STRING
  // form, for the same reason the seeding attempt above needed one.
  await page.waitForFunction(`Array.isArray(sessions) && sessions.some(s => s.name === ${JSON.stringify(NAME)})`);
  // openPeek() is a genuine top-level FUNCTION DECLARATION, not a `let` —
  // the classic bundle attaches it to `window` automatically, so calling it
  // via a normal (function-form) page.evaluate correctly reaches its own
  // closure over peekSession/peekSessionDir regardless of how it's invoked.
  // That asymmetry (declarations reach window; `let`s do not) is the root
  // fact every attempt above kept working around instead of using directly.
  await page.evaluate((name) => { (window as any).openPeek(name); }, NAME);
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
  // Every menu item in this codebase renders an icon span before its label
  // (see _renderWorkerActionMenu's own `<span class="mi">` + label pattern),
  // and .textContent naturally includes that child span's text — real DOM
  // icon glyphs, not a CSS ::before. Bare 'File browser'/'Focus mode' was
  // never what got rendered as raw textContent; semanticLabel() strips the
  // icon span before reading, so this checks the label itself rather than
  // depending on exact glyph placement (origin/main's own independent fix
  // for the same test bug, merged in over the icon-embedding approach here
  // originally, which broke this same assertion until reconciled).
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
  await bootWithPeek(page);
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
  // Same top-level-`let` visibility problem as boot() above, on the read
  // side: `_exploreSession`/`_filesPath`/`activeView` are the classic
  // bundle's own lexical bindings, unreachable from a nested eval() inside a
  // page.evaluate(fn) callback. The string form runs as a top-level
  // Runtime.evaluate and can read them directly.
  const raw = await page.evaluate(`JSON.stringify({
    session: _exploreSession,
    root: _filesPath,
    activeView,
    filesVisible: getComputedStyle(document.getElementById('files-view')).display !== 'none',
    filesTabSelected: document.getElementById('tab-files').classList.contains('active'),
  })`);
  return JSON.parse(raw as string);
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
