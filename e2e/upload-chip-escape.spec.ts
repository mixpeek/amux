import { test, expect } from '@playwright/test';

/**
 * A peek attachment must ALWAYS be removable, and closing the peek must not
 * strand one.
 *
 * WHAT BROKE (reported 2026-08-18, with the artifact: four EMPTY `.chunked-*`
 * dirs in ~/.amux/uploads — uploads that got `start` and never wrote chunk 0).
 *
 * Two defects compounded into a composer that could not be used at all:
 *
 *   1. `uploadAndAttach` decided "did the user cancel?" by asking whether its
 *      placeholder was still inside `peekFiles`. But `_peekFilesStash` SWAPS
 *      THAT ARRAY WHOLESALE when the peek closes or switches worker, so closing
 *      the peek mid-upload read as a removal: the transfer was abandoned and the
 *      placeholder was left in the stashed array at 0%, forever.
 *   2. The chip rendered its × only in the `done` branch. So the stranded chip —
 *      the one state where you most need it — had no remove control, and
 *      `sendPeekCmd()` refuses while any chip lacks a `.path`.
 *
 * Net effect: attach a picture, close the peek, reopen, and the composer is
 * permanently unsendable with no way out but abandoning the draft.
 *
 * This drives the REAL functions against a stalled server, because the bug only
 * exists while a transfer is in flight — a test that attaches an
 * instantly-completing file passes against the broken code. The assertions are
 * on the DOM and on a real click, not on internal state: the reported symptom
 * was "no remove button", which is a rendering fact.
 */

type Win = Window & typeof globalThis & Record<string, any>;

// Stall chunk 0 until released, so the chip is genuinely mid-upload.
async function stubStalledUpload(page: import('@playwright/test').Page) {
  await page.evaluate(() => {
    const w = window as Win;
    w.__release = null;
    const gate = new Promise<void>((r) => { w.__release = r; });
    w.__origFetch = w.fetch;
    w.fetch = async (url: any, opts: any) => {
      const u = String(url);
      if (u.includes('/api/upload/start')) {
        return new Response(JSON.stringify({ id: 'test01', chunks: 1 }), { status: 200 });
      }
      if (u.includes('/chunk/')) {
        await Promise.race([
          gate,
          new Promise((_, rej) => opts?.signal?.addEventListener('abort', () => rej(new Error('aborted')))),
        ]);
        return new Response(JSON.stringify({ ok: true }), { status: 200 });
      }
      if (u.includes('/finish')) {
        return new Response(JSON.stringify({ path: '/tmp/IMG_2987.png', url: '/api/uploads/IMG_2987.png' }), { status: 200 });
      }
      return w.__origFetch(url, opts);
    };
  });
}

async function attachStalledFile(page: import('@playwright/test').Page) {
  await page.evaluate(() => {
    const w = window as Win;
    const f = new File([new Uint8Array(64)], 'IMG_2987.png', { type: 'image/png' });
    w.uploadAndAttach(f);          // deliberately NOT awaited — it is stalled
  });
  await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(1);
}

test.beforeEach(async ({ page }) => {
  await page.goto('/');
  // Skip the first-run walkthrough via its own Skip button, as golden.spec.ts
  // does. Its backdrop covers the page and swallows the chip click this suite
  // is here to prove works — which failed as "x does not remove the chip", i.e.
  // as the product bug, on all three targets.
  const wt = page.locator('#wt-overlay.open');
  await wt.waitFor({ state: 'visible', timeout: 8_000 }).catch(() => {});
  if (await wt.isVisible()) {
    await page.locator('#wt-tooltip .wt-skip').click();
    await expect(wt).toBeHidden();
  }
  await page.waitForFunction(() => typeof (window as any).uploadAndAttach === 'function', { timeout: 20000 });
  // Open a peek so the composer is REALLY interactive. Without it the attach bar
  // still renders and Playwright still calls the chip visible, but the closed
  // overlay takes no pointer events, so the click lands on <body> — which
  // reports as "x does not remove the chip", i.e. as this very bug. Calling
  // openPeek directly is the house idiom (tab-customizer.spec.ts): the harness
  // has no sessions to click on.
  await page.evaluate(() => (window as Win).openPeek('e2e-probe'));
  await page.waitForSelector('#peek-overlay', { state: 'visible', timeout: 15000 });
  await stubStalledUpload(page);
});

test('an in-flight attachment can still be removed', async ({ page }) => {
  await attachStalledFile(page);

  const chip = page.locator('#peek-attach-bar .peek-attach-chip');
  const x = chip.locator('.chip-remove');
  // The reported symptom, asserted directly: the control must EXIST while the
  // upload is still running, not only after it succeeds.
  await expect(x, 'an uploading chip must carry a remove control').toHaveCount(1);

  await x.click();
  await expect(chip, 'clicking x must actually remove the chip').toHaveCount(0);
});

test('closing the peek mid-upload does not strand the attachment', async ({ page }) => {
  await attachStalledFile(page);

  // Switch to another worker and back — the real path, which is openPeek's own
  // _peekFilesStash/_peekFilesRestore pair, not a hand-rolled imitation of it.
  await page.evaluate(() => (window as Win).openPeek('e2e-other'));
  await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(0);

  // The transfer must still be alive: let the stalled chunk through while the
  // peek is CLOSED, which is the precise moment the old code gave up.
  await page.evaluate(() => (window as Win).__release());

  await page.evaluate(() => (window as Win).openPeek('e2e-probe'));
  const chip = page.locator('#peek-attach-bar .peek-attach-chip');
  await expect(chip).toHaveCount(1);

  // Either it finished (no longer 'uploading'), or it is honestly marked failed
  // with a retry — what it must NOT be is a chip frozen mid-progress that no
  // longer has anything driving it.
  await expect
    .poll(async () => chip.evaluate((el) => el.className), { timeout: 10_000 })
    .not.toContain('uploading');

  // And whatever state it landed in, there is a way out of it.
  await expect(chip.locator('.chip-remove'), 'a restored chip must offer a way out').toHaveCount(1);
});

test('a failed attachment keeps its chip, with retry and remove', async ({ page }) => {
  // Server rejects the chunk — the shape a mid-upload server restart produces
  // ("unknown upload"), which used to delete the chip behind a single toast.
  await page.evaluate(() => {
    const w = window as Win;
    w.fetch = async (url: any, opts: any) => {
      const u = String(url);
      if (u.includes('/api/upload/start')) {
        return new Response(JSON.stringify({ id: 'test02', chunks: 1 }), { status: 200 });
      }
      if (u.includes('/chunk/')) {
        return new Response(JSON.stringify({ error: 'unknown upload' }), { status: 404 });
      }
      return w.__origFetch(url, opts);
    };
  });

  await page.evaluate(() => {
    const w = window as Win;
    w.uploadAndAttach(new File([new Uint8Array(8)], 'BROKEN.png', { type: 'image/png' }));
  });

  const chip = page.locator('#peek-attach-bar .peek-attach-chip');
  await expect(chip, 'a failed upload must not silently vanish').toHaveCount(1);
  await expect(chip.locator('.chip-retry'), 'failed chip offers retry').toHaveCount(1);
  await expect(chip.locator('.chip-remove'), 'failed chip offers remove').toHaveCount(1);
});
