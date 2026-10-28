// The Record tab's three panes (owner's mockup, 2026-10-28): the list, the open
// recording (transcript, notes, audio, details) and its summary, action items,
// workers and calendar event. A filed meeting is written straight into a temp
// recordings folder (its sidecar, transcript and notes, as the server writes
// them), so this runs on the suite's isolated servers, where no transcription
// job runs. Live transcription and the jobs themselves are proven on real
// speech by desktop/test/recorder-live-e2e.py.
import { test, expect, Page } from './fixtures';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

declare function switchView(view: string): void;

async function token(page: Page): Promise<string> {
  return page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
}

function seed(dir: string, id: string) {
  const base = `2026-10-28_10-00-00_${id}`;
  const started = Date.now() - 3600_000;
  fs.writeFileSync(path.join(dir, `${base}.m4a`), Buffer.alloc(64));
  fs.writeFileSync(path.join(dir, `${base}.transcript.json`), JSON.stringify({ id, engine: 'whisper.cpp', segments: [
    { from_ms: 0, to_ms: 4000, text: 'You: Thanks everyone, let us review the recorder progress.' },
    { from_ms: 23_000, to_ms: 30_000, text: 'Them: The macOS recording is stable and recovery after sleep looks good.' },
    { from_ms: 252_000, to_ms: 260_000, text: 'You: Next, worker integration and the pelican codeword.' },
  ] }));
  fs.writeFileSync(path.join(dir, `${base}.md`), '---\nengine: whisper.cpp\n---\n[00:00] You: Thanks everyone\n');
  fs.writeFileSync(path.join(dir, `${base}.notes.md`), '# AMUX Product Sync\n\nThe team reviewed the recorder.\n\n## Decisions\n\n- Ship the hotkey\n');
  const sidecar = {
    kind: 'amux-recording', schema: 1, id, started_at: started, dur_ms: 42 * 60_000, tz_offset_min: -240,
    recorded_at_local: new Date(started).toISOString(), file: `${base}.m4a`, path: path.join(dir, `${base}.m4a`), bytes: 64, mime: 'audio/mp4',
    session: { chunk_kind: 'segment', tracks: ['mic', 'system'], track_files: {}, gaps: [], gap_ms: 0 },
    transcript: { status: 'done', engine: 'whisper.cpp', live: true, md: `${base}.md`, segments_file: `${base}.transcript.json`, words: 30, excerpt: 'Thanks everyone' },
    notes: { status: 'done', md: `${base}.notes.md`, title: 'AMUX Product Sync', summary: 'The team reviewed the recorder and planned worker integration.',
      key_points: ['macOS recording is stable', 'Workers can be tagged'], decisions: ['Ship the hotkey'],
      action_items: [{ text: 'Finish worker tagging UI', owner: 'Sarah', done: false }, { text: 'Prepare beta plan', owner: 'Ethan', done: false }] },
    about: { title: 'AMUX Product Sync', workers: ['rec-panes-w'], groups: [], why: 'It is about the recorder.' },
    meeting: { source: 'google', title: 'AMUX Product Sync', start: '2026-10-28T10:00:00-04:00', end: '2026-10-28T11:00:00-04:00',
      attendees: ['ethan@example.com', 'sarah@example.com', 'james@example.com', 'maria@example.com', 'dan@example.com', 'lee@example.com'],
      meeting_url: 'https://zoom.us/j/1' },
    user_notes: '[04:12] worker integration matters',
  };
  fs.writeFileSync(path.join(dir, `${base}.json`), JSON.stringify(sidecar));
  return sidecar;
}

async function setup(page: Page) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-e2e-recpanes-'));
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); });
  await page.goto('/');
  const tok = await token(page);
  const h = { Authorization: `Bearer ${tok}` };
  expect((await page.request.post('/api/recordings/config', { headers: h, data: { dir } })).status()).toBe(200);
  // A worker to suggest and send to (created stopped: nothing starts).
  await page.request.post('/api/sessions', { headers: h, data: { name: 'rec-panes-w', dir: os.tmpdir(), desc: 'Catalogues meetings for presales.' } });
  const id = 'rpanes' + Date.now().toString(36);
  const sc = seed(dir, id);
  await page.waitForFunction(() => typeof (window as any)._recorderInit === 'function');
  await page.evaluate(() => switchView('record'));
  return { dir, id, sc, h };
}

const phone = (page: Page) => (page.viewportSize()?.width || 1000) <= 600;

test('a filed meeting opens into its transcript, notes, summary, action items, workers and calendar event', async ({ page }) => {
  const { dir, id, h } = await setup(page);
  const row = page.locator(`.recorder-item[data-rec-id="${id}"]`);
  await expect(row).toContainText('AMUX Product Sync');
  await expect(row.locator('.rec-item-people')).toHaveText('6');
  await row.click();
  if (phone(page)) await expect(page.locator('.rec-list-pane')).toBeHidden();
  await expect(page.locator('#rec-detail-title')).toHaveValue('AMUX Product Sync');
  await expect(page.locator('#rec-detail-meta')).toContainText('Call link');
  // Transcript: speakers, times, search.
  const lines = page.locator('#rec-lines .rec-line');
  await expect(lines).toHaveCount(3);
  await expect(lines.nth(1)).toContainText('Them');
  await expect(lines.nth(2).locator('.rec-line-at')).toHaveText('4:12');
  await page.locator('.rec-tsearch').fill('pelican');
  await expect(page.locator('#rec-lines .rec-line:visible')).toHaveCount(1);
  // Notes tab renders the notes file.
  await page.locator('#rec-detail .rec-tab[data-t="notes"]').click();
  await expect(page.locator('.rec-notes')).toContainText('Ship the hotkey');
  // Summary, then action items: a tick is saved on the recording.
  await expect(page.locator('#rec-side-body')).toContainText('planned worker integration');
  await page.locator('#rec-side .rec-tab[data-s="actions"]').click();
  await page.locator('.rec-action input').first().check();
  await expect.poll(() => {
    const f = fs.readdirSync(dir).find((n) => n.endsWith(`_${id}.json`))!;
    return JSON.parse(fs.readFileSync(path.join(dir, f), 'utf8')).notes.action_items[0].done;
  }).toBe(true);
  // Meeting info: the calendar event and its people.
  await page.locator('#rec-side .rec-tab[data-s="meeting"]').click();
  await expect(page.locator('#rec-side-body')).toContainText('sarah@example.com');
  // Workers: the suggestion is added with instructions and sent.
  await page.locator('#rec-side .rec-tab[data-s="workers"]').click();
  await page.locator('.rec-suggest button', { hasText: 'Add' }).click();
  await page.locator('.rec-worker input').fill('Catalogue it in your directory.');
  await page.locator('.rec-send').click();
  await expect.poll(async () => {
    const r = await page.request.get(`/api/recordings/${id}`, { headers: h });
    return ((await r.json()).send_to || []).map((w: any) => `${w.worker}:${w.state ? 'done' : 'owed'}:${w.instructions}`).join(',');
  }, { timeout: 20_000 }).toBe('rec-panes-w:done:Catalogue it in your directory.');
  const th = await (await page.request.get('/api/conversations/w:rec-panes-w/posts?limit=10', { headers: h })).json();
  const msg = (th.posts || []).find((p: any) => String(p.body).includes('A recording for you'));
  expect(msg, JSON.stringify(th).slice(0, 400)).toBeTruthy();
  expect(msg.body).toContain('Catalogue it in your directory.');
  expect(msg.body).toContain('sarah@example.com');
  expect(msg.body).toContain(path.join(dir, ''));
  // Rename sticks.
  await page.locator('#rec-detail-title').fill('Product sync, renamed');
  await page.locator('#rec-detail-title').press('Enter');
  await expect(row).toContainText('Product sync, renamed');
});

test('the strip, filters and hotkey chip, and nothing overflows', async ({ page }) => {
  const { id } = await setup(page);
  await expect(page.locator('#rec-hotkey-combo')).toHaveText(/Alt\+Shift\+R/);
  await page.locator('.rec-seg[data-f="voice"]').click();
  await expect(page.locator(`.recorder-item[data-rec-id="${id}"]`)).toHaveCount(0);
  await page.locator('.rec-seg[data-f="meeting"]').click();
  await expect(page.locator(`.recorder-item[data-rec-id="${id}"]`)).toHaveCount(1);
  await page.locator('#rec-search').fill('no such words anywhere');
  await expect(page.locator(`.recorder-item[data-rec-id="${id}"]`)).toHaveCount(0);
  await page.locator('#rec-search').fill('');
  await page.locator('.rec-seg[data-f="all"]').click();
  await expect(page.locator(`.recorder-item[data-rec-id="${id}"]`)).toHaveCount(1);
  const overflow = await page.locator('#record-view').evaluate((e) => e.scrollWidth - e.clientWidth);
  expect(overflow).toBeLessThanOrEqual(0);
  await page.screenshot({ path: test.info().outputPath('record-panes.png') });
});
