// The peek's live tail repaints ~3 times a second while a worker runs, and a
// frame usually differs from the last in a line or two (the running timer).
// Replacing #pk-live's innerHTML threw away ~1,400 nodes per frame and the
// desktop app's web view reached 11 GB after a day (Mac memory RCA
// 20261020-165200). The tail is patched in place now: unchanged lines keep
// their nodes, and the result still reads exactly like a fresh render.
import { test, expect } from './fixtures';

test('a live-tail frame patches only the lines that changed', async ({ page }) => {
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekSetLiveHtml === 'function');
  const r = await page.evaluate(() => {
    const w = window as any;
    const frame = (t: number, extra = '') => '<div class="peek-run">'
      + Array.from({ length: 200 }, (_, i) => `<span class="ln" data-i="${i}">line ${i}</span>\n`).join('')
      + `<span class="timer">Working (${t}s)</span>${extra}</div><div class="peek-composer"><b>❯</b> typed</div>`;
    const el = document.createElement('div'); el.id = 'pk-live-test'; document.body.appendChild(el);
    const before = { ...w._peekLiveStats };
    w._peekSetLiveHtml(el, frame(1));
    const keep = el.querySelector('[data-i="100"]'); const run = el.querySelector('.peek-run');
    w._peekSetLiveHtml(el, frame(2));
    const sameNodes = el.querySelector('[data-i="100"]') === keep && el.querySelector('.peek-run') === run;
    const timer = el.querySelector('.timer')!.textContent;
    w._peekSetLiveHtml(el, frame(2));
    w._peekSetLiveHtml(el, frame(3, '<i class="new">tool call</i>'));
    const ref = document.createElement('div'); ref.innerHTML = frame(3, '<i class="new">tool call</i>');
    const grown = el.innerHTML === ref.innerHTML;
    const replacedSmall = w._peekLiveStats.nodesReplaced - before.nodesReplaced;
    w._peekSetLiveHtml(el, '<div class="peek-run"><span>cleared</span></div>');
    const ref2 = document.createElement('div'); ref2.innerHTML = '<div class="peek-run"><span>cleared</span></div>';
    const shrunk = el.innerHTML === ref2.innerHTML;
    const after = w._peekLiveStats;
    el.remove();
    return { sameNodes, timer, grown, shrunk, frames: after.frames - before.frames, unchanged: after.unchanged - before.unchanged,
      replacedSmall };
  });
  expect(r.sameNodes, 'an unchanged line keeps its node across a frame').toBe(true);
  expect(r.timer).toBe('Working (2s)');
  expect(r.grown, 'a frame that adds content matches a fresh render').toBe(true);
  expect(r.shrunk, 'a frame that removes content matches a fresh render').toBe(true);
  expect(r.frames).toBe(5);
  expect(r.unchanged, 'an identical frame touches nothing').toBe(1);
  expect(r.replacedSmall, 'a timer tick and one added line replace a handful of nodes, not 400').toBeLessThan(10);
});

test('the live tick goes through the patch, not an innerHTML swap', async ({ page }) => {
  const src = await (await page.request.get('/app.js')).text();
  expect(src).toContain('{ _peekSetLiveHtml(_liveEl, _lastLiveHTML); _peekReclassifyPrompts(); }');
  expect(src).not.toContain('_liveEl.innerHTML = _lastLiveHTML');
});
