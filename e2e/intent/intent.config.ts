// The intent-first acceptance set (docs/design/intent-first.md): nine use
// cases, each at desktop width and at 375px.
//
// Two ways to run:
// - Isolated (default): one server per viewport on a throwaway AMUX_HOME
//   holding the seeded household (household.mjs), with a stand-in helper model
//   (helper-stub.mjs) and chat helper (fake-helper-chat.py). Deterministic.
//     AMUX_E2E_SERVER_BIN=<binary> npx playwright test -c e2e/intent/intent.config.ts
// - Live: AMUX_INTENT_URL=https://localhost:<port> runs the same specs against a
//   running amux (the household demo instance, real model, real helpers),
//   with longer waits. `scripts/intent-household.sh` starts that instance.
//
// Every spec asserts no outside send happened (GET /api/email/log).
import { defineConfig } from '@playwright/test';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { buildHousehold } from './household.mjs';

const LIVE = process.env.AMUX_INTENT_URL || '';
const OFFSET = Number(process.env.AMUX_E2E_PORT_OFFSET || 0) || 0;
const TARGETS = [
  { name: 'desktop', port: 18923 + OFFSET, viewport: { width: 1280, height: 800 } },
  { name: 'mobile', port: 18933 + OFFSET, viewport: { width: 375, height: 667 } },
];
const BIN = process.env.AMUX_E2E_SERVER_BIN || path.join(os.homedir(), '.amux/rust-build-target/debug/amux-server');
const DASH = process.env.AMUX_DASHBOARD_DIR || path.resolve(__dirname, '../../crates/amux-dashboard/static');

// Playwright evaluates this file in the runner and again in every worker
// process; the runner's homes are handed down through the environment so the
// servers and the specs agree on one home per target.
const homes: Record<string, string> = JSON.parse(process.env.AMUX_INTENT_HOMES || '{}');
if (!LIVE && !Object.keys(homes).length) {
  for (const t of TARGETS) {
    const home = fs.mkdtempSync(path.join(os.tmpdir(), `amux-intent-${t.name}-`));
    fs.mkdirSync(path.join(home, 'mailboxes'), { recursive: true });
    const hh = buildHousehold();
    fs.writeFileSync(path.join(home, 'mailboxes', 'household.json'), JSON.stringify(hh.mailbox, null, 1));
    fs.writeFileSync(path.join(home, 'household-events.json'), JSON.stringify(hh.events));
    fs.writeFileSync(path.join(home, 'server.env'), '');
    fs.mkdirSync(path.join(home, 'tmux'), { recursive: true });
    homes[t.name] = home;
  }
  process.env.AMUX_INTENT_HOMES = JSON.stringify(homes);
}

export default defineConfig({
  testDir: '.',
  testMatch: /uc\d.*\.spec\.ts$/,
  timeout: LIVE ? 900_000 : 120_000,
  expect: { timeout: LIVE ? 600_000 : 30_000 },
  fullyParallel: false,
  workers: LIVE ? 1 : 2,
  retries: 0,
  reporter: [['list'], ['json', { outputFile: path.join(os.tmpdir(), `amux-intent-report-${process.pid}.json`) }]],
  use: { ignoreHTTPSErrors: true, serviceWorkers: 'block', trace: 'retain-on-failure', screenshot: 'only-on-failure' },
  projects: TARGETS.map((t) => ({
    name: t.name,
    use: {
      baseURL: LIVE || `https://localhost:${t.port}`,
      viewport: t.viewport,
      ...(t.name === 'mobile' ? { isMobile: true, hasTouch: true, deviceScaleFactor: 2 } : {}),
    },
    metadata: { home: homes[t.name] || '' },
  })),
  webServer: LIVE
    ? undefined
    : TARGETS.map((t) => ({
        command: BIN,
        url: `https://localhost:${t.port}/health`,
        ignoreHTTPSErrors: true,
        reuseExistingServer: false,
        timeout: 120_000,
        stdout: 'ignore',
        stderr: 'pipe',
        env: {
          AMUX_HOME: homes[t.name],
          AMUX_RS_PORT: String(t.port),
          AMUX_RS_BIND: '127.0.0.1',
          AMUX_NO_SELF_ADOPT: '1',
          AMUX_DASHBOARD_DIR: DASH,
          AMUX_HELPER_CLI: path.join(__dirname, 'helper-stub.mjs'),
          AMUX_CHAT_CLAUDE_BIN: path.join(__dirname, 'fake-helper-chat.py'),
          // The stand-ins answer; no real model is reachable from this server.
          ANTHROPIC_API_KEY: '',
          AMUX_HELPER_MODEL: 'haiku',
          CLAUDE_CONFIG_DIR: path.join(homes[t.name], 'claude-config'),
          AMUX_VAULT_KEYSTORE: 'file',
          TMUX: '',
          TMUX_TMPDIR: path.join(homes[t.name], 'tmux'),
        },
      })),
});
