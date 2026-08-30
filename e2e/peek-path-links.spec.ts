import { test, expect } from '@playwright/test';

// AMUX-3663. Ethan, with a screenshot of these exact links in the Terminal tab:
// "make sure i can click the links and they work / open the files".
//
// They worked, and they opened the containing FOLDER. Clicking a path ending in
// `.jpg` and getting a directory listing is a surprise, and the rest of the app
// already disagreed — every other `.file-link` in the SPA calls openFilePreview.
//
// The fixture runs the REAL render pipeline (`_peekHtml`) over raw output rather
// than hand-writing the span it emits, because the defect lives in the click
// handler that pipeline wires up; a hand-written span would assert against my
// own idea of the markup and would stay green if linkification itself broke.
//
// RENDER AND CLICK IN ONE `evaluate` ON PURPOSE. `#peek-body` is repainted by a
// 350ms poll, so injecting HTML in one call and clicking it in the next is a
// race — the first draft of this file lost it exactly once, in the test that did
// an extra round trip in between, and the restored original handler pointed at a
// different path. `el.click()` inside the evaluate is still a real DOM click
// firing the real inline handler; it just cannot be overwritten mid-flight.
//
// The discriminator needs no filesystem: a FILE opens `#file-overlay` and leaves
// peek up, a DIRECTORY calls openExplore, which closes peek and switches the
// view to `files`. Absolute paths throughout, so nothing depends on a session
// cwd being resolvable in CI.

// A BARE RELATIVE path, and that is the whole point of the fixture.
//
// The first draft of this file used an absolute path and passed against the
// PRE-FIX build, which is how I found out I had tested an ancestor of my own
// change: `ansiToHtml` linkifies absolute paths itself (its `fileRe` requires a
// leading `/` or `./`) and wires them straight to `openFilePreview`. So those
// already opened the file and never reached `_openPathFromOutput` at all.
//
// `_linkifyPaths` exists precisely for what that regex cannot see — the bare
// `customers/rothco/data/x.csv` a worker writes inside its own cwd — and THAT
// is the branch that opened a folder instead of the file. Same blue span, two
// behaviours, decided by whether the path happened to start with a slash.
const CWD = '/private/tmp/amux-e2e-3663';
const REL_FILE = 'scratchpad/poster.jpg';
const FILE_PATH = CWD + '/' + REL_FILE;
const DIR_PATH = CWD + '/scratchpad';

async function openPeek(page: import('@playwright/test').Page) {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function', { timeout: 20000 });
  await page.evaluate(() => (window as any).openPeek('e2e-probe'));
  await page.waitForSelector('#peek-overlay', { state: 'visible', timeout: 15000 });
  // A relative path is only meaningful against the session's cwd, and
  // `_linkifyPaths` deliberately refuses to link one without it rather than
  // render text that looks clickable and does nothing.
  //
  // Set through `eval`, not `window.peekSessionDir = ...`. It is declared
  // `let` at app.js top level, and a top-level `let` in a classic script lives
  // in the global DECLARATIVE environment, not on `window` — so the obvious
  // assignment creates an unrelated property and the linkifier keeps reading
  // ''. The precondition assertion below is what caught that; without it this
  // spec would have gone green on zero links.
  await page.evaluate((cwd) => eval('peekSessionDir = ' + JSON.stringify(cwd)), CWD);
}

/** Render raw output through the shipped pipeline and click the linkified path. */
async function renderAndClick(page: import('@playwright/test').Page, raw: string) {
  return page.evaluate((text) => {
    const w = window as any;
    const body = document.getElementById('peek-body')!;
    body.innerHTML = w._peekHtml(text);
    const links = body.querySelectorAll('.file-link, .md-link');
    if (links.length) (links[0] as HTMLElement).click();
    return { links: links.length, clicked: (links[0] as HTMLElement | undefined)?.textContent || '' };
  }, raw);
}

function overlayState(page: import('@playwright/test').Page) {
  return page.evaluate(() => ({
    fileOverlay: document.getElementById('file-overlay')!.classList.contains('active'),
    peekOpen: document.getElementById('peek-overlay')!.classList.contains('active'),
    title: document.getElementById('file-title')!.textContent,
    subpath: document.getElementById('file-subpath')!.textContent,
    subClickable: document.getElementById('file-subpath')!.classList.contains('clickable'),
  }));
}

test('a RELATIVE file path in session output opens the file, not its folder', async ({ page }) => {
  await openPeek(page);
  // Phrased the way a worker actually writes it: bare, relative to its cwd.
  const r = await renderAndClick(page, `Contacts are in ${REL_FILE} now.\n`);
  // Precondition, asserted rather than assumed: with no link rendered every
  // assertion below would pass vacuously.
  expect(r.links, 'the render pipeline must have produced a clickable path').toBeGreaterThan(0);
  expect(r.clicked).toContain('poster.jpg');

  await page.waitForTimeout(500);
  const s = await overlayState(page);

  expect(s.fileOverlay, 'clicking a file must open the file viewer').toBe(true);
  // Name the FILE, not the folder. False before the fix: the old handler never
  // opened this overlay at all, and a version that opened it on the containing
  // directory would still fail here.
  expect(s.title).toBe('poster.jpg');
  expect(s.subpath).toBe(DIR_PATH);
  // The way back has to EXIST. `_openPathFromOutput` used to justify sending
  // every click to the browser by claiming "the browser is reachable from a
  // preview" while nothing implemented that (ethos rule 6).
  expect(s.subClickable, 'the folder line must be a real way back to the browser').toBe(true);
  expect(s.peekOpen, 'opening a file must not tear down the session view').toBe(true);
});

test('a DIRECTORY path still opens the file browser', async ({ page }) => {
  // The negative control that matters: a change routing EVERY path to the file
  // viewer would pass the test above and break directory navigation, which is
  // what the previous design was built around.
  //
  // Called directly rather than through a linkified span, because
  // `_linkifyPaths` only matches paths WITH a file extension by design — a
  // directory reaches this handler from elsewhere in the UI, so the span is not
  // the route under test here. The routing decision is.
  await openPeek(page);
  await page.evaluate((dir) => (window as any)._openPathFromOutput(dir), DIR_PATH);
  await page.waitForTimeout(600);

  const s = await overlayState(page);
  expect(s.fileOverlay, 'a directory must NOT open the file viewer').toBe(false);
  expect(s.peekOpen, 'openExplore closes peek and switches to the files view').toBe(false);
});
