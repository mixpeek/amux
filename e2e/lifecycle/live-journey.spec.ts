import { lifecycleProvider, expectLifecycleWorker, selectLifecycleProvider } from './provider';
import { test, expect } from '@playwright/test';
import { boot, auth, checkpoint } from './evidence';

// Requires a dedicated lab with real provider auth and the normal board driver.
// Never point at a shared fleet. Only GETs after submitting the initial prompt:
// the observer cannot manufacture progress, attach evidence, or close the tasks.
test('LC-LIVE: a new worker changes code, tests it, and drives its tasks to evidenced completion', async ({ page, request }, info) => {
  const lab = process.env.AMUX_LIFECYCLE_LAB_URL;
  const cwd = process.env.AMUX_LIFECYCLE_LAB_WORKSPACE;
  expect(lab, 'dedicated lab URL is required; unavailable is not a pass').toBeTruthy();
  expect(cwd, 'empty, dedicated lab workspace is required').toBeTruthy();
  expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
  await boot(page);
  const headers = await auth(page);
  const before = await request.get('/health');
  expect(before.ok()).toBeTruthy();
  const health = await before.json();
  const name = `lifecycle-${Date.now()}`;
  const marker = `${name}-complete`;
  const prompt = `Work only in ${cwd}. This is an acceptance test with three deliverables.
Create three separate board tasks assigned to your own worker; link their dependencies.
1. Implement sum.mjs exporting sum(a,b), with a meaningful automated test covering positive,
negative and zero inputs. 2. Run the tests, then change sum.mjs to reject non-finite inputs
and extend and rerun the tests. 3. Produce result.html visibly showing ${marker} and a short
summary of the actual changes and test results; also write result.md with those results.
Drive every task through its configured lifecycle and gates, attach real artifact paths,
and store the exact test command and result in each task's evidence. Do not bypass gates.
Do not send email or peer messages or work outside this scratch workspace. Do not claim a
test passed without running it. Finish with every deliverable task in done or verified.`;
  await page.locator('#tab-sessions').click();
  await page.locator('[onclick*="toggleAddMenu"]').click();
  await page.locator('.card-menu-item', { hasText: 'New worker' }).click();
  await page.locator('#create-name').fill(name);
  await page.locator('#create-dir').fill(cwd!);
  await selectLifecycleProvider(page);
  await page.locator('#create-prompt').fill(prompt);
  await checkpoint(page, info, 'live-01-worker-and-prompt');
  await page.locator('#create-overlay').getByRole('button', { name: 'Create', exact: true }).click();
  await expect(page.locator('#create-overlay')).not.toHaveClass(/active/, { timeout: 60_000 });
  const roster = await request.get('/api/sessions', { headers });
  expect(roster.ok()).toBeTruthy();
  expectLifecycleWorker((await roster.json()).find((row: any) => row.name === name));
  const samples: any[] = [];
  let cards: any[] = [];
  try {
    await expect.poll(async () => {
      const response = await request.get(`/api/board?session=${encodeURIComponent(name)}&done_limit=0`, { headers });
      expect(response.ok()).toBeTruthy();
      const rows = await response.json();
      cards = await Promise.all(rows.map(async (row: any) => {
        const detail = await request.get(`/api/board/${encodeURIComponent(row.id)}`, { headers });
        expect(detail.ok()).toBeTruthy();
        return detail.json();
      }));
      samples.push({ at: new Date().toISOString(), cards });
      await page.goto('/#view=board');
      await page.locator('#tab-board').click();
      await page.screenshot({ path: info.outputPath(`progress-${samples.length}.png`), fullPage: true });
      const deliverables = cards.filter(row => !['epic', 'prompt'].includes(row.type));
      return deliverables.length >= 3 && cards.every(row => ['done', 'verified'].includes(row.status));
    }, { timeout: 1_050_000, intervals: [5000, 15000, 30000], message: 'worker must finish its own tasks, without observer intervention' }).toBe(true);
    for (const card of cards) {
      expect(String(card.evidence || '').length, `${card.id} must carry command/result evidence`).toBeGreaterThan(20);
      await page.goto(`/#issue=${encodeURIComponent(card.id)}`);
      await expect(page.locator('#bd-key')).toHaveText(card.id);
      await checkpoint(page, info, `live-final-${card.id}`);
    }
    for (const file of ['sum.mjs', 'result.html', 'result.md']) {
      const response = await request.get(`/api/fs/read?path=${encodeURIComponent(`${cwd}/${file}`)}`, { headers });
      expect(response.ok(), `${file} must exist`).toBeTruthy();
      const body = await response.json();
      await info.attach(file, { body: JSON.stringify(body, null, 2), contentType: 'application/json' });
      if (file === 'sum.mjs') {
        const sandbox = await page.context().browser()!.newContext();
        try {
          const probe = await sandbox.newPage();
          await probe.route('**/*', route => route.abort());
          const observed = await probe.evaluate(async source => {
            const url = URL.createObjectURL(new Blob([source], { type: 'text/javascript' }));
            try {
              const { sum } = await import(url);
              const rejects = (a: number, b: number) => { try { sum(a, b); return false; } catch { return true; } };
              return { positive: sum(2, 3), negative: sum(-2, -3), zero: sum(0, 0),
                rejectsInfinity: rejects(Infinity, 1), rejectsNaN: rejects(1, NaN) };
            } finally { URL.revokeObjectURL(url); }
          }, body.content);
          expect(observed).toEqual({ positive: 5, negative: -5, zero: 0, rejectsInfinity: true, rejectsNaN: true });
          await info.attach('independent-artifact-check', { body: JSON.stringify(observed), contentType: 'application/json' });
        } finally { await sandbox.close(); }
      }
      if (file === 'result.html') {
        expect(body.content).toContain(marker);
        // Display the actual produced bytes in a separate context with no lab credentials.
        const preview = await page.context().browser()!.newContext();
        try {
          const output = await preview.newPage();
          await output.route('**/*', route => route.abort());
          await output.setContent(body.content);
          await expect(output.getByText(marker, { exact: false })).toBeVisible();
          await info.attach('rendered-result', { body: await output.screenshot(), contentType: 'image/png' });
        } finally { await preview.close(); }
      }
    }
    const after = await request.get('/health');
    expect((await after.json()).build, 'build changed during measurement').toEqual(health.build);
  } finally {
    await info.attach('observation-timeline', { body: JSON.stringify({ worker: name, provider: lifecycleProvider, workspace: cwd, health, samples }, null, 2), contentType: 'application/json' });
    // Retain this run's IDs and files for inspection; no broad cleanup or fleet mutation.
  }
});
