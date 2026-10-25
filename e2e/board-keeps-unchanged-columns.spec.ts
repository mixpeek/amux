// AMUX-5872: every poll and SSE event re-rendered the whole board, so a column a
// person was scrolling was replaced under them (LC-BOARD-HEADER flaked on CI with
// "Element is not attached to the DOM"). Unchanged columns now keep their nodes.
// Drives the real renderBoard() with the real column renderer.
import {test, expect} from './fixtures';

const card = (id: string, status: string, title = id) =>
  ({id, title, status, owner_type: 'human', session: '', tags: [], updated: 1791700000, created: 1791700000, archived: 0});

test('re-rendering keeps unchanged columns and replaces only the column that changed', async ({page}) => {
  const items = [card('T-1', 'todo'), card('T-2', 'todo'), card('V-1', 'verified'), card('D-1', 'doing')];
  // The live app keeps polling the board. A real read (which has no V-1) that
  // landed during the final scrollIntoViewIfNeeded changed the verified column,
  // replaced it, and detached the node mid-scroll (CI 70cdeb3c3 and 908d87d75:
  // "Element is not attached to the DOM"). Serve the same cards, so a poll is
  // the unchanged re-render this test is about.
  await page.route(/\/api\/board\?archived=0/, r => r.fulfill({json: items}));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).renderBoard === 'function');
  await page.locator('#tab-board').click();
  const r = await page.evaluate(its => {
    const ev = (0, eval);
    ev('boardViewMode = "status"; boardOwnerFilter = "all"; boardSearchQuery = ""');
    ev('boardItems = ' + JSON.stringify(its));
    ev('renderBoard()');
    const col = (k: string) => document.querySelector(`#board-columns .board-col[data-col="${k}"]`)!;
    const v1 = col('verified'), t1 = col('todo'), d1 = col('doing');
    const before = ev('Object.assign({}, _boardMorphStats)');
    // Same data: nothing is replaced.
    ev('renderBoard()');
    const sameKept = col('verified') === v1 && col('todo') === t1 && col('doing') === d1;
    // One card changes in todo: only todo is replaced.
    ev('boardItems = boardItems.map(i => i.id === "T-2" ? Object.assign({}, i, {title: "T-2 renamed"}) : i)');
    ev('renderBoard()');
    const oneChanged = {todoReplaced: col('todo') !== t1, verifiedKept: col('verified') === v1, doingKept: col('doing') === d1,
      renamedShown: col('todo').textContent!.includes('T-2 renamed')};
    const addCols = document.querySelectorAll('#board-columns > .board-add-col-btn').length;
    const after = ev('Object.assign({}, _boardMorphStats)');
    return {sameKept, oneChanged, addCols, morphedDelta: after.morphed - before.morphed, v1Connected: v1.isConnected};
  }, items);
  expect(r.sameKept, 'an unchanged re-render keeps every column node').toBe(true);
  expect(r.oneChanged).toEqual({todoReplaced: true, verifiedKept: true, doingKept: true, renamedShown: true});
  expect(r.addCols, 'the "+ Add column" button is not duplicated').toBe(1);
  expect(r.morphedDelta).toBe(2);
  expect(r.v1Connected).toBe(true);
  // The node a person (or a test) holds survives a re-render, so it can still be used.
  await page.evaluate(() => (0, eval)('renderBoard()'));
  await page.locator('#board-columns .board-col[data-col="verified"]').scrollIntoViewIfNeeded();
});
