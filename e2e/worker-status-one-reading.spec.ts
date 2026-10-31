// One client reading of a worker's status (implicit state machines inventory
// 2026-10-31, #1). The card badge, the status filter/groups and the board's
// is:working / is:waiting used to derive status separately and disagreed:
// - a stopped worker carrying a stale waiting/owner showed "Needs input" on
//   its card while the filter said stopped;
// - a paused worker that is still running filtered as working;
// - a worker whose status is only a guess (agent_state unknown) had no group,
//   so grouping the list threw.
import { test, expect } from './fixtures';

const base = { name: 'one-reading', provider: 'claude', model: 'claude-sonnet-5', running: true, status: 'idle', dir: '/tmp' };

test('the card badge, the filter group and the board predicates agree on every state', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [base] }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._workerStatus === 'function' && typeof (window as any).render === 'function');

  const cases = [
    // The three disagreements, each with what every reader must now say.
    { state: { status: 'waiting', waiting_reason: 'owner', running: false }, group: 'stopped', label: 'stopped', not: 'needs input' },
    { state: { lifecycle: 'paused', running: true, status: 'active' }, group: 'blocked', label: 'pause incomplete' },
    { state: { lifecycle: 'paused', running: false }, group: 'stopped', label: 'paused' },
    { state: { agent_state: 'unknown', status: 'active' }, group: 'unknown', label: 'unknown' },
    // Controls: ordinary states keep their reading.
    { state: { status: 'active' }, group: 'working', label: 'working' },
    { state: { status: 'waiting', waiting_reason: 'owner' }, group: 'waiting', label: '' },
    { state: { status: 'waiting', waiting_reason: 'rate_limit' }, group: 'waiting', label: '' },
    { state: { status: 'blocked', waiting_reason: 'permission_prompt' }, group: 'blocked', label: 'blocked' },
    { state: { status: 'idle' }, group: 'idle', label: 'idle' },
  ];
  for (const c of cases) {
    const w = { ...base, ...c.state };
    const got = await page.evaluate(w => {
      eval('sessions=[' + JSON.stringify(w) + '];');
      const win = window as any;
      win.render();
      return {
        group: win._workerStatus(w).group,
        filterKey: win._sessStatusKey(w),
        attr: document.querySelector('#cards .card[data-session="one-reading"]')?.getAttribute('data-worker-status'),
        badge: win._workerExecutionBadge(w, {}, { inspect: false }).replace(/<[^>]*>/g, ' '),
      };
    }, w);
    expect(got.group, JSON.stringify(c.state)).toBe(c.group);
    expect(got.filterKey, 'the filter reads the same group').toBe(c.group);
    if (c.label) expect(got.badge, JSON.stringify(c.state)).toContain(c.label);
    if (c.not) expect(got.badge).not.toContain(c.not);
    // Paused workers live in the paused section, not in the card list.
    if ((c.state as any).lifecycle) continue;
    expect(got.attr, 'the card carries the same group').toBe(c.group);
    const badge = page.locator('#cards .card[data-session="one-reading"] .status-badge').first();
    if (c.label) await expect(badge, JSON.stringify(c.state)).toContainText(c.label);
    if (c.not) await expect(badge).not.toContainText(c.not);
  }

  // Grouping the list by status with an unknown worker renders its group
  // instead of throwing.
  const errors: string[] = [];
  page.on('pageerror', e => errors.push(e.message));
  const grouped = await page.evaluate(w => {
    eval('sessions=[' + JSON.stringify(w) + ']; layoutMode = "group";');
    (window as any).render();
    return document.querySelectorAll('#cards .card[data-session="one-reading"]').length;
  }, { ...base, agent_state: 'unknown', status: 'active' }).catch(e => { errors.push(String(e)); return -1; });
  expect(errors, errors.join('\n')).toEqual([]);
  expect(grouped).toBe(1);
});

// Queue mode sends straight to a worker waiting at a question only on a fresh
// reading; a stale "waiting" keeps the message queued (#36).
test('queue mode trusts only a fresh waiting status', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({ json: [base] }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._waitingNow === 'function');
  const read = (ageMs: number, status: string) => page.evaluate(([ageMs, status]) => {
    eval('sessions=[' + JSON.stringify({ name: 'one-reading', running: true, status }) + ']; _lastDataTime = Date.now() - ' + ageMs + ';');
    return (window as any)._waitingNow('one-reading');
  }, [ageMs, status] as [number, string]);
  expect(await read(1000, 'waiting'), 'fresh waiting: send to answer it').toBe(true);
  expect(await read(30000, 'waiting'), 'stale waiting: keep it queued').toBe(false);
  expect(await read(1000, 'active'), 'not waiting').toBe(false);
});
