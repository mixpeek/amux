// Ethan, 2026-10-29: "folders that we watch and migrate them to and from our
// selected hdd to local mac drive based on access patterns ... configurable in
// the disk amux tab". Nothing here is stubbed: the real server moves real
// files. The folders are throwaway trees made by this spec, and the cold root
// is a temp folder, or a folder under AMUX_E2E_TIER_ROOT (point it at a real
// external drive, e.g. /Volumes/Bulk/amux-e2e-tier, to cross a disk boundary).
import {test, expect} from './fixtures';
import type {Page} from './fixtures';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import {createHash} from 'node:crypto';
import {execFileSync} from 'node:child_process';

test.describe.configure({mode: 'serial'});

const DAY = 86_400;
let base = '';
let cold = '';

test.beforeAll(async ({}, info) => {
  base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), `amux-tier-${info.project.name}-`)));
  const root = process.env.AMUX_E2E_TIER_ROOT;
  cold = root ? path.join(root, `${info.project.name}-${process.pid}-${Date.now()}`) : path.join(base, 'cold-drive');
});

test.afterAll(async () => {
  for (const d of [base, cold]) if (d) fs.rmSync(d, {recursive: true, force: true});
});

/** A small project-like tree: nested folders, a binary file, a hard link. */
function makeTree(root: string, ageDays = 0) {
  fs.mkdirSync(path.join(root, 'runs/2026-10'), {recursive: true});
  fs.writeFileSync(path.join(root, 'README.md'), '# fixture\n');
  fs.writeFileSync(path.join(root, 'runs/2026-10/output.bin'), Buffer.alloc(256 * 1024, 7));
  fs.writeFileSync(path.join(root, 'runs/notes.txt'), 'notes');
  fs.linkSync(path.join(root, 'README.md'), path.join(root, 'runs/README.hardlink'));
  if (ageDays) {
    const t = Date.now() / 1000 - ageDays * DAY;
    const walk = (d: string) => {
      for (const e of fs.readdirSync(d, {withFileTypes: true})) {
        const p = path.join(d, e.name);
        if (e.isDirectory()) walk(p);
        fs.utimesSync(p, t, t);
      }
      fs.utimesSync(d, t, t);
    };
    walk(root);
  }
}

/** relative path -> sha256, reading through a link at the root. */
function digest(root: string): Record<string, string> {
  const out: Record<string, string> = {};
  const walk = (d: string, rel: string) => {
    for (const e of fs.readdirSync(d, {withFileTypes: true}).sort((a, b) => a.name.localeCompare(b.name))) {
      const p = path.join(d, e.name), r = path.join(rel, e.name);
      if (e.isDirectory()) walk(p, r);
      else out[r] = createHash('sha256').update(fs.readFileSync(p)).digest('hex');
    }
  };
  walk(root, '');
  return out;
}

const card = (page: Page, p: string) => page.locator(`.tier-folder[data-tier-path="${p}"]`);

async function openDisk(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/#view=disk', {waitUntil: 'domcontentloaded'});
  await expect(page.locator('#tier-title')).toBeVisible();
}

async function chooseColdRoot(page: Page, p: string) {
  await page.selectOption('#tier-drive-pick', '');
  await page.click('#tier-drive-save');
  const input = page.locator('#modal-prompt-input');
  await input.fill(p);
  await input.press('Enter');
}

async function watch(page: Page, p: string, days = 14) {
  await page.fill('#tier-add-path', p);
  await page.fill('#tier-add-days', String(days));
  await page.click('#tier-add-btn');
  await expect(card(page, p)).toBeVisible();
  // The add queues a check that measures it.
  await expect(card(page, p)).toContainText('files', {timeout: 30_000});
}

test('watch a folder, move it to the cold drive and back, from the Disk tab', async ({page}) => {
  const src = path.join(base, 'projects/customer-a/data');
  makeTree(src);
  const before = digest(src);
  await openDisk(page);
  await expect(page.locator('#tier-empty')).toBeVisible();

  await chooseColdRoot(page, cold);
  await expect(page.locator('#tier-drive-text')).toContainText(cold);

  await watch(page, src);
  const c = card(page, src);
  await expect(c).toHaveAttribute('data-tier-location', 'mac');
  await expect(c.locator('[data-tier-state]')).toHaveText('On this Mac');

  await c.locator('[data-tier-act="to-hdd"]').click();
  await expect(c).toHaveAttribute('data-tier-location', 'hdd', {timeout: 60_000});
  await expect(c.locator('[data-tier-state]')).toHaveText('On the cold drive');
  await expect(c.locator('[data-tier-note="moved"]')).toContainText('moved to the cold drive');
  await expect(c.locator('[data-tier-note="reads"]')).toContainText('comes back at 3, or on any write');

  // What a user actually depends on: the old path still works and holds the same bytes.
  expect(fs.lstatSync(src).isSymbolicLink()).toBe(true);
  const target = fs.readlinkSync(src);
  expect(target.startsWith(cold + '/')).toBe(true);
  expect(digest(src)).toEqual(before);
  expect(digest(target)).toEqual(before);
  expect(fs.statSync(path.join(target, 'runs/README.hardlink')).nlink).toBe(2);
  expect(fs.existsSync(src + '.amux-tier-old')).toBe(false);

  // It survives a reload.
  await page.reload({waitUntil: 'domcontentloaded'});
  await expect(card(page, src)).toHaveAttribute('data-tier-location', 'hdd');

  await card(page, src).locator('[data-tier-act="to-mac"]').click();
  await expect(card(page, src)).toHaveAttribute('data-tier-location', 'mac', {timeout: 60_000});
  const m = fs.lstatSync(src);
  expect(m.isDirectory() && !m.isSymbolicLink()).toBe(true);
  expect(digest(src)).toEqual(before);
  expect(fs.existsSync(target)).toBe(false);
  for (const s of ['.amux-tier-link', '.amux-tier-new', '.amux-tier-old']) expect(fs.existsSync(src + s)).toBe(false);

  // Stop watching moves nothing.
  await card(page, src).locator('[data-tier-act="unwatch"]').click();
  await page.locator('#modal-btns button.primary').click();
  await expect(card(page, src)).toHaveCount(0);
  expect(digest(src)).toEqual(before);
});

test('the policy moves an idle folder out, and brings it back once it is used', async ({page}) => {
  const src = path.join(base, 'projects/customer-b/archive');
  makeTree(src, 30);
  const before = digest(src);
  await openDisk(page);
  await expect(page.locator('#tier-drive-text')).toContainText(cold);
  await expect(page.locator('#tier-auto')).toBeChecked();
  await watch(page, src, 14);
  const c = card(page, src);
  // Added folders are checked right away; 30 idle days is past the 14-day limit.
  await page.click('#tier-run');
  await expect(c).toHaveAttribute('data-tier-location', 'hdd', {timeout: 60_000});
  await expect(c.locator('[data-tier-note="moved"]')).toContainText('by policy');
  expect(digest(src)).toEqual(before);

  // A Check now right away must not bounce it back: the move itself is not a use.
  await page.click('#tier-run');
  await expect(page.locator('#tier-run')).toBeEnabled({timeout: 30_000});
  await expect(c).toHaveAttribute('data-tier-location', 'hdd');

  // A worker uses it through the old path.
  await page.waitForTimeout(11_000);
  fs.writeFileSync(path.join(src, 'runs/new-result.json'), '{"ok":true}');
  await page.click('#tier-run');
  await expect(c).toHaveAttribute('data-tier-location', 'mac', {timeout: 60_000});
  await expect(c.locator('[data-tier-note="moved"]')).toContainText('this Mac');
  expect(fs.lstatSync(src).isSymbolicLink()).toBe(false);
  expect(fs.readFileSync(path.join(src, 'runs/new-result.json'), 'utf8')).toBe('{"ok":true}');
  expect({...digest(src), 'runs/new-result.json': undefined}).toEqual({...before, 'runs/new-result.json': undefined});
});

test('refusals are specific and never read as success', async ({page}) => {
  await openDisk(page);
  const msg = page.locator('#tier-add-msg');

  await page.fill('#tier-add-path', path.join(base, 'does-not-exist'));
  await page.click('#tier-add-btn');
  await expect(msg).toContainText('does not exist');
  await page.fill('#tier-add-path', 'relative/folder');
  await page.click('#tier-add-btn');
  await expect(msg).toContainText('absolute path');

  // In use: a file is held open inside the folder.
  const busy = path.join(base, 'busy');
  makeTree(busy);
  const fd = fs.openSync(path.join(busy, 'README.md'), 'r');
  try {
    await watch(page, busy);
    await page.fill('#tier-add-path', busy);
    await page.click('#tier-add-btn');
    await expect(msg).toContainText('already watched');
    await page.fill('#tier-add-path', path.dirname(busy) + '/busy/runs');
    await page.click('#tier-add-btn');
    await expect(msg).toContainText('overlaps');
    await card(page, busy).locator('[data-tier-act="to-hdd"]').click();
    await expect(card(page, busy).locator('[data-tier-note="refused"]')).toContainText('open inside', {timeout: 60_000});
    await expect(card(page, busy)).toHaveAttribute('data-tier-location', 'mac');
    expect(fs.lstatSync(busy).isSymbolicLink()).toBe(false);
  } finally {
    fs.closeSync(fd);
  }

  // Inside a git repo: tracked files, and an untracked folder git does not ignore.
  const repo = path.join(base, 'repo');
  fs.mkdirSync(path.join(repo, 'tracked'), {recursive: true});
  fs.writeFileSync(path.join(repo, 'tracked/a.txt'), 'a');
  const git = (...a: string[]) => execFileSync('git', ['-C', repo, ...a], {stdio: 'ignore'});
  git('init', '-q');
  git('add', 'tracked/a.txt');
  git('-c', 'user.email=e2e@amux', '-c', 'user.name=e2e', 'commit', '-qm', 'fixture');
  makeTree(path.join(repo, 'loose'));
  for (const [dir, says] of [[path.join(repo, 'tracked'), 'tracked'], [path.join(repo, 'loose'), 'git status']] as const) {
    await watch(page, dir);
    await card(page, dir).locator('[data-tier-act="to-hdd"]').click();
    await expect(card(page, dir).locator('[data-tier-note="refused"]')).toContainText(says, {timeout: 60_000});
    await expect(card(page, dir)).toHaveAttribute('data-tier-location', 'mac');
  }

  // A drive that is not connected is refused when it is chosen.
  await chooseColdRoot(page, '/Volumes/amux-e2e-no-such-drive/tier');
  await expect(page.locator('#toast, .toast').first()).toContainText('not on a connected drive');
  await expect(page.locator('#tier-drive-text')).toContainText(cold);
});

test('the tiering policy is set from the Disk tab, validated, and kept', async ({page}) => {
  await openDisk(page);
  await page.fill('#tier-low', '30');
  await page.fill('#tier-target', '20');
  await page.click('#tier-policy-save');
  await expect(page.locator('#toast')).toContainText('must be above the low-space level');
  await page.fill('#tier-recall', '2');
  await page.fill('#tier-low', '12');
  await page.fill('#tier-target', '30');
  await page.click('#tier-policy-save');
  await expect(page.locator('#toast')).toContainText('Policy saved');
  await page.reload({waitUntil: 'domcontentloaded'});
  await expect(page.locator('#tier-recall')).toHaveValue('2');
  await expect(page.locator('#tier-low')).toHaveValue('12');
  await expect(page.locator('#tier-target')).toHaveValue('30');
  // Put the default back for the rest of the file.
  await page.fill('#tier-recall', '3');
  await page.fill('#tier-low', '15');
  await page.fill('#tier-target', '25');
  await page.click('#tier-policy-save');
  await expect(page.locator('#toast')).toContainText('Policy saved');
});

test('a double click queues one move', async ({page}) => {
  const src = path.join(base, 'double');
  makeTree(src);
  await openDisk(page);
  await watch(page, src);
  const accepted: number[] = [];
  page.on('response', r => { if (r.url().endsWith('/api/tiering/move')) accepted.push(r.status()); });
  await card(page, src).locator('[data-tier-act="to-hdd"]').dblclick();
  await expect(card(page, src)).toHaveAttribute('data-tier-location', 'hdd', {timeout: 60_000});
  expect(accepted.filter(s => s === 202)).toHaveLength(1);
  expect(fs.lstatSync(src).isSymbolicLink()).toBe(true);
});

test('the panel fits a phone: no sideways scroll, real touch targets', async ({page}) => {
  test.skip((page.viewportSize()?.width || 1280) > 600, 'phone layout check');
  await openDisk(page);
  await page.locator('#tier-panel').scrollIntoViewIfNeeded();
  const fit = await page.locator('#tier-panel').evaluate(el => ({sw: el.scrollWidth, cw: el.clientWidth}));
  expect(fit.sw).toBeLessThanOrEqual(fit.cw + 1);
  for (const b of await page.locator('#tier-panel button').all()) {
    const box = await b.boundingBox();
    if (box) expect(box.height).toBeGreaterThanOrEqual(43);
  }
  await page.screenshot({path: test.info().outputPath('tier-phone.png'), fullPage: false});
});
