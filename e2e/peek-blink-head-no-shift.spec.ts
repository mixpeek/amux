// Owner, 2026-11-22: the ⏺ at the start of a running step "flickers in and out
// as an agent is currently working. It causes the terminal to do a layout
// shift", and with the block above folded nothing could be clicked until the
// step finished. Claude Code blinks the dot by drawing a coloured space in its
// cell. Both frames below are a real capture of a working pane (tmux
// capture-pane -e, 2026-11-22), trimmed: one with the dot, one blinked off.
// Through the real render pipeline (_peekHtml) they must give the same blocks
// and the same height, folded or not.
import { test, expect } from './fixtures';

const before = '\x1b[38;5;231m⏺\x1b[39m Both tablet jobs passed with 16/16 flows, and the visuals now match\n'
  + '  the landscape run.\n\n';
const after = ' Updating the README, committing, testing and pushing\x1b[38;5;246m · 7s\x1b[39m\n'
  + '\x1b[38;5;246m  ⎿  $ rm .eas/workflows/tmp-tablet-verify.yml\x1b[39m\n'
  + '     \x1b[38;5;246mp=\'.eas/workflows/README.md\'\x1b[39m\n';
const ON = before + '\x1b[38;5;246m⏺\x1b[39m' + after;
const OFF = before + '\x1b[38;5;246m \x1b[39m' + after;

async function setup(page: import('@playwright/test').Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekHtml === 'function');
}
// Render a frame the way the peek does and read back what a person sees.
const frame = (page: import('@playwright/test').Page, raw: string, foldFirst: boolean) => page.evaluate(([r, fold]) => {
  const pre = document.createElement('pre');
  pre.style.cssText = 'width:900px;font-family:var(--font-mono)';
  pre.innerHTML = (window as any)._peekHtml(r);
  document.body.prepend(pre);
  const blocks = [...pre.querySelectorAll('.ptc')];
  if (fold && blocks[0]) blocks[0].classList.add('collapsed');
  const out = {
    heads: blocks.map(b => (b.querySelector('.ptc-head') as HTMLElement).innerText.replace(/\s+/g, ' ').trim().slice(0, 24)),
    height: Math.round(pre.getBoundingClientRect().height),
    // innerText leaves out what display:none hides, so a folded line is absent.
    runningVisible: pre.innerText.includes('$ rm .eas/workflows'),
  };
  pre.remove();
  return out;
}, [raw, foldFirst] as [string, boolean]);

test('a blinked-off dot keeps the running step as its own block, with no shift', async ({ page }) => {
  await setup(page);
  const on = await frame(page, ON, false);
  expect(on.heads).toEqual(['⏺ Both tablet jobs passe', '⏺ Updating the README, c']);
  expect(await frame(page, OFF, false), 'the blank frame matches the dot frame').toEqual(on);
});

test('with the block above folded, the running step stays visible on both frames', async ({ page }) => {
  await setup(page);
  const on = await frame(page, ON, true);
  expect(on.runningVisible).toBe(true);
  expect(await frame(page, OFF, true), 'the blank frame must not fold the running step away').toEqual(on);
});

test('a plain indented line still continues the block above', async ({ page }) => {
  await setup(page);
  const r = await frame(page, before, false);
  expect(r.heads).toEqual(['⏺ Both tablet jobs passe']);
});
