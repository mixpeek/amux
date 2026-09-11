// Muse Code provider in the real dashboard.
//
// The CI e2e job (ubuntu-latest, rust.yml) installs neither tmux nor the muse
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

function haveLiveMuse(): boolean {
  try {
    execSync('which muse', { stdio: 'ignore' });
    execSync('which tmux', { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

test.afterAll(() => {
  fs.rmSync(workDir, { recursive: true, force: true });
});

test('create modal offers Muse Code and a Muse worker never inherits a Claude model', async ({
  page,
  request,
}) => {
  if (haveLiveMuse()) {
    test.setTimeout(90_000);
  }

  await page.goto('/');
  const token = await page.evaluate(
    () => (window as unknown as { _AMUX_AUTH_TOKEN?: string })._AMUX_AUTH_TOKEN,
  );
  expect(token, 'served bootstrap must carry the auth token').toBeTruthy();
  const auth = { Authorization: `Bearer ${token}` };

  await page.evaluate(() => (window as unknown as { openCreate: () => void }).openCreate());
  await expect(page.locator('#create-provider-muse')).toBeVisible();

  // Name must NOT contain "muse" — the card badge assertion below would be
  // vacuous if the worker name already matched it.
  const name = `e2e-live-${Date.now()}`;
  const created = await request.post('/api/sessions', {
    headers: { ...auth, 'Content-Type': 'application/json' },
    data: { name, dir: workDir, provider: 'muse' },
  });
  expect(created.status(), await created.text()).toBe(201);
  const body = await created.json();
  expect(body.provider).toBe('muse');

  // THE POINT OF THIS ASSERTION: an unspecified model must leave flags EMPTY so
  // muse's own CLI decides, and must never fall back to the Claude default.
  // `--model opus` reaching Meta is the gtm-researcher-gemini defect one
  // provider over: a worker dead on arrival, naming a model nobody chose. The
  // muse default (muse-spark-1.3-contributor) is applied at LAUNCH, by
  // default_model_for_provider, not baked into CC_FLAGS at create.
  expect(body.flags || '').not.toContain('sonnet');
  expect(body.flags || '').not.toContain('opus');
  expect(body.flags || '').not.toContain('haiku');

  const listed = await request.get('/api/sessions', { headers: auth });
  const row = (await listed.json()).find((s: { name: string }) => s.name === name);
  expect(row, 'legacy sessions list carries the muse worker').toBeTruthy();
  expect(row.provider).toBe('muse');

  await page.reload();
  await expect(page.locator('body')).toContainText(name, { timeout: 10_000 });
  await expect(page.getByText('Muse Code', { exact: true }).first()).toBeVisible();

  if (!haveLiveMuse()) {
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
      { timeout: 60_000 },
    )
    .toMatch(/muse|spark/i);

  // It launched `muse`, not `claude`. The whole point of the launch arm is that
  // an unhandled provider falls through to build_claude_cmd.
  expect(peek.toLowerCase()).not.toContain('claude --model');
  expect(peek).not.toMatch(/Welcome to Claude Code/i);

  // THE RISKIEST PART OF THIS INTEGRATION, asserted end to end: muse cannot be
  // told its session id, so amux learns it from disk after launch
  // (muse_pick_session). If that discovery silently failed, everything above
  // would still pass and the lane would simply open a BRAND NEW conversation on
  // every restart — a data-loss bug invisible from the outside. Read the meta
  // the server just wrote and require a real uuid.
  const metaPath = path.join(
    os.homedir(),
    '.amux',
    'sessions',
    `${name}.meta.json`,
  );
  await expect
    .poll(
      () => {
        try {
          const meta = JSON.parse(fs.readFileSync(metaPath, 'utf8'));
          return String(meta.muse_session_id || '');
        } catch {
          return '';
        }
      },
      { timeout: 30_000 },
    )
    .toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);

  await request.post(`/api/sessions/${name}/stop`, { headers: auth }).catch(() => undefined);
});
