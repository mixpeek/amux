// Ethan, 2026-10-18: "On the plus button make it so that there is an easy way
// to have users describe what they want to build ... a single input box ...
// below there are settings or constraints ... an accordion ... not expanded by
// default ... it spins up the worker ... flags to the user when it's finished
// and it should keep going until it's finished, maybe via a /goal."
// Ethan, 2026-10-29: it named a CRM worker `need-worker-behave-crm`, started it
// in /home/amux, and "it shouldn't create a goal". The server now plans the
// name, directory and groups from the text and attachments; no /goal.
import { test, expect } from './fixtures';

test('Describe what you want starts a planned worker from one box, with saved presets and no /goal', async ({ page }) => {
  test.setTimeout(90_000);
  const creates: any[] = [];
  const prefs: any[] = [];
  let reply: 'refuse' | 'ok' | 'down' = 'refuse';
  const plans: any[] = [];
  let planReply: any = { name: 'dark-mode-toggle', dir: '/srv/app', groups: ['ui'], desc: 'Ships a dark mode toggle in settings.', attachments: [] };
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.removeItem('amux_outcome_draft');
    localStorage.removeItem('amux_outcome_defaults');
    localStorage.removeItem('amux_outcome_presets');
  });
  // No real worker: the create is answered here, and nothing else is written.
  await page.route('**/api/sessions', async route => {
    if (route.request().method() !== 'POST') return route.fallback();
    creates.push(route.request().postDataJSON());
    if (reply === 'refuse') { reply = 'ok'; return route.fulfill({ status: 400, contentType: 'application/json', body: '{"error":"bad dir"}' }); }
    // A 5xx is queued by the outbox for replay, so it answers the page with a local 202.
    if (reply === 'down') return route.fulfill({ status: 503, contentType: 'application/json', body: '{"error":"down"}' });
    return route.fulfill({ status: 201, contentType: 'application/json', body: '{"ok":true}' });
  });
  await page.route('**/api/outcome/plan', async route => {
    plans.push(route.request().postDataJSON());
    if (!planReply) return route.fulfill({ status: 500, contentType: 'application/json', body: '{"error":"no helper"}' });
    return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ measured: true, n_considered: 3, ...planReply }) });
  });
  // The server's prefs, for the keys this dialog keeps: presets and the
  // shared history (outcome_history), so what one open writes the next reads.
  const store: Record<string, string> = {};
  await page.route('**/api/prefs**', async route => {
    if (route.request().method() === 'POST') {
      const b = route.request().postDataJSON();
      if (!/^outcome_/.test(b.key)) return route.fallback();
      store[b.key] = b.value;
      if (b.key === 'outcome_presets') prefs.push(b);
      return route.fulfill({ status: 200, body: '{"ok":true}' });
    }
    const m = /key=(outcome_[a-z]+)/.exec(route.request().url());
    if (m) return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ value: store[m[1]] ?? null }) });
    return route.fallback();
  });
  const hist = () => JSON.parse(store.outcome_history || '{}');
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openOutcome === 'function');

  const open = async () => {
    await page.locator('#add-btn').click();
    await page.locator('#add-menu .card-menu-item', { hasText: 'Describe what you want' }).click();
    await expect(page.locator('#outcome-text')).toBeVisible();
  };
  await open();
  await expect(page.locator('#outcome-text')).toBeFocused();
  // The settings are there but closed.
  await expect(page.locator('.outcome-settings')).not.toHaveAttribute('open', '');
  await expect(page.locator('#outcome-constraints')).toBeHidden();
  await page.locator('#outcome-text').fill('Add a dark mode toggle to settings and ship it');
  await page.locator('.outcome-settings > summary').click();
  await expect(page.locator('#outcome-constraints')).toBeVisible();
  await expect(page.locator('#outcome-constraints')).toHaveValue(/Do not spend money/);
  await expect(page.locator('#outcome-goal')).toHaveCount(0);
  await expect(page.locator('#outcome-dir')).toHaveValue('');
  // Presets: None is picked, nothing is remembered unless asked, and the
  // old "Remember these as my defaults" box is gone.
  await expect(page.locator('#outcome-preset')).toHaveValue('');
  await expect(page.locator('#outcome-remember')).toHaveCount(0);
  await expect(page.locator('#outcome-save-preset')).not.toBeChecked();
  await expect(page.locator('#outcome-preset-name')).toBeHidden();
  await page.locator('#outcome-group').fill('ui-work');
  await page.locator('#outcome-dir').fill('/tmp/outcome-e2e');
  await page.locator('#outcome-save-preset').check();
  await expect(page.locator('#outcome-preset-name')).toBeVisible();
  await page.locator('#outcome-preset-name').fill('UI work');

  // Layout: fits the screen, touch targets on phones.
  const fit = await page.evaluate(() => {
    const r = document.querySelector('#modal-backdrop .outcome-form')!.getBoundingClientRect();
    const s = document.querySelector('.outcome-settings > summary')!.getBoundingClientRect();
    return { right: r.right, vw: window.innerWidth, summaryH: s.height };
  });
  expect(fit.right).toBeLessThanOrEqual(fit.vw);
  if (fit.vw <= 600) expect(fit.summaryH).toBeGreaterThanOrEqual(44);
  await page.screenshot({ path: test.info().outputPath('describe-outcome.png') });

  // A refused start keeps the description.
  await page.locator('#modal-btns .btn.primary', { hasText: 'Start' }).click();
  await expect.poll(() => creates.length).toBe(1);
  await open();
  await expect(page.locator('#outcome-text')).toHaveValue('Add a dark mode toggle to settings and ship it');
  // The preset was saved, and the dialog opens on it: the preset last
  // started with (owner, 2026-11-21), named on the closed summary too.
  expect(prefs.length).toBe(1);
  expect(JSON.parse(prefs[0].value)).toEqual([expect.objectContaining({ name: 'UI work', group: 'ui-work', dir: '/tmp/outcome-e2e' })]);
  expect(hist().last_preset).toBe('UI work');
  await expect(page.locator('.outcome-settings > summary')).toContainText('UI work');
  await page.locator('.outcome-settings > summary').click();
  await expect(page.locator('#outcome-preset')).toHaveValue('UI work');
  await expect(page.locator('#outcome-groups .oc-pill > span')).toHaveText(['ui-work']);
  await expect(page.locator('#outcome-dir')).toHaveValue('/tmp/outcome-e2e');
  await expect(page.locator('#outcome-preset-delete')).toBeVisible();
  // None is the built-in defaults; picking the preset again fills it back.
  await page.locator('#outcome-preset').selectOption('');
  await expect(page.locator('#outcome-groups .oc-pill > span')).toHaveCount(0);
  await expect(page.locator('#outcome-dir')).toHaveValue('');
  await page.locator('#outcome-preset').selectOption('UI work');
  await expect(page.locator('#outcome-groups .oc-pill > span')).toHaveText(['ui-work']);
  await page.locator('#outcome-text').press(process.platform === 'darwin' ? 'Meta+Enter' : 'Control+Enter');
  await expect.poll(() => creates.length).toBe(2);

  const body = creates[1];
  // The plan names it; a directory and group typed in the dialog win over the plan's.
  expect(plans[plans.length - 1].text).toBe('Add a dark mode toggle to settings and ship it');
  expect(body.name).toBe('dark-mode-toggle');
  expect(body.desc).toBe('Ships a dark mode toggle in settings.');
  expect(body.dir).toBe('/tmp/outcome-e2e');
  expect(body.tags).toEqual(['ui-work']);
  // Not isolated unless asked (885c7418f, docs/design/intent-first.md):
  // the dialog sends no isolated flag for a default start.
  expect(body.isolated).toBeUndefined();
  expect(body.prompt.startsWith('Add a dark mode toggle to settings and ship it Constraints: Do not spend money')).toBe(true);
  expect(body.prompt).not.toContain('/goal');
  expect(body.prompt).not.toContain('\n');
  // Starting from a preset without ticking "Save as a preset" writes nothing.
  expect(prefs.length).toBe(1);
  // Open in worker -> New worker starts with the preset last started with.
  expect(await page.evaluate(async () => (await (window as any)._outcomeDefaults()).dir)).toBe('/tmp/outcome-e2e');
  // A started worker clears the kept draft.
  await expect.poll(() => page.evaluate(() => localStorage.getItem('amux_outcome_draft'))).toBeNull();

  // Finished: a /goal turning met fires a notification.
  const fired = await page.evaluate(() => {
    const w = window as any; const seen: string[] = [];
    const orig = w._fireSessionNotif;
    w._fireSessionNotif = (_n: string, title: string) => seen.push(title);
    try {
      const base = { name: 'add-dark-mode-toggle', status: 'idle', running: true };
      w._checkSessionTransitions([{ ...base, goal: { state: 'unmet', condition: 'ship it' } }]);
      w._checkSessionTransitions([{ ...base, goal: { state: 'met', condition: 'ship it' } }]);
      w._checkSessionTransitions([{ ...base, goal: { state: 'met', condition: 'ship it' } }]);
    } finally { w._fireSessionNotif = orig; }
    return seen;
  });
  expect(fired).toEqual(['add-dark-mode-toggle finished its goal']);

  // An attached recording reaches the plan by its uploaded path, the plan's
  // directory and groups are used when the dialog leaves them empty, and
  // the worker is told to read the recording's transcript.
  reply = 'ok';
  planReply = { name: 'gtm-crm', dir: '/home/u/amux-GTM/gtm', groups: ['gtm'], desc: 'Tracks outreach toward the pilot goal.',
    attachments: [{ path: '/up/memo.m4a', kind: 'audio', transcript: '/up/memo.m4a.transcript.txt', words: 1544 }] };
  await open();
  await page.locator('#outcome-text').fill('I need a worker to behave like a CRM that tracks my outreach and progress towards goal');
  // A preset saved with no name is named for its folder and group; this one is only "isolated".
  await page.locator('.outcome-settings > summary').click();
  await expect(page.locator('#outcome-preset')).toHaveValue('UI work');
  await page.locator('#outcome-preset').selectOption('');
  await page.locator('#outcome-isolated').check();
  await page.locator('#outcome-save-preset').check();
  await page.locator('#outcome-file').setInputFiles({ name: 'memo.m4a', mimeType: 'audio/mp4', buffer: Buffer.from('not really audio') });
  await expect(page.locator('#outcome-file-list')).toContainText('memo.m4a');
  await expect(page.locator('#outcome-file-list')).not.toContainText('uploading');
  await page.locator('#modal-btns .btn.primary', { hasText: 'Start' }).click();
  await expect.poll(() => creates.length).toBe(3);
  const planned = plans[plans.length - 1];
  expect(planned.files).toHaveLength(1);
  expect(planned.files[0]).toMatch(/memo\.m4a$/);
  const gtm = creates[2];
  expect(gtm.name).toBe('gtm-crm');
  expect(gtm.isolated).toBe(true);   // ticked, so asked for
  expect(prefs.length).toBe(2);
  expect(JSON.parse(prefs[1].value).map((p: any) => p.name)).toEqual(['UI work', 'Preset']);
  expect(JSON.parse(prefs[1].value)[1].isolated).toBe(true);
  expect(gtm.dir).toBe('/home/u/amux-GTM/gtm');
  expect(gtm.tags).toEqual(['gtm']);
  expect(gtm.prompt).toContain('/up/memo.m4a.transcript.txt');
  expect(gtm.prompt).not.toContain('/goal');

  // No plan (helper down): the computed name drops the request's scaffolding.
  planReply = null;
  await open();
  await page.locator('#outcome-text').fill('I need a worker to behave like a CRM that tracks my outreach');
  // It opens on the preset just saved; None is the built-in defaults.
  await page.locator('.outcome-settings > summary').click();
  await expect(page.locator('#outcome-preset')).toHaveValue('Preset');
  await expect(page.locator('#outcome-isolated')).toBeChecked();
  await page.locator('#outcome-preset').selectOption('');
  await expect(page.locator('#outcome-isolated')).not.toBeChecked();
  // Delete asks on the same button, then removes the preset.
  await page.locator('#outcome-preset').selectOption('Preset');
  await page.locator('#outcome-preset-delete').click();
  await expect(page.locator('#outcome-preset-delete')).toHaveText('Delete "Preset"?');
  expect(prefs.length).toBe(2);
  await page.locator('#outcome-preset-delete').click();
  await expect.poll(() => prefs.length).toBe(3);
  expect(JSON.parse(prefs[2].value).map((p: any) => p.name)).toEqual(['UI work']);
  await expect(page.locator('#outcome-preset')).toHaveValue('');
  await expect(page.locator('#outcome-preset option')).toHaveText(['None', 'UI work']);
  await page.locator('#modal-btns .btn.primary', { hasText: 'Start' }).click();
  await expect.poll(() => creates.length).toBe(4);
  expect(creates[3].name).toBe('crm-tracks-outreach');
  expect(creates[3].isolated).toBeUndefined();
  // ...and the built-in defaults once a worker was started on None.
  expect(await page.evaluate(async () => (await (window as any)._outcomeDefaults()).dir)).toBe('');

  // Server down: the create is queued for replay, so the draft goes rather
  // than inviting a second press that would start a duplicate worker.
  reply = 'down';
  const before = creates.length;
  await open();
  await page.locator('#outcome-text').fill('Write a weekly summary of open cards');
  await page.locator('#modal-btns .btn.primary', { hasText: 'Start' }).click();
  await expect.poll(() => creates.length).toBeGreaterThan(before);
  await expect.poll(() => page.evaluate(() => localStorage.getItem('amux_outcome_draft'))).toBeNull();
  await expect(page.locator('#toast')).toContainText('starts on its own');
  await expect(page.locator('#toast')).toBeVisible();
});

test('Defaults remembered the old way come back as a preset named "My defaults"', async ({ page }) => {
  const posts: any[] = [];
  let history: string | null = null;
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.removeItem('amux_outcome_history');
    localStorage.removeItem('amux_outcome_presets');
    localStorage.removeItem('amux_outcome_defaults');
  });
  await page.route('**/api/prefs**', async route => {
    const url = route.request().url();
    if (route.request().method() === 'POST') {
      const b = route.request().postDataJSON();
      if (b.key === 'outcome_presets') { posts.push(b); return route.fulfill({ status: 200, body: '{"ok":true}' }); }
      if (b.key === 'outcome_history') { history = b.value; return route.fulfill({ status: 200, body: '{"ok":true}' }); }
      return route.fallback();
    }
    if (url.includes('key=outcome_presets')) return route.fulfill({ status: 200, contentType: 'application/json', body: '{"value":null}' });
    if (url.includes('key=outcome_history')) return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ value: history }) });
    if (url.includes('key=outcome_defaults')) return route.fulfill({ status: 200, contentType: 'application/json',
      body: JSON.stringify({ value: JSON.stringify({ group: 'gtm', dir: '/srv/gtm', provider: 'claude', model: '', isolated: false, constraints: 'Be brief.' }) }) });
    return route.fallback();
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openOutcome === 'function');
  await page.locator('#add-btn').click();
  await page.locator('#add-menu .card-menu-item', { hasText: 'Describe what you want' }).click();
  await page.locator('.outcome-settings > summary').click();
  await expect(page.locator('#outcome-preset option')).toHaveText(['None', 'My defaults']);
  // They were what every start used, so the dialog opens on them, and so does
  // Open in worker -> New worker.
  await expect(page.locator('#outcome-preset')).toHaveValue('My defaults');
  await expect.poll(() => posts.length).toBe(1);
  expect(JSON.parse(history!).last_preset).toBe('My defaults');
  expect(await page.evaluate(async () => (await (window as any)._outcomeDefaults()).dir)).toBe('/srv/gtm');
  await expect(page.locator('#outcome-dir')).toHaveValue('/srv/gtm');
  await expect(page.locator('#outcome-constraints')).toHaveValue('Be brief.');
  // Layout: the preset row fits the screen.
  const fit = await page.evaluate(() => {
    const r = document.querySelector('.outcome-preset-row')!.getBoundingClientRect();
    return { right: r.right, vw: window.innerWidth };
  });
  expect(fit.right).toBeLessThanOrEqual(fit.vw);
  await page.screenshot({ path: test.info().outputPath('outcome-presets.png') });
});

// Owner, 2026-11-25: "control-V paste an image from the clipboard in the
// 'Describe what you want' dialog to be added as an attachment". A pasted
// screenshot uploads like a picked file, under its own name (every clipboard
// image is "image.png"), and the worker is told to read it. A text paste is
// left to the box.
test('Pasting an image into Describe what you want attaches it to the new worker', async ({ page }) => {
  const creates: any[] = [];
  const plans: any[] = [];
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.removeItem('amux_outcome_draft');
  });
  await page.route('**/api/sessions', async route => {
    if (route.request().method() !== 'POST') return route.fallback();
    creates.push(route.request().postDataJSON());
    return route.fulfill({ status: 201, contentType: 'application/json', body: '{"ok":true}' });
  });
  await page.route('**/api/outcome/plan', async route => {
    plans.push(route.request().postDataJSON());
    return route.fulfill({ status: 200, contentType: 'application/json',
      body: JSON.stringify({ measured: true, n_considered: 1, name: 'fix-the-button', dir: '/srv/app', groups: [], desc: 'Fixes the button.', attachments: [] }) });
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openOutcome === 'function');
  await page.locator('#add-btn').click();
  await page.locator('#add-menu .card-menu-item', { hasText: 'Describe what you want' }).click();
  await expect(page.locator('#outcome-text')).toBeVisible();
  await page.locator('#outcome-text').fill('Fix the button in this screenshot');

  // The paste a browser fires for Ctrl/Cmd+V with an image on the clipboard.
  const paste = (withImage: boolean) => page.locator('#outcome-text').evaluate((ta, withImage) => {
    const dt = new DataTransfer();
    if (withImage) {
      const png = Uint8Array.from(atob('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=='), c => c.charCodeAt(0));
      dt.items.add(new File([png], 'image.png', { type: 'image/png' }));
    } else dt.setData('text/plain', ' (urgent)');
    const ev = new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true });
    ta.dispatchEvent(ev);
    return ev.defaultPrevented;
  }, withImage);
  // Text is not taken over: the box handles it as usual.
  expect(await paste(false)).toBe(false);
  await expect(page.locator('#outcome-file-list .outcome-file')).toHaveCount(0);
  expect(await paste(true)).toBe(true);
  expect(await paste(true)).toBe(true);
  const chips = page.locator('#outcome-file-list .outcome-file');
  await expect(chips).toHaveCount(2);
  await expect(chips.first()).toContainText(/pasted-.*\.png/);
  // The pasted image shows itself.
  await expect(chips.first().locator('.chip-thumb img')).toHaveAttribute('src', /^blob:/);
  await expect(page.locator('#outcome-file-list')).not.toContainText('uploading');
  await expect(page.locator('#outcome-file-list')).not.toContainText('did not upload');
  const names = await chips.allInnerTexts();
  expect(names[0]).not.toBe(names[1]);
  await expect(page.locator('#outcome-text')).toHaveValue('Fix the button in this screenshot');
  // Removing one leaves the other.
  await chips.nth(1).locator('.chip-remove').click();
  await expect(chips).toHaveCount(1);
  await page.screenshot({ path: test.info().outputPath('outcome-paste.png') });

  await page.locator('#modal-btns .btn.primary', { hasText: 'Start' }).click();
  await expect.poll(() => creates.length).toBe(1);
  expect(plans[0].files).toHaveLength(1);
  expect(plans[0].files[0]).toMatch(/pasted-.*\.png$/);
  expect(creates[0].prompt).toContain(plans[0].files[0]);
});

// Owner, 2026-11-26: "add a little image preview to the pasted content ... a
// play symbol to symbolize it's a video file and then just take the first
// frame". Each kind of attachment gets its own thumbnail, and the thumbnail
// opens the shared preview on top of the dialog without closing it.
test('Attachments in Describe what you want preview by kind: image, video first frame, audio, PDF, text, other', async ({ page }) => {
  test.setTimeout(60_000);
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.removeItem('amux_outcome_draft');
  });
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openOutcome === 'function');
  await page.locator('#add-btn').click();
  await page.locator('#add-menu .card-menu-item', { hasText: 'Describe what you want' }).click();
  await expect(page.locator('#outcome-text')).toBeVisible();
  await page.locator('#outcome-text').fill('Make the onboarding match this recording and the spec');

  // A screenshot on the clipboard, pasted.
  await page.locator('#outcome-text').evaluate(ta => {
    const c = document.createElement('canvas'); c.width = 320; c.height = 200;
    const g = c.getContext('2d')!;
    g.fillStyle = '#f4f6fb'; g.fillRect(0, 0, 320, 200);
    g.fillStyle = '#3b6cf6'; g.fillRect(0, 0, 320, 36);
    g.fillStyle = '#e5484d'; g.fillRect(24, 60, 120, 40);
    g.fillStyle = '#30a46c'; g.fillRect(170, 60, 120, 110);
    return new Promise<void>(res => c.toBlob(b => {
      const dt = new DataTransfer();
      dt.items.add(new File([b!], 'image.png', { type: 'image/png' }));
      ta.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
      res();
    }, 'image/png'));
  });
  // A short screen recording, picked: drawn on a canvas and recorded by the
  // browser, so its first frame is a known colour.
  const recorded = await page.evaluate(async () => {
    const c = document.createElement('canvas'); c.width = 320; c.height = 180;
    const g = c.getContext('2d')!;
    const paint = (t: number) => { g.fillStyle = '#7c3aed'; g.fillRect(0, 0, 320, 180); g.fillStyle = '#fbbf24'; g.fillRect(20 + t * 4, 50, 80, 80); };
    paint(0);
    const types = ['video/webm;codecs=vp8', 'video/webm', 'video/mp4'];
    const mime = types.find(t => (window as any).MediaRecorder?.isTypeSupported?.(t));
    if (!mime) return '';
    const rec = new MediaRecorder((c as any).captureStream(30), { mimeType: mime });
    const parts: Blob[] = [];
    rec.ondataavailable = e => parts.push(e.data);
    const stopped = new Promise(r => { rec.onstop = r; });
    rec.start();
    for (let i = 0; i < 20; i++) { paint(i); await new Promise(r => setTimeout(r, 40)); }
    rec.stop(); await stopped;
    const type = mime.split(';')[0];
    const dt = new DataTransfer();
    dt.items.add(new File(parts, 'walkthrough.' + (type === 'video/mp4' ? 'mp4' : 'webm'), { type }));
    const input = document.getElementById('outcome-file') as HTMLInputElement;
    input.files = dt.files;
    input.dispatchEvent(new Event('change', { bubbles: true }));
    return type;
  });
  // The rest, picked from disk.
  await page.locator('#outcome-file').setInputFiles([
    { name: 'voice-memo.m4a', mimeType: 'audio/mp4', buffer: Buffer.from('not really audio') },
    { name: 'onboarding-spec.pdf', mimeType: 'application/pdf', buffer: Buffer.from('%PDF-1.4\n%%EOF\n') },
    { name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('Step 1: sign in\nStep 2: pick a plan\n') },
    { name: 'design-export.zip', mimeType: 'application/zip', buffer: Buffer.from('PK') },
  ]);
  const chips = page.locator('#outcome-file-list .outcome-file');
  await expect(chips).toHaveCount(recorded ? 6 : 5);
  await expect(page.locator('#outcome-file-list')).not.toContainText('uploading');
  await expect(page.locator('#outcome-file-list')).not.toContainText('did not upload');

  const chip = (name: RegExp | string) => chips.filter({ hasText: name });
  await expect(chip(/pasted-.*\.png/).locator('.chip-thumb img')).toHaveAttribute('src', /^blob:/);
  await expect(chip('voice-memo.m4a').locator('.chip-icon')).toHaveText('\u{1F3B5}');
  await expect(chip('onboarding-spec.pdf').locator('.chip-icon')).toHaveText('\u{1F4C4}');
  await expect(chip('notes.txt').locator('.chip-icon')).toHaveText('\u{1F4DD}');
  await expect(chip('design-export.zip').locator('.chip-icon')).toHaveText('\u{1F4CE}');
  // Chromium records and decodes webm here, so the frame must be captured;
  // every engine shows the play mark either way.
  const chromium = test.info().project.name !== 'ios-safari';
  if (chromium) expect(recorded).toBe('video/webm');
  if (recorded) {
    const video = chip(/walkthrough\./);
    await expect(video.locator('.oc-video-thumb .oc-play')).toBeVisible();
    if (chromium) {
      await expect(video.locator('.oc-video-thumb img')).toHaveAttribute('src', /^data:image\/jpeg/);
      // The frame is the recording's purple, not a blank.
      const px = await video.locator('.oc-video-thumb img').evaluate(async (img: HTMLImageElement) => {
        await img.decode();
        const c = document.createElement('canvas'); c.width = img.naturalWidth; c.height = img.naturalHeight;
        const g = c.getContext('2d')!; g.drawImage(img, 0, 0);
        return Array.from(g.getImageData(2, 2, 1, 1).data);
      });
      expect(px[2]).toBeGreaterThan(150);   // blue channel of #7c3aed
      expect(px[1]).toBeLessThan(110);
    }
  }
  await page.evaluate(() => (window as any)._applyTheme(true));
  await page.screenshot({ path: test.info().outputPath('outcome-attachments-light.png') });
  await page.evaluate(() => (window as any)._applyTheme(false));
  await page.screenshot({ path: test.info().outputPath('outcome-attachments-dark.png') });

  // The thumbnail opens the preview over the dialog; closing it keeps the dialog.
  await chip(/pasted-.*\.png/).locator('.chip-thumb').click();
  await expect(page.locator('#attach-preview .attach-preview-img')).toBeVisible();
  // On top of the dialog, not behind it: the preview's own close is the
  // element at its centre.
  expect(await page.locator('#attach-preview .modal-close').evaluate(b => {
    const r = b.getBoundingClientRect();
    return document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2) === b;
  })).toBe(true);
  await page.screenshot({ path: test.info().outputPath('outcome-attachments-preview.png') });
  // Its file-viewer button would open beneath this dialog, so it is not offered.
  await expect(page.locator('#attach-preview [data-open-file]')).toHaveCount(0);
  await expect(page.locator('#attach-preview')).toContainText('uploaded');
  await page.keyboard.press('Escape');
  await expect(page.locator('#attach-preview')).toHaveCount(0);
  await expect(page.locator('#outcome-text')).toHaveValue('Make the onboarding match this recording and the spec');
  if (recorded) {
    await chip(/walkthrough\./).locator('.chip-thumb').click();
    await expect(page.locator('#attach-preview video.attach-preview-media')).toBeVisible();
    await page.locator('#attach-preview .modal-close').click();
  }
  await expect(page.locator('#outcome-text')).toBeVisible();
});
