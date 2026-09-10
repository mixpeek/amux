import { defineConfig } from '@playwright/test';
import path from 'node:path';
// One identity shared by the pair and its follow-on upload scenario.
process.env.AMUX_LIFECYCLE_PAIR_RUN ||= `lc-sonnet-${Date.now()}`;
const output = path.resolve(process.env.AMUX_LIFECYCLE_OUTPUT || 'test-results/lifecycle');
export default defineConfig({
  testDir: '.', testMatch: 'live-*.spec.ts', workers: 1, retries: 0,
  timeout: 1_200_000,
  outputDir: path.join(output, 'live-artifacts'),
  reporter: [['line'], ['json', { outputFile: path.join(output, 'live.json') }],
    ['html', { outputFolder: path.join(output, 'live-report'), open: 'never' }]],
  use: { baseURL: process.env.AMUX_LIFECYCLE_LAB_URL, ignoreHTTPSErrors: true,
    storageState: process.env.AMUX_LIFECYCLE_STORAGE_STATE || undefined,
    viewport: { width: 1280, height: 800 }, serviceWorkers: 'block',
    trace: 'on', video: 'on', screenshot: 'on' },
});
