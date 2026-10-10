// Opt-in fault proof against a disposable local server. Never damage live prefs.
import { test, expect } from './fixtures';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

test('damaged local or inherited connector scope is visible and explicitly repairable', async ({ page, request }, info) => {
  const home = process.env.AMUX_SCOPE_E2E_HOME || '';
  test.skip(!home, 'Requires an explicitly owned disposable server home');
  expect(home).toMatch(/^\/private\/tmp\/amux-ce-scope/);
  const token = readFileSync(home + '/auth_token', 'utf8').trim();
  const auth = { Authorization: 'Bearer ' + token };
  const worker = 'scope-ui-' + info.project.name.replace(/[^a-z0-9-]/gi, '-') + '-' + Date.now();
  const group = worker + '-group';
  const id = 'scope-proof-oauth'; // Synthetic usable grant seeded by the fault rig.
  const layers = [
    { level: 'global', name: '', pref: 'connectors:global' },
    { level: 'group', name: group, pref: 'connectors:group:' + group },
    { level: 'worker', name: worker, pref: 'connectors:worker:' + worker },
  ];
  const sql = (statement: string, args: unknown[]) => execFileSync('python3', ['-c',
    'import sqlite3,json,sys; c=sqlite3.connect(sys.argv[1]); q=c.execute(sys.argv[2],json.loads(sys.argv[3])); print(json.dumps(q.fetchall())); c.commit()',
    home + '/amux.db', statement, JSON.stringify(args)], { encoding: 'utf8' });
  const original = layers.map(l => JSON.parse(sql('SELECT value FROM prefs WHERE key=?', [l.pref])));
  const created = await request.post('/api/sessions', { headers: auth, data: { name: worker, dir: '/tmp', tags: [group] } });
  expect(created.status()).toBe(201);
  try {
    for (const layer of layers) {
      for (const l of layers) sql('DELETE FROM prefs WHERE key=?', [l.pref]);
      sql('INSERT INTO prefs(key,value) VALUES(?,?)', [layer.pref, '{damaged-ui-scope']);
      const denied = await request.post(`/api/connectors/${id}/token?account=beth`, {
        headers: { ...auth, 'X-Amux-Session': worker }, data: {},
      });
      expect(denied.status()).toBe(403);
      expect((await denied.json()).blocked).toBe('connector_scope_unreadable');
      await page.goto('/?peekEmbed=' + worker + '&peekTab=scope');
      const tile = page.locator('#peek-scope-body .scope-tile').filter({ hasText: 'Connectors' });
      await expect(tile).toContainText('unreadable');
      await expect(tile).toContainText('Access blocked');
      await tile.click();
      const alert = page.locator('#peek-scope-body [role=alert]').filter({ hasText: 'invalid saved connector scope' });
      await expect(alert).toContainText(layer.level);
      await alert.getByRole('button', { name: 'Repair saved scope' }).click();
      const modal = page.locator('#scope-edit-backdrop');
      await expect(modal).toHaveClass(/open/);
      await expect(modal.locator('#scope-edit-msg')).toContainText('Connector access is blocked');
      await expect(modal.locator('#scope-edit-input')).toHaveValue('');
      let puts = 0;
      const observe = (req: any) => { if (new URL(req.url()).pathname === '/api/scope' && req.method() === 'PUT') puts++; };
      page.on('request', observe);
      await modal.getByRole('button', { name: 'Save', exact: true }).click();
      await expect(modal.locator('#scope-edit-msg')).toContainText('Enter the complete replacement JSON');
      expect(puts).toBe(0);
      expect(JSON.parse(sql('SELECT value FROM prefs WHERE key=?', [layer.pref]))[0][0]).toBe('{damaged-ui-scope');
      const replacement = { [id]: { enabled: false, account: 'beth', mcp: false }, unrelated: { enabled: false, account: 'retained', mcp: true } };
      await modal.locator('#scope-edit-input').fill(JSON.stringify(replacement));
      const saved = page.waitForResponse(r => new URL(r.url()).pathname === '/api/scope' && r.request().method() === 'PUT');
      await modal.getByRole('button', { name: 'Save', exact: true }).click();
      expect((await saved).status()).toBe(200);
      await expect(modal.locator('#scope-edit-msg')).toHaveText('Saved');
      expect(puts).toBe(1);
      page.off('request', observe);
      const scope = await request.get('/api/scope?level=' + layer.level + '&name=' + layer.name, { headers: auth });
      expect((await scope.json()).capabilities.find((c: any) => c.key === 'connectors').value.connectors).toEqual(replacement);
      expect((await request.post(`/api/connectors/${id}/token?account=beth`, { headers: { ...auth, 'X-Amux-Session': worker }, data: {} })).status()).toBe(403);
      await page.screenshot({ path: info.outputPath(layer.level + '-repair.png'), fullPage: true });
    }
  } finally {
    layers.forEach((l, i) => {
      sql('DELETE FROM prefs WHERE key=?', [l.pref]);
      if (original[i].length) sql('INSERT INTO prefs(key,value) VALUES(?,?)', [l.pref, original[i][0][0]]);
    });
    await request.delete('/api/sessions/' + worker, { headers: auth });
  }
});
