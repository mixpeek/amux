// Grok Build provider in the real dashboard.
//
// The CI e2e job (ubuntu-latest, rust.yml) does not install tmux or the grok
// CLI. Live start/peek is therefore gated on both binaries being on PATH —
// locally that is the real launch; in CI the test still proves the create
// modal, the stored provider/flags, and the worker card.
import { test, expect } from '@playwright/test';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { execSync } from 'child_process';

test.skip(({ viewport }) => (viewport?.width ?? 1280) < 500, 'desktop project only');

const workDir = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-e2e-wd-'));

function haveLiveGrok(): boolean {
  try {
    // `command -v` is a shell builtin; execSync without a shell would
    // always fail and silently skip the live launch even when grok is installed.
    execSync('which grok', { stdio: 'ignore' });
    execSync('which tmux', { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

test.afterAll(() => {
  fs.rmSync(workDir, { recursive: true, force: true });
});

test('create modal offers Grok and a Grok worker is stored without a Claude model', async ({
  page,
  request,
}) => {
  if (haveLiveGrok()) {
    test.setTimeout(60_000);
  }

  await page.goto('/');
  const token = await page.evaluate(
    () => (window as unknown as { _AMUX_AUTH_TOKEN?: string })._AMUX_AUTH_TOKEN,
  );
  expect(token, 'served bootstrap must carry the auth token').toBeTruthy();
  const auth = { Authorization: `Bearer ${token}` };

  await page.evaluate(() => (window as unknown as { openCreate: () => void }).openCreate());
  await expect(page.locator('#create-provider-grok')).toBeVisible();

  // Name must NOT contain "grok" — the card badge assertion is `/^Grok$/`
  // and would be vacuous if the worker name already matched.
  const name = `e2e-live-${Date.now()}`;
  const created = await request.post('/api/sessions', {
    headers: { ...auth, 'Content-Type': 'application/json' },
    data: { name, dir: workDir, provider: 'grok' },
  });
  expect(created.status(), await created.text()).toBe(201);
  const body = await created.json();
  expect(body.provider).toBe('grok');
  expect(body.flags).toContain('grok-4.6');
  expect(body.flags).not.toContain('sonnet');
  expect(body.flags).not.toContain('opus');

  const listed = await request.get('/api/sessions', { headers: auth });
  const row = (await listed.json()).find((s: { name: string }) => s.name === name);
  expect(row, 'legacy sessions list carries the grok worker').toBeTruthy();
  expect(row.provider).toBe('grok');

  await page.reload();
  await expect(page.locator('body')).toContainText(name, { timeout: 10_000 });
  await expect(page.getByText('Grok', { exact: true }).first()).toBeVisible();

  if (!haveLiveGrok()) {
    return;
  }

  const started = await request.post(`/api/sessions/${name}/start`, { headers: auth });
  expect([200, 202], await started.text()).toContain(started.status());

  let peek = '';
  await expect
    .poll(
      async () => {
        const r = await request.get(`/api/sessions/${name}/peek?lines=80`, { headers: auth });
        const j = await r.json();
        peek = String(j.output || j.live || '');
        if (!peek || peek === '(no output)') return '';
        return peek;
      },
      { timeout: 40_000 },
    )
    .toMatch(/Grok 4\.|grok --session-id|logged in with grok/i);

  expect(peek.toLowerCase()).not.toContain('claude --model');
  expect(peek).not.toMatch(/Welcome to Claude Code/i);

  await request.post(`/api/sessions/${name}/stop`, { headers: auth }).catch(() => undefined);
});
