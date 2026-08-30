import { test, expect } from './fixtures';
test('read-aloud plays through the shared bottom player', async ({ page }) => {
  await page.route('**/api/tts', r => r.fulfill({ contentType: 'application/json',
    body: JSON.stringify({ engine: 'stub', size: 44,
      url: 'data:audio/wav;base64,UklGRiQAAABXQVZFZm10IBAAAAABAAEARKwAAIhYAQACABAAZGF0YQAAAAA=' }) }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._ttsSpeak === 'function', { timeout: 20000 });
  const before = await page.evaluate(() =>
    document.querySelector('#amux-audio-bar,.audio-bar,[id*="audio"][class*="bar"]')?.className || 'NO BAR ELEMENT');
  console.log('[BAR BEFORE] ' + before);

  // Register a one-shot click listener that calls _ttsSpeak from inside the
  // gesture, then click to fire it. WebKit (iOS) rejects audio.play() that is
  // not called inside a user-activation stack; calling _ttsSpeak via a bare
  // page.evaluate has no activation, so _ttsClaimGesture() fails silently and
  // the audio element is left in a state where _abPlay cannot make the bar
  // visible. (Same class as e955a72 — the production fix pre-claims the element;
  // the test fix is to supply the activation the test was missing.)
  //
  // Pattern: start the evaluate WITHOUT awaiting it (so the listener is
  // registered), then click to provide activation and trigger the handler,
  // then await the evaluate's resolution.
  // NO CLICK, NO AWAIT ON PLAYBACK. Two independent CI hangs live here and this
  // removes both dependencies rather than picking between the diagnoses:
  //   - amux-frustrations: `_ttsSpeak` awaits `audio.play()`, which never settles
  //     on a headless runner with no audio sink (desktop has one; [mobile] and
  //     [ios-safari] do not — exactly the two projects that failed).
  //   - amux-homepage: `page.click('body')` does not reliably reach a document
  //     listener on CI mobile/webkit; the app absorbs/re-targets the click, so the
  //     listener never fires and the awaited promise never resolves.
  // Either one produces the same 30s `page.evaluate` timeout, and a local run
  // cannot reproduce either — the environment IS the discriminator.
  //
  // The load-bearing fact (amux-homepage, app.js:10741): _abPlay marks the bar
  // visible SYNCHRONOUSLY, before any audio.play(). So neither a real gesture nor
  // a working audio device is needed for the property under test. The gesture
  // dance exists only because _ttsClaimGesture itself calls play(); that rejects
  // harmlessly here and is caught.
  //
  // Calling _ttsSpeak (not _abPlay) on purpose: the claim is "read-aloud routes
  // through the SHARED bottom player", so the _ttsSpeak -> _abPlay wiring is the
  // subject. Invoking _abPlay directly would be more robust and would stop
  // testing the thing that was actually broken — read-aloud playing on its own
  // detached Audio element. Real user-activation is covered by the product path
  // in prod, not by this assertion.
  page.evaluate(() => {
    try { (window as any)._ttsSpeak('hello from the sweep', null); } catch (e) { /* asserted via state below */ }
  }).catch(() => {});

  // Poll instead of sleeping — a fixed 600ms can expire before the async chain
  // resolves on slow CI machines, and on fast machines it wastes wall-clock time.
  // `(0,eval)`, NOT `window._abEls`. _abEls is a top-level `const` in a classic
  // script (app.js:10696), so it is a global LEXICAL binding and is NOT a
  // property of window — `window._abEls` is undefined, the predicate can never
  // become true, and this poll times out at 30s regardless of whether the bar
  // appeared. The two assertions 6 lines below already use the eval form; the
  // poll was the odd one out.
  //
  // Third instance of this trap in two days (boardItems, _bwWantFrame, now
  // _abEls) and it fails the same way every time: silently, as a timeout that
  // reads like the feature is broken rather than like the probe cannot see it.
  await page.waitForFunction(
    () => {
      try { return !!(0, eval)('_abEls')?.bar?.classList.contains('visible'); }
      catch { return false; }
    },
    { timeout: 5000 }
  );

  const r = await page.evaluate(() => {
    const a = (0, eval)('_abAudio');
    const bar = (0, eval)('_abEls').bar;
    return { barVisible: bar.classList.contains('visible'),
             sharedElementUsed: (0, eval)('_ttsSpeakAudio') === a,
             srcIsClip: (a.src || '').startsWith('data:audio/wav'),
             title: (0, eval)('_abEls').title.textContent };
  });
  console.log('[AFTER] ' + JSON.stringify(r));
  expect(r.barVisible, 'bottom player bar did not become visible').toBe(true);
  expect(r.sharedElementUsed, 'played on a detached element, not the shared player').toBe(true);
  expect(r.title, 'player title should name the feature').toMatch(/Read aloud/);
});
