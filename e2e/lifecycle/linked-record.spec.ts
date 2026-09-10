import { test, expect } from '../fixtures';
import { boot, auth, checkpoint, deleteOwnedWorkers } from './evidence';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';

test('LC-LINKED-RECORD: epic, children, criteria, evidence and real file/URL/commit outputs are navigable', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  page.setDefaultTimeout(15_000);
  const dir = await mkdtemp(path.join(os.tmpdir(), 'amux-linked-record-'));
  const worker = `lc-record-${info.project.name}-${Date.now()}`;
  await writeFile(path.join(dir, 'result.md'), '# Linked output\nVerified invoice total: 42\n');
  for (const args of [['init'], ['add', 'result.md'], ['-c', 'user.name=Lifecycle Test', '-c', 'user.email=lifecycle@example.test', 'commit', '-m', 'Produce linked invoice report']]) execFileSync('git', args, { cwd: dir, stdio: 'pipe' });
  const commit = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: dir, encoding: 'utf8' }).trim();
  await boot(page); const headers = await auth(page);
  expect((await request.post('/api/sessions', { headers, data: { name: worker, dir } })).ok()).toBe(true);
  const make = async (data: any) => {
    const r = await request.post('/api/board', { headers, data: { type: 'chore', session: worker, ...data } });
    expect(r.ok(), await r.text()).toBe(true); const card = await r.json();
    const patch = await request.patch(`/api/board/${card.id}`, { headers, data: { ...data, acceptance_criteria: data.acceptance_criteria ? JSON.stringify(data.acceptance_criteria) : undefined } });
    expect(patch.ok(), await patch.text()).toBe(true); return patch.json();
  };
  const epic = await make({ title: 'Invoice reconciliation release', type: 'epic', desc: 'Three linked workstreams produce a reviewed report.' });
  const prerequisite = await make({ title: 'Validate invoice input', epic: epic.id });
  const card = await make({ title: 'Build the reviewed invoice report with linked evidence', epic: epic.id,
    depends_on: [prerequisite.id], desc: 'Preserve the detailed task context while navigating its linked work.',
    acceptance_criteria: ['Report total equals 42', 'Reviewer checks malformed input'],
    evidence: 'Ran invoice fixture: PASS total=42; see result.md. Reviewer verified the same result.' });
  const linkedMessage = await request.post('/api/history', { headers, data: {session: worker, type:'session', origin:'fixture-peer', text:`REVIEW_EVIDENCE ${card.id}: independently checked invoice total 42.`} });
  expect(linkedMessage.ok()).toBe(true);
  const message = await linkedMessage.json();
  const child = await make({ title: 'Verify malformed input handling', epic: card.id });
  const origin = new URL(page.url()).origin;
  for (const artifact of [
    { kind: 'implementation', ref: 'result.md', description: 'Report total equals 42' },
    { kind: 'verification', ref: 'file://' + path.join(dir, 'result.md'), description: 'File URL output' },
    { kind: 'verification', ref: origin + '/health', description: 'Live test-server URL' },
    { kind: 'implementation', ref: commit, description: 'Git commit producing the report' },
  ]) expect((await request.post(`/api/board/${card.id}/artifacts`, { headers, data: { ...artifact, state: 'created' } })).ok()).toBe(true);
  const open = async () => {
    await page.goto('/#issue=' + card.id);
    // Following Messages may leave the same hash in place; reload exercises a
    // direct link instead of asserting against a closed detail's stale DOM.
    await page.reload();
    await expect(page.locator('#board-detail-overlay')).toHaveClass(/active/);
    await expect(page.locator('#bd-key')).toHaveText(card.id);
    await expect(page.locator('#bd-record-summary')).toContainText(epic.id);
    await expect(page.locator('#bd-meta')).toContainText('Produced output (4)');
  };
  try {
    await open();
    await expect(page.locator('#bd-preview')).toContainText('Preserve the detailed task context');
    await expect(page.locator('.bd-evidence-section')).toContainText('Report total equals 42');
    await expect(page.locator('.bd-evidence-section')).toContainText('PASS total=42');
    await checkpoint(page, info, 'linked-task-details');
    await page.locator('#bd-tab-related').click();
    await page.locator('#bd-meta button').filter({hasText: `MSG-${message.id}`}).click();
    await expect(page.locator('#msgs-search')).toHaveValue(`MSG-${message.id}`);
    await expect(page.locator('#messages-view')).toContainText('REVIEW_EVIDENCE');
    await open();
    await page.locator('#bd-tab-subtasks').click();
    await expect(page.locator('#bd-meta')).toContainText(child.title);
    await page.locator('#bd-meta .bd-related-link').filter({ hasText: child.id }).click();
    await expect(page.locator('#bd-key')).toHaveText(child.id);
    await open();
    await page.locator('#bd-tab-related').click();
    await page.locator('#bd-meta .task-id-chip').filter({ hasText: prerequisite.id }).click();
    await expect(page.locator('#bd-key')).toHaveText(prerequisite.id);
    await open();
    await page.locator('#bd-tab-files').click();
    const files = page.locator('#bd-meta [data-record-kind="files"]').filter({ hasText: 'Produced output' });
    await files.getByRole('button', { name: 'result.md', exact: true }).click();
    await expect(page.locator('#file-body')).toContainText('Verified invoice total: 42');
    await checkpoint(page, info, 'linked-file-preview');
    await page.locator('[onclick="closeFilePreview()"]').click();
    await files.getByRole('button', { name: 'file://' + path.join(dir, 'result.md'), exact: true }).click();
    await expect(page.locator('#file-body')).toContainText('Verified invoice total: 42');
    await page.locator('[onclick="closeFilePreview()"]').click();
    const popupPromise = page.waitForEvent('popup');
    await files.getByRole('link', { name: origin + '/health', exact: true }).click();
    const popup = await popupPromise; await popup.waitForLoadState();
    await expect(popup.locator('body')).toContainText('build'); await popup.close();
    await files.getByRole('button', { name: commit, exact: true }).click();
    await expect(page.locator('#commits-detail-header')).toContainText('Produce linked invoice report');
    await expect(page.locator('#commits-detail-diff')).toContainText('Verified invoice total: 42');
    await checkpoint(page, info, 'linked-git-commit');
    await page.getByRole('button', { name: 'Close worker', exact: true }).click();
    await page.goto('/'); await page.locator('#tab-board').click();
    await page.locator('#bo-agent').click(); await page.locator('#bv-status').click();
    await page.locator('#board-search').fill('worker:' + worker);
    await expect(page.locator('#board-columns')).toContainText(card.title);
    await checkpoint(page, info, 'linked-outcome-board');
  } finally {
    await deleteOwnedWorkers(page, request, headers, [worker]);
    await rm(dir, { recursive: true, force: true });
  }
});
