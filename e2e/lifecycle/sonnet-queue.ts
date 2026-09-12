import { lifecyclePrefix, expectLifecycleWorker } from './provider';
import { test, expect, Page, APIRequestContext, TestInfo } from '@playwright/test';
import { boot, auth, checkpoint, getSessionsResilient } from './evidence';

// Seed actual board work, then observe. There is deliberately no /send, claim,
// status PATCH, evidence write or completion call from the test driver.
export async function runSonnetQueue({ page, request }: { page: Page, request: APIRequestContext }, info: TestInfo) {
  test.setTimeout(1_500_000);
  expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
  const run = process.env.AMUX_LIFECYCLE_PAIR_RUN!;
  const cwd = process.env.AMUX_LIFECYCLE_LAB_WORKSPACE!;
  expect(run.startsWith(lifecyclePrefix)).toBe(true);
  expect(cwd).toBeTruthy();
  const observeOnly = process.env.AMUX_LIFECYCLE_QUEUE_OBSERVE === '1';
  const health = await (await request.get('/health')).json();
  await boot(page);
  const headers = await auth(page);
  const names = [`${run}-author`, `${run}-reviewer`];
  const roster = await getSessionsResilient(request, headers);
  expect(roster.ok()).toBe(true);
  const rows = await roster.json();
  for (const name of names) {
    const row = rows.find((r: any) => r.name === name);
    expectLifecycleWorker(row);
    expect(row?.auto_drain_backlog, 'default backlog dispatch must be enabled').toBe(true);
    expect(row?.auto_pickup, 'default todo dispatch must be enabled').toBe(true);
  }
  const seeded: any[] = [], timeline: any[] = [], quietCycles: any[] = [];
  for (const [index, name] of names.entries()) {
    let previous: any;
    // Backlog prerequisite -> dependent Todo on author; Todo prerequisite ->
    // dependent Backlog on reviewer. Both owners must exercise both sources.
    for (const [order, status] of (index === 0 ? ['backlog', 'todo'] : ['todo', 'backlog']).entries()) {
      const title = `${run} queue ${index}-${order} from ${status}`;
      const file = `${run}-queue-${index}-${order}.json`;
      const value = (index + 2) * (order + 7);
      let card: any;
      if (observeOnly) {
        const board = await request.get(`/api/board?session=${name}&done_limit=0`, { headers });
        expect(board.ok()).toBe(true);
        card = (await board.json()).find((c: any) => c.title === title);
        expect(card, 'observation requires the original queued card').toBeTruthy();
      } else {
        const response = await request.post('/api/board', { headers, data: {
          title, session: name, owner_type: 'agent', type: 'chore', status,
          depends_on: previous ? [previous.id] : [],
          desc: `Authorized lifecycle queue task. Work only in ${cwd}. This is a real chore already assigned to you: work this exact card, do not create a replacement. It starts in ${status}; the board driver must pick it up without a chat prompt. Read this card and its prerequisites. ${previous ? `Read completed prerequisite ${previous.id} and its artifact ${previous.file} before proceeding.` : ''} Compute (${index + 2}) * (${order + 7}) using a real shell command. Write ${file} as JSON with task_id (this exact card ID), worker (your name), initial_status "${status}", result (the computed number), and prerequisite_task_id (${previous ? `"${previous.id}"` : 'null'}). Run a real command validating the JSON and record the command, result and artifact path in this card's evidence. Finish this card through the normal chore flow and continue ready work. Before idling, list your entire own board including backlog, not only active cards: fold/discard your own captured FYI duplicates into the real completed work with individual truthful reasons. Leave peer cards untouched. No production/external contact, no human reminders, no bypassing gates.`,
        } });
        expect(response.status(), await response.text()).toBe(201);
        card = await response.json();
        expect(card.status).toBe(status);
      }
      const entry = { id: card.id, name, title, file, initialStatus: status, value, prerequisite: previous?.id || null };
      seeded.push(entry); previous = entry;
    }
  }
  let details: any[] = [], allCards: any[] = [];
  try {
    await expect.poll(async () => {
      details = []; allCards = [];
      for (const fixture of seeded) {
        const response = await request.get(`/api/board/${fixture.id}`, { headers });
        if (!response.ok()) return false;
        details.push(await response.json());
      }
      timeline.push({ at: new Date().toISOString(), cards: details.map(c => ({ id: c.id, status: c.status, log: c.log })) });
      if (!details.every(c => ['done', 'verified'].includes(c.status))) return false;
      for (const name of names) {
        const response = await request.get(`/api/board?session=${name}&done_limit=0`, { headers });
        if (!response.ok()) return false;
        allCards.push(...await response.json());
      }
      return allCards.every(c => ['done', 'verified', 'discarded', 'cancelled'].includes(c.status));
    }, { timeout: 1_050_000, intervals: [5000, 15000, 30000], message: 'Sonnet workers must autonomously pick up backlog/todo and finish every queued task and remaining own capture' }).toBe(true);
    for (const fixture of seeded) {
      const detail = details.find(c => c.id === fixture.id);
      expect(detail.session).toBe(fixture.name);
      expect(String(detail.evidence)).toContain(fixture.file);
      const file = await request.get(`/api/fs/read?path=${encodeURIComponent(`${cwd}/${fixture.file}`)}`, { headers });
      expect(file.ok()).toBe(true);
      const receipt = JSON.parse((await file.json()).content);
      expect(receipt).toMatchObject({ task_id: fixture.id, worker: fixture.name,
        initial_status: fixture.initialStatus, result: fixture.value, prerequisite_task_id: fixture.prerequisite });
      await info.attach(`queue-receipt-${fixture.id}`, { body: JSON.stringify(receipt), contentType: 'application/json' });
      for (const size of [{ width: 1280, height: 800 }, { width: 375, height: 667 }]) {
        await page.setViewportSize(size);
        await page.goto(`/#issue=${fixture.id}`);
        await expect(page.locator('#bd-key')).toHaveText(fixture.id);
        await checkpoint(page, info, `queue-complete-${fixture.id}-${size.width}`);
      }
    }
    // Completion must remain quiet across real driver cycles, not just one
    // lucky snapshot between acknowledgement-created tasks. Read only here.
    const baselineIds = allCards.map(c => c.id).sort();
    const initialDrive = await (await request.get('/api/debug/board-drive', {headers})).json();
    expect(initialDrive.loop_running, 'a stopped driver cannot prove quiet cycles').toBe(true);
    const firstTick = initialDrive.last.tick;
    await expect.poll(async () => {
      const drive = await (await request.get('/api/debug/board-drive', {headers})).json();
      const current: any[] = [];
      for (const name of names) {
        const response = await request.get(`/api/board?session=${name}&done_limit=0`, {headers});
        expect(response.ok()).toBe(true); current.push(...await response.json());
      }
      quietCycles.push({tick:drive.last?.tick, cards:current.map(c=>({id:c.id,status:c.status}))});
      expect(current.map(c=>c.id).sort(), 'completed work must not create acknowledgement tasks').toEqual(baselineIds);
      expect(current.every(c=>['done','verified','discarded','cancelled'].includes(c.status)), 'original terminal work must remain settled').toBe(true);
      return (drive.last?.tick || 0) - firstTick;
    }, {timeout:240_000, intervals:[5000], message:'observe three actual quiet board-driver cycles'}).toBeGreaterThanOrEqual(3);
    expect((await (await request.get('/health')).json()).build).toBe(health.build);
  } finally {
    const debug = await request.get('/api/debug/board-drive', { headers });
    await info.attach('queue-pickup-proof', { body: JSON.stringify({ run, observeOnly,
      intervention: 'none after board creation', health, seeded, timeline, quietCycles, details, allCards,
      boardDrive: debug.ok() ? await debug.json() : { status: debug.status() } }, null, 2), contentType: 'application/json' });
  }
}
