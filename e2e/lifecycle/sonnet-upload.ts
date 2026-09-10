import { test, expect, Page, APIRequestContext, TestInfo } from '@playwright/test';
import { boot, auth, checkpoint, getSessionsResilient } from './evidence';

// Runs after the pair scenario, reusing its author rather than adding a third
// worker. A fresh conversation makes the attachment test independent of the
// deliberate code-review defect and its earlier messages.
export async function runSonnetUpload({ page, request }: { page: Page, request: APIRequestContext }, info: TestInfo) {
  test.setTimeout(900_000);
  expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
  const run = process.env.AMUX_LIFECYCLE_PAIR_RUN!;
  const cwd = process.env.AMUX_LIFECYCLE_LAB_WORKSPACE!;
  expect(run).toMatch(/^lc-sonnet-/);
  expect(cwd).toBeTruthy();
  const author = `${run}-author`;
  const observeOnly = process.env.AMUX_LIFECYCLE_UPLOAD_OBSERVE === '1';
  const receipt = process.env.AMUX_LIFECYCLE_UPLOAD_RECEIPT || `${run}-upload-receipt.json`;
  const health = await (await request.get('/health')).json();
  await boot(page);
  const headers = await auth(page);
  const rows = await (await getSessionsResilient(request, headers)).json();
  const worker = rows.find((row: any) => row.name === author);
  expect(`${worker?.model} ${worker?.flags}`).toMatch(/sonnet/i);
  const action = async (verb: string) => {
    await page.goto('/');
    await page.locator(`.card[data-session="${author}"]`).locator('visible=true').first().locator('.card-menu-btn').click();
    await page.locator(`.card-menu.open [data-worker-action="${verb}"]`).click();
  };
  let uploadPath: string | undefined;
  if (!observeOnly) {
    await action('new-conversation');
    const reset = page.waitForResponse(r => r.url().endsWith(`/${author}/config`) && r.request().method() === 'PATCH', { timeout: 90_000 });
    await page.locator('#modal-btns').getByRole('button', { name: 'Reset', exact: true }).click();
    expect((await reset).ok()).toBe(true);
    await action('peek-terminal');
    await expect(page.locator('#peek-body')).toContainText(/Sonnet [0-9.]+(?: with [^\n]+)?[·•]/i, { timeout: 90_000 });
    await page.locator('#peek-composer-more-btn').click();
    const choose = page.waitForEvent('filechooser');
    await page.locator('#peek-more-menu').getByRole('button', { name: 'Attach file', exact: false }).click();
    await (await choose).setFiles({ name: 'fruit-counts.csv', mimeType: 'text/csv', buffer: Buffer.from('fruit,count\napples,2\npears,4\n') });
    await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(1);
    await expect.poll(() => page.evaluate(() => eval('peekFiles')[0]?.path)).toBeTruthy();
    uploadPath = await page.evaluate(() => eval('peekFiles')[0].path);
    await checkpoint(page, info, 'sonnet-upload-ready');
    await page.locator('#peek-cmd-input').fill(`Read the attached fruit-counts.csv and calculate the total count. In ${cwd}, write ${receipt} as JSON with filename, file_path (the exact uploaded path you read), row_count and total_count. Use your own chore board card for this authorized upload test, record actual file evidence, finish your task and idle. Leave completed peer-review cards alone. Do not contact any other worker or production service.`);
    const delivered = page.waitForResponse(r => r.url().endsWith(`/${author}/send`) && r.request().method() === 'POST', { timeout: 90_000 });
    await page.locator('#peek-overlay .send-split-main').click();
    const response = await delivered;
    expect(response.ok()).toBe(true);
    expect((await response.json()).submitted).toBe(true);
  }
  let result: any, cards: any[] = [];
  await expect.poll(async () => {
    const response = await request.get(`/api/fs/read?path=${encodeURIComponent(`${cwd}/${receipt}`)}`, { headers });
    if (!response.ok()) return false;
    try { result = JSON.parse((await response.json()).content); } catch { return false; }
    const board = await request.get(`/api/board?session=${author}&done_limit=0`, { headers });
    if (!board.ok()) return false;
    cards = await board.json();
    return result.row_count === 2 && result.total_count === 6 &&
      cards.some(c => ['done', 'verified'].includes(c.status) && String(c.evidence || '').includes(receipt)) &&
      cards.every(c => ['done', 'verified', 'discarded', 'cancelled'].includes(c.status));
  }, { timeout: 720_000, intervals: [5000, 15000, 30000], message: 'actual uploaded data must produce a receipt and completed task evidence' }).toBe(true);
  expect(result.file_path).toMatch(/uploads\/[^/]+-fruit-counts\.csv$/);
  if (uploadPath) expect(result.file_path).toBe(uploadPath);
  const uploaded = await request.get(`/api/fs/read?path=${encodeURIComponent(result.file_path)}`, { headers });
  expect((await uploaded.json()).content).toBe('fruit,count\napples,2\npears,4\n');
  const history = await (await request.get(`/api/history?session=${author}&limit=200`, { headers })).json();
  expect(history.some((m: any) => String(m.text).includes(`@${result.file_path}`))).toBe(true);
  for (const size of [{ width: 1280, height: 800 }, { width: 375, height: 667 }]) {
    await page.setViewportSize(size);
    await action('peek-terminal');
    await expect(page.locator('#peek-body')).toContainText('fruit-counts.csv');
    await page.waitForFunction(() => getComputedStyle(document.querySelector('#peek-overlay')!).opacity === '1');
    await checkpoint(page, info, `sonnet-upload-terminal-${size.width}`);
    await page.locator('#peek-tab-messages').click();
    await expect(page.locator('#peek-overlay')).toContainText('fruit-counts.csv');
    await checkpoint(page, info, `sonnet-upload-message-${size.width}`);
  }
  await info.attach('sonnet-upload-proof', { body: JSON.stringify({ run, author, observeOnly, result, cards }, null, 2), contentType: 'application/json' });
  expect((await (await request.get('/health')).json()).build).toBe(health.build);
}
