import {test, expect} from './fixtures';
import type {Page} from '@playwright/test';

const workers = ['draft-amux', 'draft-other'].map(name => ({name,provider:'claude',model:'sonnet',running:true,status:'active',dir:'/tmp'}));
async function prepare(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done','1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json:workers}));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(workers => {
    eval('sessions='+JSON.stringify(workers)+'; render();');
    if (!document.querySelector('.card[data-session="draft-amux"]')?.classList.contains('expanded')) (window as any).toggle('draft-amux');
    (window as any).doSend = async () => 'sent';
  },workers);
}
async function open(page: Page, name='draft-amux') {
  await page.evaluate(name => { (window as any).openPeek(name); (window as any)._stopPeekPoll(); },name);
  await expect(page.locator('#peek-cmd-input')).toBeVisible();
}
const stored = (page:Page,name='draft-amux') => page.evaluate(name => (window as any)._draftGet(name),name);

test('details send clears a partially mirrored card draft before background or reload', async ({page},info) => {
  await prepare(page); await open(page);
  const input=page.locator('#peek-cmd-input');
  await input.fill('go thru all browser profiles');
  await page.waitForTimeout(300);
  await input.fill('go thru all browser profiles in amux and clean up the ones that are unused');
  await page.evaluate(() => (window as any).sendPeekCmd());
  await page.evaluate(() => { (window as any).closePeek(); window.dispatchEvent(new Event('pagehide')); });
  await expect(page.locator('#input-draft-amux')).toHaveValue('');
  expect(await stored(page)).toBe('');
  await page.reload();
  await expect(page.locator('#input-draft-amux')).toHaveValue('');
  await page.screenshot({path:info.outputPath('accepted-draft-cleared.png')});
});

test('edits transfer immediately between card and details and stay isolated by worker', async ({page}) => {
  await prepare(page);
  await page.locator('#input-draft-amux').fill('amux unsent');
  await open(page);
  await expect(page.locator('#peek-cmd-input')).toHaveValue('amux unsent');
  await page.locator('#peek-cmd-input').fill('latest amux edit');
  await open(page,'draft-other');
  await expect(page.locator('#peek-cmd-input')).toHaveValue('');
  await page.locator('#peek-cmd-input').fill('other unsent');
  await open(page);
  await expect(page.locator('#peek-cmd-input')).toHaveValue('latest amux edit');
  expect(await stored(page,'draft-other')).toBe('other unsent');
});

test('a delayed acceptance preserves the newer draft after switching workers', async ({page}) => {
  await prepare(page); await open(page);
  await page.locator('#peek-cmd-input').fill('accepted old text');
  await page.evaluate(() => {
    (window as any).doSend=()=>new Promise(resolve => (window as any).__accept=()=>resolve('sent'));
    (window as any).__sending=(window as any).sendPeekCmd();
  });
  await page.locator('#peek-cmd-input').fill('new unsent edit');
  await open(page,'draft-other');
  await page.locator('#peek-cmd-input').fill('other worker edit');
  await page.evaluate(async () => { (window as any).__accept(); await (window as any).__sending; });
  await open(page);
  await expect(page.locator('#peek-cmd-input')).toHaveValue('new unsent edit');
  expect(await stored(page,'draft-other')).toBe('other worker edit');
});

test('fullscreen input persists on background and reload without collapsing first', async ({page}) => {
  await prepare(page); await open(page);
  await page.evaluate(() => (window as any)._expandPeekInput());
  await page.locator('#peek-input-fs-ta').fill('fullscreen unsent');
  await page.evaluate(() => window.dispatchEvent(new Event('pagehide')));
  expect(await stored(page)).toBe('fullscreen unsent');
  await page.reload(); await open(page);
  await expect(page.locator('#peek-cmd-input')).toHaveValue('fullscreen unsent');
});

test('storage events share edits and accepted clears with another tab', async ({page,context}) => {
  await prepare(page); await open(page);
  const other=await context.newPage();
  await prepare(other); await open(other);
  await page.locator('#peek-cmd-input').fill('shared worker draft');
  await expect(other.locator('#peek-cmd-input')).toHaveValue('shared worker draft');
  await other.evaluate(() => (window as any).sendPeekCmd());
  await expect(page.locator('#peek-cmd-input')).toHaveValue('');
  await page.evaluate(() => { (window as any).closePeek(); window.dispatchEvent(new Event('pagehide')); });
  expect(await stored(other)).toBe('');
  await other.close();
});

test('local send refusal retains the same draft in both views', async ({page}) => {
  await prepare(page); await open(page);
  await page.locator('#peek-cmd-input').fill('keep until accepted');
  await page.evaluate(async () => { (window as any).doSend=async()=> 'local-failed'; await (window as any).sendPeekCmd(); (window as any).closePeek(); });
  await expect(page.locator('#input-draft-amux')).toHaveValue('keep until accepted');
  expect(await stored(page)).toBe('keep until accepted');
});

test('retyping identical text creates a newer draft that an old receipt cannot consume', async ({page}) => {
  await prepare(page); await open(page);
  await page.locator('#peek-cmd-input').fill('same text');
  await page.evaluate(() => {
    (window as any).doSend=()=>new Promise(resolve => (window as any).__accept=()=>resolve('sent'));
    (window as any).__sending=(window as any).sendPeekCmd();
  });
  await page.locator('#peek-cmd-input').fill('');
  await page.locator('#peek-cmd-input').fill('same text');
  await page.evaluate(async () => { (window as any).__accept(); await (window as any).__sending; });
  await expect(page.locator('#peek-cmd-input')).toHaveValue('same text');
  expect(await stored(page)).toBe('same text');
});

test('closing a stale view cannot restore a draft already cleared in storage', async ({page}) => {
  await prepare(page); await open(page);
  await page.locator('#peek-cmd-input').fill('accepted in another context');
  await page.evaluate(() => {
    // Storage changes synchronously; its event to this context arrives later.
    localStorage.removeItem('amux_draft_draft-amux');
    (window as any).closePeek(); window.dispatchEvent(new Event('pagehide'));
  });
  expect(await stored(page)).toBe('');
  await open(page);
  await expect(page.locator('#peek-cmd-input')).toHaveValue('');
});

test('storage failure keeps an in-page draft and retries persistence on background', async ({page}) => {
  await prepare(page); await open(page);
  await page.evaluate(() => {
    const original=Storage.prototype.setItem;
    (window as any).__restoreStorage=()=>Storage.prototype.setItem=original;
    Storage.prototype.setItem=function(key,value) {
      if(key==='amux_draft_draft-amux')throw new DOMException('Test storage failure','QuotaExceededError');
      return original.call(this,key,value);
    };
  });
  await page.locator('#peek-cmd-input').fill('draft survives storage failure');
  await expect(page.locator('#input-draft-amux')).toHaveValue('draft survives storage failure');
  expect(await stored(page)).toBe('draft survives storage failure');
  await page.evaluate(() => { (window as any).__restoreStorage(); window.dispatchEvent(new Event('pagehide')); });
  await page.reload(); await open(page);
  await expect(page.locator('#peek-cmd-input')).toHaveValue('draft survives storage failure');
});

test('embedded grid details share the same draft and accepted clear', async ({page}) => {
  await prepare(page); await open(page);
  await page.evaluate(() => {
    const frame=document.createElement('iframe'); frame.id='draft-grid'; frame.src='/?peekEmbed=draft-amux'; document.body.appendChild(frame);
  });
  const frame=page.frameLocator('#draft-grid');
  await expect(frame.locator('#peek-cmd-input')).toBeVisible();
  await page.locator('#peek-cmd-input').fill('shared with grid');
  await expect(frame.locator('#peek-cmd-input')).toHaveValue('shared with grid');
  await frame.locator('#peek-cmd-input').fill('edited in grid');
  await expect(page.locator('#peek-cmd-input')).toHaveValue('edited in grid');
  await page.evaluate(() => (window as any).sendPeekCmd());
  await expect(frame.locator('#peek-cmd-input')).toHaveValue('');
});

test('history and chip selections persist as edits', async ({page}) => {
  await prepare(page);
  await page.evaluate(() => (window as any).chipToInput('draft-amux','chip draft'));
  await open(page);
  await expect(page.locator('#peek-cmd-input')).toHaveValue('chip draft');
  await page.evaluate(() => (window as any)._pickCmdHistory('history draft'));
  await page.reload(); await open(page);
  await expect(page.locator('#peek-cmd-input')).toHaveValue('history draft');
});

test('draft persistence still works when randomUUID is unavailable', async ({page}) => {
  await prepare(page); await open(page);
  await page.evaluate(() => Object.defineProperty(crypto,'randomUUID',{value:undefined,configurable:true}));
  await page.locator('#peek-cmd-input').fill('portable draft');
  expect(await stored(page)).toBe('portable draft');
  await expect(page.locator('#input-draft-amux')).toHaveValue('portable draft');
});
