// Approving a land-approval card records the approval the land gate reads
// (2026-11-09): Approve on AMUX-6809 moved the card to todo and saved a
// standing approval, while POST /api/land/approvals never ran, so the gate kept
// refusing the exact diff the owner had approved. Approve now records the
// patch-id there (which closes the card) and does not move the card itself.
// A refusal there leaves the card in needsyou and says why.
// The card and every write are fakes.
import { test, expect, allowUnusedRoute } from './fixtures';

const PID = '88f8f5b067f2794864ffc76bbc61bff2f850aafc';

for (const answer of [200, 403]) test(`Approve on a land-approval card records patch-id ${PID.slice(0, 12)} (server answers ${answer})`, async ({ page }) => {
  const approvals: any[] = [];
  const patches: any[] = [];
  await page.route('**/api/land/approvals', async r => {
    if (r.request().method() !== 'POST') return r.fallback();
    approvals.push(JSON.parse(r.request().postData() || '{}'));
    return answer === 200
      ? r.fulfill({ json: { ok: true, patch_id: PID, approved_at: 1794238800, by: 'owner', card: 'LA-9001', card_closed: true } })
      : r.fulfill({ status: 403, json: { ok: false, error: 'only the owner may approve an owner-gated change' } });
  });
  await page.route('**/api/board/LA-9001**', async r => {
    if (r.request().method() === 'PATCH') patches.push(JSON.parse(r.request().postData() || '{}'));
    return r.fulfill({ json: { id: 'LA-9001', status: 'needsyou', desc: '' } });
  });
  allowUnusedRoute(page, '**/api/board/LA-9001**');   // the point: the card is not moved by hand
  await page.route('**/api/needs-input/log', r => r.fulfill({ json: { ok: true } }));
  allowUnusedRoute(page, '**/api/needs-input/log');
  await page.route('**/api/events**', r => r.abort());
  allowUnusedRoute(page, '**/api/events**');
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._niApply === 'function');
  const out = await page.evaluate(async (pid) => {
    const it = { kind: 'card', key: 'card:LA-9001', card: 'LA-9001', worker: null,
      question: "Approve amux-frustrations's change " + pid.slice(0, 12) + ' to scripts/land_gate.py?',
      unblocks: 'Approving at /#land-approve=' + pid + ' lets that exact diff land.', land_patch_id: pid };
    return await (window as any)._niApply(it, 'approve', '', null);
  }, PID);
  expect(approvals.map(a => a.patch_id)).toEqual([PID]);
  expect(patches).toEqual([]);
  const said = JSON.stringify(out);
  if (answer === 200) expect(said).toContain('approved to land');
  else expect(said).toContain('land approval ' + PID.slice(0, 12));
});
