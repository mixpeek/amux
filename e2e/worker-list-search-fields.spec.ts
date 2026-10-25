// The worker-list search matches a worker by its name, its description, AND its
// active task (task name + board card id) — Ethan, 2026-10-17: "make sure search
// workers on the list workers page searchers: name, description, active tasks".
// Three workers are injected so each field is the ONLY thing that can match a
// given query; a hit on the right card and a miss on the others proves the field
// is searched, independent of the others.
import { test, expect, Page } from './fixtures';
import { boot } from './lifecycle/evidence';

type Win = Window & { _AMUX_AUTH_TOKEN: string };

const SESSIONS = [
  { name: 'zeph-alpha', dir: '/tmp/a', running: true, status: 'active',
    desc: 'guards the nightly backup pipeline',
    task_name: 'Investigate the billing webhook retry storm', task_board_id: 'ZQ-11',
    runtime_board: { measured: true, status: 'linked', card_id: 'ZQ-11', card_count: 1 } },
  { name: 'zeph-bravo', dir: '/tmp/b', running: true, status: 'active',
    desc: 'owns the onboarding email sequence',
    task_name: 'Draft the quarterly investor update', task_board_id: 'ZQ-22',
    runtime_board: { measured: true, status: 'linked', card_id: 'ZQ-22', card_count: 1 } },
  { name: 'zeph-charlie', dir: '/tmp/c', running: true, status: 'active',
    desc: 'maintains the search indexer',
    task_name: 'Tune the vector recall threshold', task_board_id: 'ZQ-33',
    runtime_board: { measured: true, status: 'linked', card_id: 'ZQ-33', card_count: 1 } },
];

async function inject(page: Page) {
  await page.evaluate((list) => {
    (0, eval)('sessions = ' + JSON.stringify(list));
    (0, eval)('boardItems = ' + JSON.stringify(list.map((s: any) =>
      ({ id: s.task_board_id, title: s.task_name, status: 'doing', session: s.name, archived: false }))));
    (window as any).render();
  }, SESSIONS);
}

async function searchShows(page: Page, q: string) {
  await page.locator('#search-input').fill(q);
  await page.evaluate(() => (window as any).onSearchInput());
  // Which injected workers have a visible card?
  return page.evaluate((names: string[]) => names.filter(n =>
    !!document.querySelector(`.card[data-session="${n}"]`)), SESSIONS.map(s => s.name));
}

test('worker-list search matches name, description and active task', async ({ page }, info) => {
  test.setTimeout(90_000);
  await boot(page);
  await inject(page);

  // NAME: a substring unique to one worker's name.
  expect(await searchShows(page, 'charlie'), 'name match').toEqual(['zeph-charlie']);

  // DESCRIPTION: a word only in one worker's description, in neither name nor task.
  expect(await searchShows(page, 'onboarding'), 'description match').toEqual(['zeph-bravo']);

  // ACTIVE TASK (task name): a word only in one worker's task text.
  expect(await searchShows(page, 'webhook'), 'active-task name match').toEqual(['zeph-alpha']);

  // ACTIVE TASK (board card id): the id shown on the card.
  expect(await searchShows(page, 'zq-33'), 'active-task card id match').toEqual(['zeph-charlie']);

  // All three share the "zeph" prefix — a broad query keeps them all.
  expect((await searchShows(page, 'zeph')).sort(), 'shared prefix keeps all').toEqual(
    ['zeph-alpha', 'zeph-bravo', 'zeph-charlie']);

  // A term in none of the fields matches nothing.
  expect(await searchShows(page, 'nonexistentxyz'), 'no false positives').toEqual([]);
  await page.screenshot({ path: info.outputPath('worker-search.png') });
});
