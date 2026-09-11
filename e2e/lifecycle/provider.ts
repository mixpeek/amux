import { expect, Page } from '@playwright/test';

export const lifecycleProvider = process.env.AMUX_LIFECYCLE_PROVIDER || 'claude';
if (!['claude', 'gemini', 'codex', 'ollama'].includes(lifecycleProvider)) {
  throw new Error(`Unsupported lifecycle provider: ${lifecycleProvider}`);
}
export const lifecyclePrefix = `lc-${lifecycleProvider === 'claude' ? 'sonnet' : lifecycleProvider}-`;
export function expectLifecycleWorker(worker: any) {
  expect(worker, 'the requested real worker must exist').toBeTruthy();
  expect(worker.provider || 'claude').toBe(lifecycleProvider);
  if (lifecycleProvider === 'claude') expect(`${worker.model} ${worker.flags}`).toMatch(/sonnet/i);
}
export async function selectLifecycleProvider(page: Page) {
  await page.locator(`#create-provider-${lifecycleProvider}`).click();
  if (lifecycleProvider === 'claude') await page.locator('#create-model').selectOption('sonnet');
}
export async function expectLifecycleTerminal(page: Page) {
  const identity = lifecycleProvider === 'claude' ? /Sonnet [0-9.]+(?: with [^\n]+)?[·•]/i
    : lifecycleProvider === 'gemini' ? /Gemini CLI v[0-9.]+/i : /OpenAI Codex/i;
  await expect(page.locator('#peek-body')).toContainText(identity, { timeout: 120_000 });
}
