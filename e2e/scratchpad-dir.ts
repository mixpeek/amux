import type { APIRequestContext } from '@playwright/test';

// The scratchpad folder this test server uses: the `dir` of
// GET /api/scratchpad/config. playwright.config.ts gives every isolated
// server its own folder, so a test never writes into the owner's real
// ~/Vault/Scratchpad (150 leaked test notes were cleaned by hand, 2026-10-18).
export async function scratchpadDir(request: APIRequestContext, headers: Record<string, string>): Promise<string> {
  const d = await (await request.get('/api/scratchpad/config', { headers })).json();
  const dir = String(d.dir || '').replace(/\/+$/, '');
  if (!dir || dir.includes('Vault/Scratchpad')) throw new Error('e2e scratchpad dir is the real one (' + dir + '); playwright.config.ts must seed scratchpad.json');
  return dir;
}
