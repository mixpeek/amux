import { test, expect } from '@playwright/test';
import { cleanup } from './teardown';

// AMUX-2755. GET /api/schedules/audit was routed and the SPA referenced it zero
// times. The trail existed and reached nobody — in the subsystem whose own
// incident (AMUX-1812: eight schedules vanished with no attribution) is why it
// was built.
//
// The assertions below are about the DELETED case specifically. A per-schedule
// row cannot show a deleted schedule — the row is gone — so a panel that only
// annotated live rows would reproduce the original blindness. Measured on the
// live board: 151 schedules have audit rows, 118 still exist, 45 are visible
// only here.

test('the change-history panel renders a DELETED schedule, which no row can show', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');

  const r = await page.evaluate(() => {
    const w = window as any;
    // Two entries: one for a live schedule, one for a schedule that no longer
    // exists. The second is the case this panel exists for.
    const rows = [
      { schedule_id: 'SCHED-1', title: 'still here', field: 'enabled', by_who: 'amux',
        source: 'api-patch', old_value: '0', new_value: '1', ts: Math.floor(Date.now() / 1000) - 60 },
      { schedule_id: 'SCHED-GONE', title: 'vanished one', field: 'deleted', by_who: '',
        source: 'api-delete', old_value: '', new_value: '1', ts: Math.floor(Date.now() / 1000) - 120 },
    ];
    // Passed as ARGUMENTS: top-level `let`s are not window properties, so
    // assigning window._schedulerAudit would leave this function reading an
    // empty array while every assertion below still ran.
    w.renderSchedulerAudit(rows, new Set(['SCHED-1']));
    const el = document.getElementById('scheduler-audit');
    return { html: el ? el.innerHTML : '', count: document.getElementById('sched-audit-n')?.textContent || '' };
  });

  expect(r.html, 'the live schedule change must render').toContain('still here');
  expect(r.html, 'a DELETED schedule must render — no row can show it').toContain('vanished one');
  expect(r.html, 'and be marked as gone').toContain('gone');
  // An unattributed write is the exact AMUX-1812 failure; it must not render blank.
  expect(r.html, 'an unattributed write must be named, not blank').toContain('unattributed');
  expect(r.html, 'the attributed one must name its author').toContain('amux');
  expect(r.count).toBe('(2)');
});

test('an empty trail says so rather than rendering nothing', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  const html = await page.evaluate(() => {
    const w = window as any;
    w.renderSchedulerAudit([], new Set());
    return document.getElementById('scheduler-audit')?.innerHTML || '';
  });
  expect(html).toContain('No recorded schedule changes');
});

// Primitive integrity audit 2026-11-26, P2: schedule_audit folds into the
// ledger. A row shows the actor the server PROVED beside the claimed one, and
// an anonymous record proves nothing, so it adds nothing.
test('a change with a ledger record shows who the server proved made it', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  const html = await page.evaluate(() => {
    const w = window as any;
    const now = Math.floor(Date.now() / 1000);
    w.renderSchedulerAudit([
      { schedule_id: 'SCHED-1', title: 'owner edit', field: 'enabled', by_who: 'amux', source: 'api-patch',
        old_value: '1', new_value: '0', ts: now - 30, ledger: { seq: 41, actor: 'owner', sealed: true } },
      { schedule_id: 'SCHED-1', title: 'claimed only', field: 'enabled', by_who: 'lane-x', source: 'api-patch',
        old_value: '0', new_value: '1', ts: now - 40, ledger: { seq: 42, actor: 'anonymous', claimed: 'worker:lane-x' } },
    ], new Set(['SCHED-1']));
    return document.getElementById('scheduler-audit')?.innerHTML || '';
  });
  expect(html).toContain('(proven: owner)');
  expect(html.match(/proven:/g)?.length, 'an anonymous record proves nothing and is not shown as proof').toBe(1);
});

test('the real audit trail carries the ledger record, and the panel renders it', async ({ page, request }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const title = `ledger-fold-${Date.now()}`;
  const created = await request.post('/api/schedules', {
    headers: auth, data: { title, session: 'nobody', command: 'echo fold', schedule_expr: 'daily at 9am', enabled: 1 },
  });
  expect([200, 201]).toContain(created.status());
  const id = (await created.json()).id;
  try {
    const patched = await request.patch(`/api/schedules/${id}`, { headers: auth, data: { enabled: 0 } });
    expect(patched.status()).toBe(200);
    const audit = await (await request.get(`/api/schedules/audit?id=${id}`, { headers: auth })).json();
    expect(audit.length, 'create and patch are both audited').toBeGreaterThanOrEqual(2);
    for (const row of audit) {
      expect(row.ledger, `row ${row.id} (${row.field}) has a ledger record`).toBeTruthy();
      expect(typeof row.ledger.seq).toBe('number');
    }
    const actor = audit[0].ledger.actor;
    const html = await page.evaluate(({ rows, id }) => {
      (window as any).renderSchedulerAudit(rows, new Set([id]));
      return document.getElementById('scheduler-audit')?.innerHTML || '';
    }, { rows: audit, id });
    expect(html).toContain(title);
    if (actor !== 'anonymous') expect(html).toContain(`(proven: ${actor})`);
    else expect(html).not.toContain('proven:');
  } finally {
    await cleanup('delete the test schedule', () => request.delete(`/api/schedules/${id}`, { headers: auth }));
  }
});
