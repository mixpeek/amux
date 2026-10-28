// Channel settings in Messages (AA-52, Ethan 2026-10-28: "only owners/admins
// can but i want to be able to create new channels, add people, remove
// people"). An owner or admin gets "Manage channels" in the sidebar and a
// settings button in a channel's header; a member gets neither. The server
// refuses members on its own (tests/conversation_channels.rs); these stubs pin
// what the dialog sends and that it fits a phone.
import { test, expect, allowUnusedRoute } from './fixtures';

type Person = { email: string; name: string; member: boolean; via: string };

function stubServer(page: any, admin: boolean) {
  const calls: { method: string; path: string; body: any }[] = [];
  const channels: any[] = [
    { conv: 'g:general', name: 'general', title: '', private: false, archived: false, people: 2, workers: [] },
    { conv: 'g:wexus', name: 'wexus', title: '', private: false, archived: false, people: 2, workers: ['ed'] },
  ];
  const people: Record<string, Person[]> = {};
  const peopleFor = (name: string) => (people[name] ||= [
    { email: 'owner', name: 'owner', member: true, via: 'owner' },
    { email: 'mate@example.com', name: 'mate', member: name !== 'launch', via: name === 'launch' ? 'none' : 'team' },
  ]);
  page.route(/\/api\/conversations$/, (route: any) => route.fulfill({ json: {
    channels: channels.filter(c => !c.archived).map(c => ({ conv: c.conv, name: c.name, title: c.title, private: c.private })),
    people: [], workers: [], totals: {},
    me: { key: 'person:owner', scope_level: 'global', can_admin_channels: admin },
  } }));
  page.route(/\/api\/conversations\/g%3A[^/]+\/posts/, (route: any) => route.fulfill({ json: { posts: [], people: [], workers: [] } }));
  page.route(/\/api\/conversations\/g%3A[^/]+\/read/, (route: any) => route.fulfill({ json: { ok: true } }));
  const settings = /\/api\/conversations\/channels(\/.*)?$/;
  // A member's page must never call these, so the stub going unused is the pass.
  if (!admin) allowUnusedRoute(page, settings);
  page.route(settings, async (route: any) => {
    const req = route.request();
    const url = new URL(req.url());
    const rest = url.pathname.replace('/api/conversations/channels', '');
    const body = req.postData() ? JSON.parse(req.postData()) : null;
    calls.push({ method: req.method(), path: rest, body });
    if (!admin) return route.fulfill({ status: 403, json: { ok: false, code: 'channel_admin_only', error: 'not an admin' } });
    if (rest === '' && req.method() === 'GET') return route.fulfill({ json: { channels, measured: true, n_considered: channels.length } });
    if (rest === '' && req.method() === 'POST') {
      channels.push({ conv: 'g:' + body.name, name: body.name, title: '', private: !!body.private, archived: false, people: 1, workers: [] });
      return route.fulfill({ status: 201, json: { ok: true, conv: 'g:' + body.name, name: body.name } });
    }
    const m = rest.match(/^\/([^/]+)(\/members(?:\/(.+))?)?$/)!;
    const name = decodeURIComponent(m[1]);
    const ch = channels.find(c => c.name === name)!;
    if (!m[2] && req.method() === 'PATCH') { Object.assign(ch, body); return route.fulfill({ json: { ok: true, channel: ch } }); }
    const ppl = peopleFor(name);
    if (req.method() === 'POST') { const p = ppl.find(x => x.email === body.email)!; p.member = true; p.via = 'added'; }
    if (req.method() === 'DELETE') { const p = ppl.find(x => x.email === decodeURIComponent(m[3]))!; p.member = false; p.via = 'removed'; }
    return route.fulfill({ json: { channel: ch, people: ppl, measured: true, n_considered: ppl.length } });
  });
  return calls;
}

async function openMessages(page: any) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvManageChannels === 'function'
    && typeof (window as any).switchView === 'function');
  await page.evaluate(() => (window as any).switchView('messages'));
  await expect(page.locator('#cv-side')).toBeVisible();
}

async function atLeast44(loc: any) {
  const b = (await loc.boundingBox())!;
  expect(b.height, 'touch target height').toBeGreaterThanOrEqual(44);
  expect(b.width, 'touch target width').toBeGreaterThanOrEqual(44);
}

test('a member sees no channel settings', async ({ page }) => {
  const calls = stubServer(page, false);
  await openMessages(page);
  await expect(page.locator('#cv-channels [data-cv="g:wexus"]')).toBeVisible();
  await expect(page.locator('#cv-manage-channels')).toHaveCount(0);
  await page.locator('#cv-channels [data-cv="g:wexus"]').click();
  await expect(page.locator('#cv-title')).toHaveText('#wexus');
  await expect(page.locator('#cv-chan-gear')).toHaveCount(0);
  expect(calls, 'the page never called a settings route for a member').toEqual([]);
});

test('an admin creates a channel, adds and removes people, renames and archives it', async ({ page }) => {
  const calls = stubServer(page, true);
  await openMessages(page);
  const phone = page.viewportSize()!.width <= 600;

  // From a channel's header (one row on a phone too).
  await page.locator('#cv-channels [data-cv="g:wexus"]').click();
  const gear = page.locator('#cv-chan-gear');
  await expect(gear).toBeVisible();
  if (phone) {
    await atLeast44(gear);
    const head = (await page.locator('#cv-pane .cv-head').boundingBox())!;
    expect(head.height, 'the header stays one row').toBeLessThanOrEqual(72);
  }
  await gear.click();
  await expect(page.locator('#cv-ch-body .cv-ch-person[data-email="mate@example.com"]')).toBeVisible();
  await page.locator('#cv-ch-body .cv-ch-person[data-email="mate@example.com"] button').click();
  await expect(page.locator('#cv-ch-body .cv-ch-person[data-email="mate@example.com"] button')).toHaveText('Add');
  expect(calls.at(-2)).toEqual({ method: 'DELETE', path: '/wexus/members/mate%40example.com', body: null });
  await page.locator('#modal-btns .btn.primary').click();

  // From the sidebar: create a private channel and add someone.
  if (phone) await page.locator('#cv-pane .cv-back').click();
  const manage = page.locator('#cv-manage-channels');
  await expect(manage).toBeVisible();
  if (phone) await atLeast44(manage);
  await manage.click();
  await page.locator('#cv-ch-new').fill('Launch');
  await page.locator('#cv-ch-new-private').check();
  await page.locator('#cv-ch-create').click();
  expect(calls.find(c => c.method === 'POST' && c.path === '')!.body).toEqual({ name: 'launch', private: true });
  const mate = page.locator('#cv-ch-body .cv-ch-person[data-email="mate@example.com"]');
  await expect(mate.locator('button')).toHaveText('Add');
  if (phone) await atLeast44(mate.locator('button'));
  await mate.locator('button').click();
  await expect(mate.locator('button')).toHaveText('Remove');
  await expect(mate).toContainText('added');
  expect(calls.some(c => c.method === 'POST' && c.path === '/launch/members' && c.body.email === 'mate@example.com')).toBe(true);

  await page.locator('#cv-ch-title').fill('Launch room');
  await page.locator('#cv-ch-rename').click();
  await expect.poll(() => calls.some(c => c.method === 'PATCH' && c.body && c.body.title === 'Launch room')).toBe(true);

  // Archive asks twice, in place.
  const archive = page.locator('#cv-ch-archive');
  await archive.click();
  expect(calls.some(c => c.method === 'PATCH' && c.body && c.body.archived === true), 'one click archived').toBe(false);
  await expect(archive).toContainText('Sure?');
  await archive.click();
  await expect.poll(() => calls.some(c => c.method === 'PATCH' && c.body && c.body.archived === true)).toBe(true);
  await expect(page.locator('#cv-ch-archive')).toHaveText('Restore');

  // The modal fits a phone: nothing wider than the screen.
  if (phone) {
    const vw = page.viewportSize()!.width;
    const box = (await page.locator('#cv-ch-body').boundingBox())!;
    expect(box.x + box.width).toBeLessThanOrEqual(vw);
  }
  await page.locator('#modal-btns .btn.primary').click();
  await expect(page.locator('#cv-channels [data-cv="g:launch"]'), 'an archived channel leaves the sidebar').toHaveCount(0);
});
