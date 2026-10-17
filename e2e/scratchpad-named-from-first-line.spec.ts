// Scratchpad .md files are named from their own contents — the first non-empty
// line, no LLM (Ethan, 2026-10-17: "make sure the scratchpad md files are named
// according to the contents (first line, no llm needed)"). Both creation paths
// are covered: the quick-capture box (_spCaptureSave) and the editor new-note
// (_scratchpadNewNote -> _fileSave rename). A markdown heading marker is
// stripped, and two notes sharing a first line get distinct names so one never
// overwrites the other.
import { test, expect, Page } from './fixtures';
import { boot } from './lifecycle/evidence';

type Win = Window & { switchView: (v: string) => void; _scratchpadNewNote: () => void;
  _AMUX_AUTH_TOKEN: string };

const DIR = '~/Vault/Scratchpad';
const token = (p: Page) => p.evaluate(() => (window as unknown as Win)._AMUX_AUTH_TOKEN);

async function openScratchpad(p: Page) {
  await boot(p);
  await p.evaluate(() => (window as unknown as Win).switchView('scratchpad'));
  await expect(p.locator('#scratchpad-view')).toBeVisible();
}

// The scratchpad filename whose stored content contains `marker`, or null.
async function nameContaining(p: Page, auth: Record<string, string>, marker: string) {
  const ls = await (await p.request.get('/api/ls?path=' + encodeURIComponent(DIR) + '&hidden=0', { headers: auth })).json();
  for (const e of (ls.entries || []).filter((x: any) => String(x.name).endsWith('.md'))) {
    const f = await p.request.get('/api/file?path=' + encodeURIComponent(DIR + '/' + e.name), { headers: auth });
    if (f.ok() && String((await f.json()).content || '').includes(marker)) return String(e.name);
  }
  return null;
}

// The three projects share the one real ~/Vault/Scratchpad, so every marker is
// scoped by project + time to keep each test finding only its own files.
const mark = (info: { project: { name: string } }) => `${info.project.name}-${Date.now()}`;

test('a quick-capture note is named from its first line', async ({ page }, info) => {
  test.setTimeout(90_000);
  await openScratchpad(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  const m = mark(info);
  // First line becomes the name; the body line must not.
  const first = `Buy milk and eggs ${m}`;
  await page.locator('#sp-capture').fill(`${first}\nand a second line that is not in the name`);
  await page.locator('#sp-capture').press('Meta+Enter');

  await expect.poll(() => nameContaining(page, auth, m), { timeout: 15_000 }).not.toBeNull();
  const name = await nameContaining(page, auth, m);
  expect(name, 'named from the first line, slugified, .md').toBe(`buy-milk-and-eggs-${m}.md`);
  await page.request.delete('/api/file?path=' + encodeURIComponent(DIR + '/' + name), { headers: auth }).catch(() => {});
});

test('an editor note is renamed from its first line, markdown heading stripped', async ({ page }, info) => {
  test.setTimeout(90_000);
  await openScratchpad(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  const m = mark(info);
  await page.evaluate(() => (window as unknown as Win)._scratchpadNewNote());
  await expect(page.locator('#file-edit-ta')).toBeVisible();
  await page.locator('#file-edit-ta').fill(`# Weekly sync agenda ${m}\n\n- item one`);
  await page.locator('#file-save-btn').click();
  await expect(page.locator('#file-save-btn')).toHaveText(/Saved/);

  // The rename is fire-and-forget; poll for the content-named file to appear.
  await expect.poll(() => nameContaining(page, auth, m), { timeout: 15_000 }).not.toBeNull();
  const name = await nameContaining(page, auth, m);
  expect(name, 'the "#" heading marker is stripped from the name').toBe(`weekly-sync-agenda-${m}.md`);
  await page.request.delete('/api/file?path=' + encodeURIComponent(DIR + '/' + name), { headers: auth }).catch(() => {});
});

test('two notes with the same first line get distinct names (no overwrite)', async ({ page }, info) => {
  test.setTimeout(90_000);
  await openScratchpad(page);
  const auth = { Authorization: `Bearer ${await token(page)}` };
  const m = mark(info);
  const first = `Meeting notes ${m}`;
  // Two captures, identical first line, different bodies. Neither may clobber.
  await page.locator('#sp-capture').fill(`${first}\nbody ALPHA ${m}`);
  await page.locator('#sp-capture').press('Meta+Enter');
  await expect.poll(() => nameContaining(page, auth, `ALPHA ${m}`), { timeout: 15_000 }).not.toBeNull();
  await page.locator('#sp-capture').fill(`${first}\nbody BRAVO ${m}`);
  await page.locator('#sp-capture').press('Meta+Enter');
  await expect.poll(() => nameContaining(page, auth, `BRAVO ${m}`), { timeout: 15_000 }).not.toBeNull();

  const a = await nameContaining(page, auth, `ALPHA ${m}`);
  const b = await nameContaining(page, auth, `BRAVO ${m}`);
  expect(a, 'first note named from its first line').toBe(`meeting-notes-${m}.md`);
  expect(b, 'second note stepped so it does not overwrite the first').toBe(`meeting-notes-${m}-2.md`);
  for (const n of [a, b]) await page.request.delete('/api/file?path=' + encodeURIComponent(DIR + '/' + n), { headers: auth }).catch(() => {});
});
