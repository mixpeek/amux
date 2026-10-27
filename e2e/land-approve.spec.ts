import {test, expect} from './fixtures';

// The owner approves one exact change to an owner-gated path from the link the
// land gate puts in its needsyou card (scripts/land_gate.py, api/land_approvals.rs).
// A worker files the ask; only the owner's dashboard can answer it.
test('the owner approves an owner-gated change from its link, and the ask closes', async ({page, request}) => {
  const pid = Array.from({length: 40}, (_, i) => '0123456789abcdef'[(i * 7 + Date.now()) % 16]).join('');
  const ask = await request.post('/api/land/approvals/request', {
    headers: {'X-Amux-Session': 'e2e-lane'},
    data: {patch_id: pid, files: ['boundaries.toml'], diffstat: ' boundaries.toml | 2 +-\n 1 file changed',
           subjects: ['feat: cap payments at 40'], head: 'abc123'},
  });
  expect(ask.ok()).toBeTruthy();
  const {card, link} = await ask.json();
  expect(link).toBe('/#land-approve=' + pid);

  const byWorker = await request.post('/api/land/approvals', {headers: {'X-Amux-Session': 'e2e-lane'}, data: {patch_id: pid}});
  expect(byWorker.status(), 'a worker cannot approve').toBe(403);

  await page.goto(link);
  const modal = page.locator('#modal-backdrop.open');
  await expect(modal).toBeVisible();
  await expect(modal).toContainText('Approve this exact change?');
  await expect(modal).toContainText('e2e-lane');
  await expect(modal).toContainText('boundaries.toml');
  await expect(modal).toContainText('feat: cap payments at 40');
  expect(new URL(page.url()).hash, 'the link leaves the address bar').toBe('');
  const approve = page.locator('#modal-btns .btn.primary');
  await expect(approve).toHaveText('Approve');
  const box = await approve.boundingBox();
  expect(box && box.height, 'the button is a touch target').toBeGreaterThanOrEqual(32);
  await approve.click();
  await expect(page.locator('#toast')).toContainText('Approved');

  const got = await (await request.get('/api/land/approvals?patch_id=' + pid)).json();
  expect(got.approved).toBe(true);
  if (card) {
    const row = await (await request.get('/api/board/' + card)).json();
    expect(row.status, 'the needsyou ask closed').not.toBe('needsyou');
  }

  // The same link again says it is done rather than asking twice.
  await page.goto('about:blank');
  await page.goto(link);
  await expect(page.locator('#toast')).toContainText('Already approved');
});
