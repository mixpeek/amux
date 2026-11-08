// Owner, 2026-10-29 (peek view "buggy"): "have @mvs-infra verify the 100ms p95
// test irrefutably" showed twice in the worker's Messages: the delivered row,
// with amux's mention note appended and the mention backticked, and a "direct?"
// row of the typed text. The composer sends the text through _expandAtMentions;
// Messages compared the typed text with that and never matched. It also showed
// the owner amux's note as if they had written it.
import { test, expect } from './fixtures';

test('a send with an @mention is one row, shown as typed', async ({ page }) => {
  const now = Date.now();
  const typed = 'have @mvs-infra verify the 100ms p95 test irrefutably';
  const sent = 'have `@mvs-infra` verify the 100ms p95 test irrefutably'
    + '\n\n[amux: @-mentions above are amux workers, NOT files. Reach them via HTTP API, never tmux directly]\n'
    + '  @mvs-infra → POST $AMUX_URL/api/sessions/mvs-infra/send  {"text":"<msg>"}';
  const rows = [{ id: 100638, text: sent, type: 'user', kind: 'human', session: 'mention-echo', ts: now, delivery: 'direct' }];
  await page.route('**/api/history?*', async route => {
    const url = new URL(route.request().url());
    await route.fulfill({ json: url.searchParams.has('counts') ? { all: 1, human: 1 } : rows });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekMsgFetch === 'function' && typeof (window as any).cmdHistoryAdd === 'function');
  const out = await page.evaluate(async ({ typed }) => {
    const w = window as any;
    w.cmdHistoryAdd(typed, { type: 'steering', session: 'mention-echo' });
    const scoped = await w._peekMsgFetch({ level: 'worker', name: 'mention-echo' });
    const merged = w._mergeUnechoed(scoped, 'mention-echo');
    const html = w._cmdHistItemHTML(merged.find((r: any) => r.id === 100638));
    const div = document.createElement('div'); div.innerHTML = html;
    return { rows: merged.length, ids: merged.map((r: any) => r.id || null), shown: div.querySelector('.msg-body')?.textContent || '',
             strip: w._mentionNoteStrip(typed) === typed };
  }, { typed });
  expect(out.rows, 'one row for one send').toBe(1);
  expect(out.ids).toEqual([100638]);
  expect(out.shown, 'shown as typed').toBe(typed);
  expect(out.strip, 'text without the note is left alone').toBe(true);
});
