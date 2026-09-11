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

// Closing the create form is optimistic; navigating away before the remaining
// config/start requests finish aborts creation, particularly with YOLO enabled.
export async function createLifecycleWorker(page: Page) {
  const name = await page.locator('#create-name').inputValue();
  const started = page.waitForResponse(r => r.url().endsWith(`/api/sessions/${encodeURIComponent(name)}/start`)
    && r.request().method() === 'POST', { timeout: 90_000 });
  await page.locator('#create-overlay').getByRole('button', { name: 'Create', exact: true }).click();
  const response = await started;
  expect(response.ok(), `${name} startup: ${await response.text()}`).toBe(true);
  await expect(page.locator('#create-overlay')).not.toHaveClass(/active/);
}
