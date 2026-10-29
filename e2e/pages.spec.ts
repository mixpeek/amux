// HTML apps (api/pages.rs, pages-host.js): an app whose backend is a worker
// and its own SQLite file, in the Apps tab and at /pages/<name>.
//
// The fixture is a small lead list with the shapes a real page needs: rows
// read from page.db, a button that saves, a form that asks the worker, and
// the page re-rendering when someone else writes its data. It also probes its
// own sandbox, because the isolation claim is the one that matters most.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

const HTML = `<!doctype html><html><head><meta charset="utf-8"><title>Lead list</title></head><body>
<h1>Leads</h1><a href="#bottom" id="jump">Jump</a>
<table id="t"><tbody></tbody></table>
<form id="f"><input id="fb" aria-label="Ask"><button>Ask</button></form>
<p id="sandbox"></p><div style="height:1600px"></div><p id="bottom">Bottom</p>
<script>(async () => {
  const load = async () => {
    const r = await amux.sql("SELECT id, name, status FROM leads ORDER BY id");
    document.querySelector("#t tbody").innerHTML = r.rows.map(x =>
      '<tr data-id="' + x.id + '"><td class="nm">' + x.name + '</td><td class="st">' + x.status + '</td><td><button class="ap">Approve</button></td></tr>').join("");
  };
  await load();
  document.getElementById("t").addEventListener("click", async e => {
    if (!e.target.matches(".ap")) return;
    await amux.sql("UPDATE leads SET status = 'approved' WHERE id = ?", [+e.target.closest("tr").dataset.id]);
    await load();
  });
  document.getElementById("f").addEventListener("submit", async e => {
    e.preventDefault();
    await amux.ask(document.getElementById("fb").value, { lead: 1 }, "enrich");
    document.getElementById("fb").value = "";
  });
  amux.on("change", load);
  const probes = [];
  try { const r = await fetch("/api/workers"); probes.push(r.ok ? "api:reachable" : "api:refused"); } catch (_) { probes.push("api:blocked"); }
  try { void parent.document.title; probes.push("parent:reachable"); } catch (_) { probes.push("parent:blocked"); }
  try { void localStorage.length; probes.push("storage:reachable"); } catch (_) { probes.push("storage:blocked"); }
  document.getElementById("sandbox").textContent = probes.join(" ");
})();</script></body></html>`;

const SEED = `CREATE TABLE leads (id INTEGER PRIMARY KEY, name TEXT, status TEXT);
INSERT INTO leads (name, status) VALUES ('Magnolia Ridge', 'new'), ('Laurel Creek', 'new'), ('Sweetgum', 'new');`;

test('a page reads and saves its own database, asks its worker, and cannot reach amux', async ({ page, request }, testInfo) => {
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const name = `leads-${testInfo.project.name}-${Date.now() % 100000}`.toLowerCase().replace(/[^a-z0-9-]/g, '-');
  // An existing worker name that runs nothing: the ask is recorded and its
  // delivery refused, so the suite never starts a model.
  const created = await request.post('/api/pages', { headers: auth, data: { name, html: HTML, seed_sql: SEED, worker: 'pages-e2e-none' } });
  expect(created.status(), await created.text()).toBe(201);
  const sql = async (q: string, params: unknown[] = []) => {
    const r = await request.post(`/api/pages/${name}/sql`, { headers: auth, data: { sql: q, params } });
    expect(r.ok(), await r.text()).toBe(true);
    return r.json();
  };
  try {
    // The standalone URL.
    await page.goto(`/pages/${name}`);
    await expect(page).toHaveTitle('Lead list');
    const frame = page.frameLocator(`iframe[data-page="${name}"]`);
    await expect(frame.locator('#t tr')).toHaveCount(3);
    await expect(frame.locator('#sandbox')).toHaveText('api:blocked parent:blocked storage:blocked');

    // A click saves to page.db, and a reload reads it back.
    await frame.locator('#t tr').nth(1).locator('.ap').click();
    await expect(frame.locator('#t tr').nth(1).locator('.st')).toHaveText('approved');
    expect((await sql("SELECT status FROM leads WHERE name = 'Laurel Creek'")).rows[0].status).toBe('approved');
    await page.reload();
    await expect(frame.locator('#t tr').nth(1).locator('.st')).toHaveText('approved');

    // An in-page link scrolls the page instead of navigating its frame away.
    await frame.locator('#jump').click();
    await expect(frame.locator('#bottom')).toBeInViewport();
    await expect(frame.locator('#t tr')).toHaveCount(3);

    // An ask is a row the worker is woken for.
    await frame.locator('#fb').fill('Find the owner of Sweetgum');
    await frame.locator('#f button').click();
    await expect.poll(async () => (await sql("SELECT kind, status, text FROM asks")).rows)
      .toEqual([{ kind: 'enrich', status: 'new', text: 'Find the owner of Sweetgum' }]);

    // Someone else's write (the worker's) re-renders the open page.
    await sql("UPDATE leads SET name = 'Sweetgum Hospice' WHERE id = 3");
    await expect(frame.locator('#t tr').nth(2).locator('.nm')).toHaveText('Sweetgum Hospice', { timeout: 8000 });

    // ATTACH cannot reach amux.db.
    const attach = await request.post(`/api/pages/${name}/sql`, { headers: auth, data: { sql: "ATTACH DATABASE 'x.db' AS x" } });
    expect(attach.status()).toBe(400);
    expect(await attach.text()).toContain('too many attached');

    // The Apps tab lists it and mounts the same page.
    await page.goto('/#view=apps');
    const row = page.locator(`.pages-row[data-page="${name}"]`);
    await expect(row).toBeVisible();
    await expect(row).toContainText('Lead list');
    await row.click();
    const tabFrame = page.frameLocator(`#pages-frame iframe[data-page="${name}"]`);
    await expect(tabFrame.locator('#t tr').nth(2).locator('.nm')).toHaveText('Sweetgum Hospice');
    await expect(page.locator('#pages-standalone')).toHaveAttribute('href', `/pages/${name}`);
    // The frame fills the window below its bar (it got its 320px minimum
    // while the views sat in a block body).
    const vh = page.viewportSize()!.height;
    await expect.poll(async () => { const b = (await page.locator('#pages-frame').boundingBox())!; return b.y + b.height; })
      .toBeGreaterThan(vh - 40);
    await expect(page.locator('#pages-status')).toHaveText('1 ask with the worker');

    // Versions: restore puts the data back, and keeps the state it replaced.
    const snap = await request.post(`/api/pages/${name}/snapshots`, { headers: auth, data: {} });
    expect(snap.ok()).toBe(true);
    await sql("UPDATE leads SET status = 'lost'");
    await expect(tabFrame.locator('#t tr').first().locator('.st')).toHaveText('lost', { timeout: 8000 });
    await page.getByRole('button', { name: 'Versions', exact: true }).click();
    await page.locator('#modal-msg .pages-version', { hasText: 'Saved by hand' }).first().getByRole('button', { name: 'Restore' }).click();
    await page.locator('#modal-btns').getByRole('button', { name: 'Restore' }).click();
    await expect(tabFrame.locator('#t tr').first().locator('.st')).toHaveText('new', { timeout: 8000 });
    await expect(tabFrame.locator('#t tr').nth(1).locator('.st')).toHaveText('approved');
    const snaps = (await (await request.get(`/api/pages/${name}/snapshots`, { headers: auth })).json()).snapshots;
    expect(snaps.map((x: any) => x.reason)).toContain('before-restore');
  } finally {
    await cleanup('trash fixture page', () => request.delete(`/api/pages/${name}`, { headers: auth }));
  }
});
