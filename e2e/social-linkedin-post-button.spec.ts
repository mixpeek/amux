// The LinkedIn publisher clicks the Post button of the composer in view. It took
// the first match, so the caption went into the visible composer and the click
// into a hidden, empty copy the media step leaves behind, and LinkedIn published
// a video with no caption three times (amux-gtm, 2026-11-02). Runs the shipped
// JS from social_browser.rs in a real browser.
import { test, expect } from './fixtures';
import * as fs from 'fs';
import * as path from 'path';

const src = fs.readFileSync(path.join(__dirname, '../crates/amux-server/src/api/social_browser.rs'), 'utf8');
const js = JSON.parse(`"${/const POST_BUTTON_JS: &str = "(.*)";/.exec(src)![1]}"`);

test('Post means the visible composer, and a disabled one is not clicked', async ({ page }) => {
  const pick = async (html: string) => {
    await page.setContent(html);
    return page.evaluate(code => { const b = (0, eval)(code); return b ? b.id : null; }, js);
  };
  expect(await pick('<div style="display:none"><button id="hidden">Post</button></div><button id="shown">Post</button>')).toBe('shown');
  expect(await pick('<div style="visibility:hidden"><button id="hidden">Post</button></div><button id="shown">Post</button>')).toBe('shown');
  expect(await pick('<button id="old">Post</button><button id="new">Post</button>')).toBe('new');
  expect(await pick('<div style="display:none"><button id="hidden">Post</button></div><button id="shown" disabled>Post</button>')).toBe(null);
  expect(await pick('<button id="other">Posts</button>')).toBe(null);
});
