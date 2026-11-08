// The full worker lifecycle, end to end, through the surfaces a person uses.
//
// This owns the whole journey that the narrower renderer and API suites do
// not: create a named-model worker, persist its first human prompt, watch its
// terminal, enforce every resolved column gate, and delete it through the
// dashboard's guarded confirmation. Every fixture is self-cleaning.
import type { Page } from '@playwright/test';
// Use the shared fixture so isolated runs exercise candidate dashboard assets,
// not whichever bundle the installed API binary happens to contain.
import { expect } from './fixtures';
import { test } from './worker-lifecycle-fixture';
import { getSessionsResilient } from './lifecycle/evidence';

async function appToken(page: Page): Promise<string> {
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  expect(token, 'served bootstrap must provide the API token').toBeTruthy();
  return token;
}

for (const modelFamily of ['sonnet', 'haiku']) {
  test(`${modelFamily} worker goes create → run → prompt → peek → delete, and the board gates hold`, async ({ page, request, workerCleanup }, testInfo) => {
    // First-run boot is deliberately part of this journey. WebKit under the
    // full six-worker matrix once reached guarded delete at 30.1s, so the
    // generic 30s whole-test limit would make host load the verdict. Every
    // product wait below remains separately bounded.
    test.setTimeout(90_000);
    const token = await appToken(page);
    const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
    const uiToken = await page.evaluate(() => (window as any)._AMUX_UI_TOKEN as string);
    expect(uiToken, 'served bootstrap must provide the human-only destructive guard').toBeTruthy();
    const worker = `e2e-life-${modelFamily}-${testInfo.project.name}-${Date.now()}`;
    const prompt = `Lifecycle ${modelFamily} delivery probe ${worker}`;
    const workerAuth = { ...auth, 'X-Amux-Worker': worker };
    let card = '';
    Object.assign(workerCleanup, { worker, auth, uiToken });

    {
      // ── CREATE, through the real dialog, on a named model ────────────────
      await page.addInitScript(() => {
        try { localStorage.setItem('amux_walkthrough_done', '1'); } catch (e) {}
      });
      await page.goto('/');
      await page.waitForFunction(() => typeof (window as any).fetchSessions === 'function');
      await page.click('#add-btn');
      await page.locator('text=New worker').first().click();
      await page.waitForSelector('#create-name', { state: 'visible' });
      await expect.poll(async () => page.evaluate(
        () => (document.getElementById('create-model') as HTMLSelectElement).options.length)).toBeGreaterThan(1);
      const namedModel = await page.evaluate((family) =>
        [...(document.getElementById('create-model') as HTMLSelectElement).options]
          .map(option => option.value)
          .find(value => new RegExp(family, 'i').test(value)) || '', modelFamily);
      expect(namedModel, `a ${modelFamily} model must be offered in the create dialog`).toBeTruthy();
      await page.fill('#create-name', worker);
      await page.fill('#create-dir', process.env.AMUX_E2E_DIR || '/tmp');
      await page.fill('#create-prompt', prompt);
      await page.selectOption('#create-model', namedModel);
      // A harness-driven worker: isolated workers never have a board, and the
      // opt-in board below is about the ones that can.
      await page.locator('#create-isolated').uncheck();
      const [created] = await Promise.all([
        page.waitForResponse(response => new URL(response.url()).pathname === '/api/sessions'
          && response.request().method() === 'POST'),
        page.locator('#create-overlay button.primary:has-text("Create")').click(),
      ]);
      expect(created.ok(), `worker creation must succeed (HTTP ${created.status()})`).toBe(true);

      await expect.poll(async () =>
        (await request.get(`/api/sessions/${worker}`, { headers: auth })).status(),
      { timeout: 30_000 }).toBe(200);
      const cfg = await (await request.get(`/api/sessions/${worker}`, { headers: auth })).json();
      expect(cfg.flags || '', 'the selected model must reach the launch flags').toContain(modelFamily);
      // The create-time prompt is queued durably (steering_queue) BEFORE the
      // start and delivered once the worker boots, so it is attributed to this
      // worker in whichever of the three stages it has reached: still queued,
      // delivered by steering, or in message history.
      await expect.poll(async () => {
        const enc = encodeURIComponent(worker);
        const [history, queued, steered] = await Promise.all([
          request.get(`/api/history?session=${enc}&limit=20`, { headers: auth }),
          request.get(`/api/sessions/${enc}/steer`, { headers: auth }),
          request.get(`/api/sessions/${enc}/steer?history=1`, { headers: auth }),
        ].map(async r => JSON.stringify(await (await r).json())));
        return history + queued + steered;
      }, {
        message: 'the create-time prompt must be durably attributed to this worker',
        timeout: 30_000,
      }).toContain(prompt);
      // ── NO BOARD UNTIL SOMEONE TURNS IT ON (Ethan, 2026-11-02: "the board
      // is just getting in the models way") ─────────────────────────────────
      // A new worker gets its prompt as plain text. Nothing files it as a card,
      // its own board calls are refused, and its Board switch reads Off.
      const boardUsed = async () => {
        const rows = await getSessionsResilient(request, auth);
        return (await rows.json()).find((s: any) => s.name === worker)?.board_used;
      };
      expect(await boardUsed(), 'a new worker has no board until someone turns it on').toBe(false);
      const noBoard = await request.post('/api/board', {
        headers: workerAuth,
        data: { title: `[e2e] card before the board is on ${worker}`, session: worker, status: 'backlog' },
      });
      expect(noBoard.status(), 'a worker with no board is refused on its own board').toBe(403);
      expect((await noBoard.json()).code).toBe('board_off');
      const filed = await (await request.get(
        `/api/board?all=1&session=${encodeURIComponent(worker)}`, { headers: auth })).json();
      expect(filed.filter((c: any) => c.session === worker).map((c: any) => c.title),
        'the create prompt must not be filed as a card').toEqual([]);
      // The owner turns it on with the worker's one Board switch.
      await page.evaluate(name => (window as any).openPeek(name), worker);
      await page.locator('#peek-tab-issues').click();
      const boardSwitch = page.locator('#peek-board-config .board-one-switch');
      await expect(boardSwitch).toHaveText('Off');
      await Promise.all([
        page.waitForResponse(r => r.url().includes(`/api/sessions/${encodeURIComponent(worker)}/config`)
          && r.request().method() === 'PATCH'),
        boardSwitch.click(),
      ]);
      await expect.poll(boardUsed, { timeout: 20_000 }).toBe(true);
      await expect(boardSwitch).toHaveText('On');
      await page.evaluate(() => (window as any).closePeek());

      // Explicit negative-control invocation: fail after the real create/prompt
      // boundary and require teardown to retain this error and remove the worker.
      if (process.env.AMUX_E2E_LIFECYCLE_FAIL_AFTER_CREATE === '1') {
        throw new Error(`INJECTED_LIFECYCLE_FAILURE_AFTER_CREATE ${worker}`);
      }

      // A throwaway AMUX_HOME refuses to spawn tmux (AMUX-4724) unless the
      // server has a private tmux and fake agents, which the shared golden
      // servers deliberately do not (live fake workers left running for the
      // rest of the suite degraded the mobile server until every later test's
      // page.goto timed out). Everything above holds either way; the running
      // half needs a started worker, and says so rather than failing.
      // Worker launch end to end is covered by e2e/chaos (fake agents, one
      // private server per test).
      const running = await expect.poll(async () =>
        (await (await request.get(`/api/sessions/${worker}`, { headers: auth })).json()).running === true,
      { timeout: 10_000 }).toBe(true).then(() => true, () => false);
      test.skip(!running, 'this server does not spawn workers from a throwaway AMUX_HOME (AMUX-4724); the create and durable-attribution half ran');

      // ── VISIBLE WORKER AND STABLE TERMINAL ──────────────────────────────
      await page.reload();
      await page.waitForFunction(() => typeof (window as any).fetchSessions === 'function');
      // The receipt panel also contains this name, including while collapsed.
      // Assert the actual worker card, not the first matching text in the page.
      await expect(page.locator(`.card[data-session="${worker}"] .card-name`)).toBeVisible({ timeout: 30_000 });
      await page.evaluate(name => (window as any).openPeek(name), worker);
      await expect.poll(async () => page.evaluate(
        () => (document.getElementById('peek-body') as HTMLElement).textContent!.length),
      { timeout: 30_000 }).toBeGreaterThan(0);
      expect(await page.locator('.peek-render-chunk, .peek-code-row, .peek-code-split').count(),
        'the reverted input-chunk parser and inferred split diff must remain absent').toBe(0);
      expect(await page.locator('.peek-output-controls .scroll-lock-badge').count(),
        'the new-output badge must not enter toolbar layout flow').toBe(0);

      const geom = async () => page.evaluate(() => {
        const rect = (selector: string) => {
          const r = document.querySelector(selector)?.getBoundingClientRect();
          return r ? `${Math.round(r.top)}x${Math.round(r.height)}` : '-';
        };
        const body = document.getElementById('peek-body')!.getBoundingClientRect();
        return {
          body: rect('#peek-body'),
          header: rect('#peek-overlay > .overlay-header'),
          tabs: rect('#peek-overlay > .peek-tabs'),
          dir: rect('#peek-overlay > .peek-dir-bar'),
          plan: rect('#peek-plan'),
          status: rect('#peek-status'),
          cmd: rect('.peek-cmd-bar'),
          chips: rect('#peek-chips'),
          classes: document.getElementById('peek-overlay')!.className,
          layout: [...document.querySelectorAll('#peek-overlay *')]
            .filter(el => el.id || el.classList.contains('overlay-header'))
            .map(el => ({ name: el.id || el.className,
              top: Math.round(el.getBoundingClientRect().top),
              height: Math.round(el.getBoundingClientRect().height) }))
            .filter(el => el.height > 0 && el.top <= body.top),
        };
      });
      const samples = [];
      for (let i = 0; i < 8; i++) {
        samples.push(await geom());
        await page.waitForTimeout(400);
      }
      await testInfo.attach('terminal-layout-samples', {
        body: JSON.stringify(samples), contentType: 'application/json',
      });
      const seen = new Set(samples.map(sample => sample.body));
      if (seen.size > 1) console.log('TERMINAL_LAYOUT_SAMPLES', JSON.stringify(samples));
      expect([...seen], `the terminal box must hold still while output streams\n${JSON.stringify(samples, null, 2)}`)
        .toHaveLength(1);
      await page.screenshot({ path: testInfo.outputPath('worker-terminal.png') });
      await page.evaluate(() => (window as any).closePeek());

      // ── OWN-BOARD BOUNDARY AND RESOLVED COLUMN GATES ───────────────────
      const foreign = await request.post('/api/board', {
        headers: workerAuth,
        data: { title: `[e2e] forbidden foreign card ${worker}`,
          session: `${worker}-peer`, status: 'backlog' },
      });
      expect(foreign.status(), 'a worker must not create on another worker\'s board').toBe(403);
      expect((await foreign.json()).code).toBe('cross_board_create_forbidden');

      const made = await request.post('/api/board', {
        headers: workerAuth,
        data: { title: `[e2e] lifecycle gate probe ${worker}`, session: worker, status: 'backlog' },
      });
      expect(made.ok(), 'backlog must always accept a card').toBeTruthy();
      card = (await made.json()).id;
      workerCleanup.card = card;

      const statuses = await (await request.get('/api/board/statuses', { headers: auth })).json();
      const contract = await (await request.get(
        `/api/board/contract?card=${card}`, { headers: auth })).json();
      expect(contract.card_effective_gates?.card).toBe(card);
      const resolvedGates = contract.card_effective_gates.gates as Record<string, string[]>;
      const gatedColumns = statuses.map((column: any) => ({
        ...column,
        criteria: Array.isArray(resolvedGates[column.id]) && resolvedGates[column.id].length
          ? resolvedGates[column.id]
          : (Array.isArray(column.gate) ? column.gate : []),
      })).filter((column: any) => column.criteria.length > 0);
      for (const status of ['review', 'done', 'verified']) {
        expect(gatedColumns.find((column: any) => column.id === status)?.criteria.length,
          `${status} must expose a resolved gate`).toBeGreaterThan(0);
      }
      const move = (data: object) => request.patch(
        `/api/board/${card}`, { headers: workerAuth, data });
      const detail = async () =>
        (await (await request.get(`/api/board/${card}`, { headers: auth })).json());
      const statusOf = async () => (await detail()).status;

      const reassign = await move({ session: `${worker}-peer` });
      expect(reassign.status(), 'a worker must not move its card onto another worker\'s board').toBe(403);
      expect((await reassign.json()).code).toBe('cross_board_reassignment_forbidden');

      const done = gatedColumns.find((column: any) => column.id === 'done');
      const noAsset = await move({ status: 'done', gate_checked: done.criteria });
      expect(noAsset.status(), 'done must refuse a card that names no artifact').toBe(409);
      expect((await noAsset.json()).code).toBe('done_requires_asset_link');
      const artifact = 'e2e/worker-lifecycle.spec.ts';
      expect((await move({ desc_append: `\nArtifact: ${artifact}` })).ok(),
        'the lifecycle spec must be linked as the produced artifact').toBeTruthy();
      const noEvidence = await move({ status: 'done', gate_checked: done.criteria });
      expect(noEvidence.status(), 'an artifact link alone remains a plan, not execution evidence').toBe(409);
      expect((await noEvidence.json()).code).toBe('done_requires_evidence');
      const evidence = `ran \`npx playwright test -c e2e/playwright.config.ts ${artifact} --project=${testInfo.project.name}\`; browser exercised creation, ${modelFamily} selection, prompt persistence, stable terminal geometry, and ownership refusals`;
      expect((await move({ evidence })).ok(), 'actual browser evidence must persist on the card').toBeTruthy();

      for (const column of gatedColumns) {
        const gate = column.criteria;
        const bare = await move({ status: column.id });
        expect(bare.status(), `${column.id} must refuse a move that acknowledges nothing`).toBe(409);
        const why = await bare.json();
        expect(why.gate, `${column.id}'s contract and refusal must agree`).toEqual(gate);
        expect(JSON.stringify(why), `${column.id}'s refusal must explain how to comply`)
          .toMatch(/cli|how_to_ack|how_to_fix/);
        expect(await statusOf(), `${column.id} must not have moved`).not.toBe(column.id);

        const fake = await move({ status: column.id, gate_checked: ['not a real criterion'] });
        expect(fake.status(), `${column.id} must refuse a fabricated acknowledgement`).toBe(409);
        expect((await fake.json()).gate).toEqual(gate);
        if (gate.length > 1) {
          const partial = await move({ status: column.id, gate_checked: [gate[0]] });
          expect(partial.status(), `${column.id} must refuse a partial acknowledgement`).toBe(409);
          expect((await partial.json()).gate).toEqual(gate);
        }
        expect(await statusOf(), `${column.id} must still not have moved`).not.toBe(column.id);
      }

      const review = gatedColumns.find((column: any) => column.id === 'review');
      const reviewer = `${worker}-peer`;
      const reviewAck = await move({ status: 'review', reviewer, gate_checked: review.criteria });
      const reviewBody = await reviewAck.json();
      expect(reviewAck.ok(), `exact review acknowledgement must pass: ${JSON.stringify(reviewBody)}`)
        .toBeTruthy();
      expect(await statusOf()).toBe('review');
      expect((await detail()).session, 'peer review must not transfer board ownership').toBe(worker);
      expect((await detail()).reviewer, 'the peer must remain linked as reviewer').toBe(reviewer);
      const doneAck = await move({ status: 'done', gate_checked: done.criteria });
      const doneBody = await doneAck.json();
      expect(doneAck.ok(), `done must accept exact gate + artifact + evidence: ${JSON.stringify(doneBody)}`)
        .toBeTruthy();
      expect(await statusOf()).toBe('done');

      const verified = gatedColumns.find((column: any) => column.id === 'verified');
      const blanket = await move({ status: 'verified', gate_ack: true });
      expect(blanket.status(), 'verified must reject the blanket gate-ack backdoor').toBe(409);
      expect((await blanket.json()).code).toBe('verified_requires_gate_checked');
      const verifiedAck = await move({ status: 'verified', gate_checked: verified.criteria });
      expect(verifiedAck.ok(), 'verified must accept every exact criterion after review and done evidence')
        .toBeTruthy();
      expect(await statusOf()).toBe('verified');

      // ── DELETE THROUGH THE ACTUAL HUMAN-GUARDED DASHBOARD FLOW ──────────
      const bareDelete = await request.delete(`/api/sessions/${worker}`, { headers: auth });
      expect(bareDelete.status(), 'API calls without the dashboard UI token cannot delete workers').toBe(403);
      const deletion = page.evaluate(name => (window as any).deleteSession(name), worker);
      await expect(page.locator('#modal-msg')).toHaveText(`Delete worker "${worker}"?`);
      const [deleted] = await Promise.all([
        page.waitForResponse(response =>
          new URL(response.url()).pathname === `/api/sessions/${worker}/delete`
          && response.request().method() === 'POST'),
        page.locator('#modal-btns button.danger').click(),
      ]);
      expect(deleted.ok(), `confirmed dashboard deletion must succeed (HTTP ${deleted.status()})`).toBeTruthy();
      await deletion;
      await expect.poll(async () =>
        (await request.get(`/api/sessions/${worker}`, { headers: auth })).status(),
      { timeout: 30_000 }).toBe(404);

      await page.reload();
      await page.waitForFunction(() => typeof (window as any).fetchSessions === 'function');
      // Startup renders the durable offline cache before the first list read.
      // A function definition and an absent modal cannot prove deletion.
      await page.evaluate(() => (window as any).fetchSessions());
      const deletedCard = page.locator(`.card[data-session="${worker}"]`);
      // A soft assertion preserves this failure if a later diagnostic read fails.
      await expect.soft(deletedCard, 'deleted worker must disappear from the refreshed fleet').toHaveCount(0);
      const list = await request.get('/api/sessions', { headers: auth, timeout: 5000 });
      const workers = list.ok() ? await list.json() : null;
      const measured = Array.isArray(workers);
      const deletionEvidence = {
        kind: 'e2e-worker-deletion-view', measured, n_considered: measured ? 1 : 0, worker,
        visible_cards: await deletedCard.count(), list_status: list.status(),
        list_contains_worker: measured ? workers.some((s: any) => s.name === worker) : null,
        why_unmeasured: measured ? null : 'Session list read did not return an array',
      };
      console.log(JSON.stringify(deletionEvidence));
      await testInfo.attach('worker-deletion-view', { body: JSON.stringify(deletionEvidence), contentType: 'application/json' });
      await page.screenshot({ path: testInfo.outputPath('worker-deleted.png') });
      const beacon = await request.post('/api/client-debug', { headers: auth, data: deletionEvidence, timeout: 5000 });
      expect(beacon.ok(), 'deletion visibility diagnostic must reach amux logs').toBeTruthy();
      expect(deletionEvidence.visible_cards, 'final measured view must exclude the deleted worker').toBe(0);
      expect(measured, 'deletion evidence requires a successful array list read').toBe(true);
      expect(deletionEvidence.list_contains_worker, 'deleted worker must be absent from the API list').toBe(false);
      await expect(page.locator('text=/Unsaved changes/')).toHaveCount(0);
    }
  });
}
