// AA-40: a skill created or edited in the Skills tab must reach the places
// that use it: the composer's `/` dropdown, the command file Claude Code reads,
// and Claude Code itself. Before the fix a Skills-tab skill lived only in the
// amux table, so it was missing from the dropdown and Claude Code answered
// "Unknown command" (Ethan's screenshot of /pr-merge, 2026-10-10).
//
// The e2e server's CLAUDE_CONFIG_DIR points into its throwaway home
// (playwright.config.ts), so this never writes the developer's real
// ~/.claude/commands; the test asserts that before trusting any path.
//
// Recognition by REAL Claude Code runs only with AMUX_E2E_REAL_CLAUDE=1 and a
// `claude` on PATH (it costs a model call and needs a signed-in CLI, which CI
// does not have). It runs `claude -p /<name>` in a project whose
// .claude/commands is a symlink to the folder the server wrote, so the file
// Claude Code reads is the exact file amux produced, and checks the reply
// carries the token from the skill body, before and after an edit, and not
// after a delete.
import { test, expect, Page } from './fixtures';
import { execFileSync } from 'child_process';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

declare function switchView(view: string): void;

const workers = [{ name: 'skills-e2e', provider: 'claude', model: 'sonnet', running: true, status: 'idle', dir: '/tmp' }];

function realClaude(): boolean {
  if (process.env.AMUX_E2E_REAL_CLAUDE !== '1') return false;
  try { execFileSync('claude', ['--version'], { stdio: 'ignore' }); return true; } catch { return false; }
}

// Ask real Claude Code to run /<name> from a project whose commands folder IS
// the folder the server wrote. --setting-sources project keeps the developer's
// user hooks out of a test run.
function claudeRuns(commandsDir: string, name: string): string {
  const proj = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-e2e-skill-proj-'));
  fs.mkdirSync(path.join(proj, '.claude'));
  fs.symlinkSync(commandsDir, path.join(proj, '.claude', 'commands'));
  const env = { ...process.env };
  for (const k of Object.keys(env)) if (k.startsWith('AMUX_')) delete env[k];
  try {
    const out = execFileSync('claude', ['-p', `/${name}`, '--setting-sources', 'project', '--model', 'haiku'],
      { cwd: proj, env, encoding: 'utf8', timeout: 120_000 });
    // Printed so a pass shows that Claude Code really ran, not that it was skipped.
    console.log(`[real-claude] /${name} -> ${out.trim().slice(0, 200)}`);
    return out;
  } finally {
    fs.rmSync(proj, { recursive: true, force: true });
  }
}

async function openSkills(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, (r) => r.fulfill({ json: workers }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).editSkill === 'function');
  await page.evaluate(() => switchView('skills'));
  await expect(page.locator('#skills-tab-sections')).toBeVisible();
}

// Save through the editor exactly as a person does, and return the server's
// answer so the test reads where the command file went from the product.
async function saveInEditor(page: Page, content: string): Promise<{ command_file: string | null; command_file_error?: string }> {
  await page.locator('#skill-edit-content').fill(content);
  const [resp] = await Promise.all([
    page.waitForResponse((r) => /\/api\/skills\/[^/]+$/.test(new URL(r.url()).pathname) && r.request().method() === 'POST'),
    page.locator('#skill-edit-modal').getByRole('button', { name: 'Save' }).click(),
  ]);
  expect(resp.status(), await resp.text()).toBe(200);
  await expect(page.locator('#skill-edit-modal')).not.toHaveClass(/active/);
  return resp.json();
}

// The composer's `/` dropdown, read the way a person sees it.
async function slashOffers(page: Page, prefix: string): Promise<string[]> {
  await page.evaluate((w) => {
    eval('sessions=' + JSON.stringify(w) + '; render();');
    (window as any).openPeek('skills-e2e');
    (window as any)._stopPeekPoll();
  }, workers);
  const input = page.locator('#peek-cmd-input');
  await expect(input).toBeVisible();
  await input.fill('');
  await input.pressSequentially('/');
  await input.pressSequentially(prefix.slice(1));
  const list = page.locator('#slash-ac-list');
  await expect(list).toHaveClass(/open/);
  const rows = await list.locator('.ac-item').allInnerTexts();
  await page.evaluate(() => (window as any).closePeek && (window as any).closePeek());
  return rows;
}

test('a skill created and edited in the Skills tab is offered, written and recognized', async ({ page }, info) => {
  test.setTimeout(240_000);
  const name = `e2e-skill-${info.project.name}-${Date.now().toString(36)}`;
  const body = (token: string, desc: string) =>
    `---\ndescription: ${desc}\n---\nReply with exactly the text ${token} and nothing else.\n`;
  await openSkills(page);

  // ── Create ──
  await page.getByRole('button', { name: /\+ New/ }).first().click();
  await expect(page.locator('#skill-edit-modal')).toHaveClass(/active/);
  await page.locator('#skill-edit-name').fill(name);
  const created = await saveInEditor(page, body('SKILL-TOKEN-V1', 'first version'));
  expect(created.command_file, created.command_file_error).toBeTruthy();
  const file = created.command_file as string;
  // Never trust a path that could be the developer's real config.
  expect(file.startsWith(path.join(os.homedir(), '.claude')), file).toBe(false);
  expect(path.basename(file)).toBe(`${name}.md`);
  expect(fs.readFileSync(file, 'utf8')).toBe(body('SKILL-TOKEN-V1', 'first version'));

  await expect(page.locator('#skills-tab-sections')).toContainText(`/${name}`);
  await expect.poll(async () => (await slashOffers(page, `/${name}`)).join(' | '), { timeout: 15_000 })
    .toContain('first version');

  const claude = realClaude() && info.project.name === 'desktop';
  console.log(`[real-claude] ${claude ? 'running' : 'skipped (set AMUX_E2E_REAL_CLAUDE=1 with claude on PATH, desktop project)'}`);
  if (claude) expect(claudeRuns(path.dirname(file), name)).toContain('SKILL-TOKEN-V1');

  // ── Update ──
  await page.evaluate(() => switchView('skills'));
  await page.evaluate((n) => (window as any).editSkill(n), name);
  await expect(page.locator('#skill-edit-name')).toHaveValue(name);
  await expect(page.locator('#skill-edit-content')).toHaveValue(body('SKILL-TOKEN-V1', 'first version'));
  const updated = await saveInEditor(page, body('SKILL-TOKEN-V2', 'second version'));
  expect(updated.command_file).toBe(file);
  expect(fs.readFileSync(file, 'utf8')).toBe(body('SKILL-TOKEN-V2', 'second version'));
  await expect.poll(async () => (await slashOffers(page, `/${name}`)).join(' | '), { timeout: 15_000 })
    .toContain('second version');
  if (claude) {
    const out = claudeRuns(path.dirname(file), name);
    expect(out).toContain('SKILL-TOKEN-V2');
    expect(out).not.toContain('SKILL-TOKEN-V1');
  }

  // ── Delete ──
  await page.evaluate(() => switchView('skills'));
  await page.evaluate((n) => (window as any).editSkill(n), name);
  await expect(page.locator('#skill-delete-btn')).toBeVisible();
  const [del] = await Promise.all([
    page.waitForResponse((r) => r.request().method() === 'DELETE' && r.url().includes(`/api/skills/${name}`)),
    page.locator('#skill-delete-btn').click(),
  ]);
  expect(await del.json()).toMatchObject({ deleted: true, command_file_removed: true });
  expect(fs.existsSync(file)).toBe(false);
  await expect(page.locator('#skills-tab-sections')).not.toContainText(`/${name}`);
  if (claude) expect(claudeRuns(path.dirname(file), name)).not.toContain('SKILL-TOKEN-V2');
});
