// A link to a file on this amux reads as the file's path (Ethan, 2026-11-06:
// "it should just be a clickable file path"). Asked for "a link to the plan",
// mixpeek-research wrote https://<this amux>/f/Users/.../PLAN.md, and the
// worker view, Messages and Chat all showed that server URL. Each renderer now
// shows the path; the link still opens the file here. A link to another site
// is unchanged.
import { test, expect } from './fixtures';

test('a /f/ link to this amux shows its path in every renderer; other links stay URLs', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => ['_ownFilePath', '_linkifyUrls', '_linkWrap', 'renderMarkdown']
    .every(f => typeof (window as any)[f] === 'function'));
  const got = await page.evaluate(() => {
    const w = window as any;
    const file = '/Users/ethan/Dev/mixpeek/research/agent-search/adaptive-planning/PLAN.md';
    const own = location.origin + '/f' + file;
    const other = 'https://github.com/mixpeek/mixpeek/blob/main/PLAN.md';
    const text = (html: string) => { const d = document.createElement('div'); d.innerHTML = html; return d.textContent || ''; };
    const md = w.renderMarkdown('- **Plan:** ' + own + '\n- **GitHub:** ' + other);
    return {
      path: w._ownFilePath(own),
      notOwn: w._ownFilePath(other),
      urls: text(w._linkifyUrls('Plan: ' + own + ' and ' + other)),
      term: text(w._linkWrap({ type: 'url', value: own }, own)),
      termOther: text(w._linkWrap({ type: 'url', value: other }, other)),
      md: text(md),
      mdFileLink: /class="md-file-link"[^>]*data-file="\/Users\/ethan\/Dev\/mixpeek/.test(md),
      file, own, other,
    };
  });
  expect(got.path).toBe(got.file);
  expect(got.notOwn).toBe('');
  expect(got.urls).toContain(got.file);
  expect(got.urls).not.toContain(got.own);
  expect(got.urls, 'a link to another site stays a URL').toContain(got.other);
  expect(got.term).toBe(got.file);
  expect(got.termOther).toBe(got.other);
  expect(got.md).toContain(got.file);
  expect(got.md).not.toContain(got.own);
  expect(got.md).toContain(got.other);
  expect(got.mdFileLink, 'markdown opens it in the file viewer').toBe(true);
});
