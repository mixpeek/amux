// One browser acceptance entry point: existing regressions + connected journeys.
// Keep every existing browser project; adding a spec automatically includes it.
import { defineConfig } from '@playwright/test';
import base from '../playwright.config';
import path from 'node:path';
import fs from 'node:fs';
import os from 'node:os';

const output = path.resolve(process.env.AMUX_LIFECYCLE_OUTPUT || 'test-results/lifecycle');
const firstPort = Number(process.env.AMUX_LIFECYCLE_PORT || 19823);
const selected = process.argv.flatMap((arg, index, args) => arg.startsWith('--project=')
  ? [arg.slice(10)] : arg === '--project' ? [args[index + 1]] : []);
// A separate socket prevents read-only discovery from probing the live fleet.
// macOS's long TMPDIR exceeds the Unix socket path limit once tmux appends its suffix.
const socketRoot = () => fs.mkdtempSync(path.join(process.platform === 'darwin' ? '/tmp' : os.tmpdir(), 'amux-lc-'));
export default defineConfig({
  ...base,
  testDir: '..',
  testIgnore: ['**/live-*.spec.ts'],
  workers: 3, // independent desktop/mobile/Safari servers may run concurrently
  fullyParallel: false,
  retries: 0,
  projects: base.projects!.map((project, index) => ({
    ...project, workers: 1, // serialize every mutation within this project
    use: { ...project.use, baseURL: `https://localhost:${firstPort + index * 10}` },
  })),
  webServer: (base.webServer as any[]).map((server, index) => ({
    ...server,
    command: `bash ${path.join(__dirname, 'serve.sh')}`,
    url: `https://localhost:${firstPort + index * 10}/health`,
    env: { ...server.env, AMUX_LIFECYCLE_BROWSER_TTL_S: process.env.AMUX_LIFECYCLE_BROWSER_TTL_S || '', AMUX_RS_PORT: String(firstPort + index * 10), TMUX_TMPDIR: socketRoot() },
  })).filter((_, index) => !selected.length || selected.includes(base.projects![index].name!)),
  outputDir: path.join(output, 'browser-artifacts'),
  reporter: [
    ['line'],
    ['json', { outputFile: path.join(output, 'browser.json') }],
    ['html', { outputFolder: path.join(output, 'browser-report'), open: 'never' }],
  ],
  use: { ...base.use, trace: 'on', screenshot: 'on', video: 'on' },
});
