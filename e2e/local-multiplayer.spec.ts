// Local/Tailscale multiplayer: an owner creates an email-bound invite through
// the real Team UI, a second isolated browser accepts it, then both browsers
// converge on one board and the member's mutation is visible in Amux logs.
import { test, expect } from './fixtures';
import type { Page } from '@playwright/test';

async function settle(page: Page): Promise<void> {
  await expect(page.locator('#conn-status').first()).toBeAttached();
  await page.waitForFunction(() => typeof (window as any).apiCall === 'function');
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => {
    await bar.locator('button').last().click();
  });
  const walkthrough = page.locator('#wt-overlay.open');
  await walkthrough.waitFor({ state: 'visible', timeout: 8_000 }).catch(() => {});
  if (await walkthrough.isVisible()) {
    await page.locator('#wt-tooltip .wt-skip').click();
    await expect(walkthrough).toBeHidden();
  }
}

async function openTeam(page: Page): Promise<void> {
  const menu = page.locator('#settings-menu');
  if (!(await menu.evaluate((el) => el.classList.contains('open')))) {
    await page.click('#settings-btn');
  }
  await expect(menu).toHaveClass(/open/);
  await page.addStyleTag({ content: '#settings-menu .settings-tab-panel{display:block !important}' });
  await expect(page.locator('#settings-team-section')).toBeVisible();
}

test('local invitee joins, shares work, uses worker APIs, appears in logs, and can be revoked', async ({
  page: owner,
  browser,
  request,
}) => {
  test.setTimeout(120_000);
  await owner.goto('/');
  await settle(owner);
  const ownerToken = await owner.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const ownerUiToken = await owner.evaluate(() => (window as any)._AMUX_UI_TOKEN as string);
  expect(ownerToken).toBeTruthy();
  expect(ownerUiToken).toBeTruthy();
  const ownerHeaders = {
    Authorization: `Bearer ${ownerToken}`,
    'Content-Type': 'application/json',
  };

  await openTeam(owner);
  await owner.locator('#settings-team-section button', { hasText: '+ Invite' }).click();
  await expect(owner.locator('#team-invite-email')).toBeVisible();
  await owner.locator('#team-invite-email').fill('guest@example.com');
  const [inviteResponse] = await Promise.all([
    owner.waitForResponse(
      (response) =>
        response.url().endsWith('/api/org/invites') &&
        response.request().method() === 'POST',
    ),
    owner.locator('#team-scope-submit').click(),
  ]);
  expect(inviteResponse.status()).toBe(201);
  const inviteUrl = await owner.locator('#invite-link-input').inputValue();
  expect(inviteUrl).toContain('/invite/');

  // A separate BrowserContext is a separate person: no localStorage, cookies,
  // bearer bootstrap, or service worker state is shared with the owner.
  const guestContext = await browser.newContext({
    ignoreHTTPSErrors: true,
    serviceWorkers: 'block',
  });
  const guest = await guestContext.newPage();
  const createdWorkers: string[] = [];
  const createdCards: string[] = [];
  let memberWorker: string | undefined;
  try {
    await guest.goto(inviteUrl);
    await expect(guest.getByRole('heading', { name: /^Join / })).toBeVisible();
    await expect(guest.locator('#email')).toHaveValue('guest@example.com');
    await guest.locator('#name').fill('Guest User');
    await Promise.all([
      guest.waitForURL((url) => url.pathname === '/'),
      guest.getByRole('button', { name: 'Join workspace' }).click(),
    ]);
    await settle(guest);

    const guestIdentity = await guest.evaluate(async () => {
      const response = await fetch('/api/identity');
      return { status: response.status, body: await response.json() };
    });
    expect(guestIdentity.status).toBe(200);
    expect(guestIdentity.body).toMatchObject({
      email: 'guest@example.com',
      is_local_member: true,
      is_cloud: false,
      access_scope: { level: 'global', name: '' },
    });
    expect(
      await guest.evaluate(() => (window as any)._AMUX_AUTH_TOKEN),
      'member shell must use its cookie, never the owner bearer',
    ).toBe('');

    // A member sees their own grant, but membership administration remains an
    // owner capability even for a global member. loadTeamSection() (app.js)
    // renders a member's own access level into #settings-teams-list, not
    // #settings-members-list — that element holds only the "membership is
    // managed by the server owner" notice for a member's own view.
    await openTeam(guest);
    await expect(guest.locator('#settings-teams-list')).toContainText('Global workspace access');
    await expect(guest.locator('#settings-team-invite')).toBeHidden();
    await owner.locator('#invite-done-button').click();
    await openTeam(owner);
    await owner.evaluate(() => (window as any).loadTeamSection());
    await expect(owner.locator('#settings-members-list')).toContainText('Guest User');

    // Multiplayer includes the fleet, not just the board. Exercise the real
    // worker registry and per-worker read surface with only the invitee's
    // HttpOnly cookie. The live Tailnet acceptance run starts and sends to this
    // same shape against a disposable Codex worker; this hermetic case pins the
    // member-auth contract without auto-waking a model in CI.
    memberWorker = `e2e-member-worker-${Date.now()}`;
    const groupPeer = `e2e-group-peer-${Date.now()}`;
    const outsider = `e2e-outsider-${Date.now()}`;
    const workerAccess = await guest.evaluate(async (workerName) => {
      const create = await fetch('/api/sessions', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          // Ordinary attribution headers are caller-controlled. The server
          // must still persist the authenticated member as the author.
          'X-Amux-Worker': 'spoofed-worker-author',
        },
        body: JSON.stringify({
          name: workerName,
          dir: '/tmp',
          provider: 'codex',
          creator: 'spoofed-owner',
          tags: ['e2e-multiplayer'],
        }),
      });
      const createBody = await create.json();
      const fleet = await fetch('/api/sessions');
      const rows = await fleet.json();
      const fleetRow = rows.find((row: any) => row.name === workerName);
      const info = await fetch(`/api/sessions/${encodeURIComponent(workerName)}/info`);
      const infoBody = await info.json();
      const stoppedSend = await fetch(`/api/sessions/${encodeURIComponent(workerName)}/send`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'X-Amux-Session': 'another-spoofed-worker',
        },
        body: JSON.stringify({ text: 'member worker-use probe', record_history: true }),
      });
      return {
        createStatus: create.status,
        createBody,
        fleetStatus: fleet.status,
        listed: Boolean(fleetRow),
        fleetCreator: fleetRow?.creator,
        infoStatus: info.status,
        infoBody,
        sendStatus: stoppedSend.status,
        sendBody: await stoppedSend.json(),
      };
    }, memberWorker);
    expect(workerAccess).toMatchObject({
      createStatus: 201,
      createBody: { creator: 'member:guest@example.com' },
      fleetStatus: 200,
      listed: true,
      fleetCreator: 'member:guest@example.com',
      infoStatus: 200,
      infoBody: { name: memberWorker },
      sendStatus: 200,
      sendBody: { ok: true, authored_by: 'member:guest@example.com' },
    });
    createdWorkers.push(memberWorker);
    for (const [name, tags] of [
      [groupPeer, ['e2e-multiplayer']],
      [outsider, ['e2e-outsider']],
    ] as Array<[string, string[]]>) {
      const status = await guest.evaluate(async ({ name, tags }) => (
        await fetch('/api/sessions', {
          method: 'POST', headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ name, dir: '/tmp', provider: 'codex', tags }),
        })
      ).status, { name, tags });
      expect(status).toBe(201);
      createdWorkers.push(name);
    }

    // The owner can grant a non-global scope at invite creation, not only
    // rescope an existing member later. Exercise the actual Team dialog and
    // verify the persisted API response before revoking this unused link.
    await owner.evaluate(() => (window as any).fetchSessions());
    await openTeam(owner);
    await owner.locator('#settings-team-invite').click();
    await owner.locator('#team-invite-email').fill('group-invite@example.com');
    await owner.locator('#team-scope-level').selectOption('group');
    await owner.locator('#team-scope-name').selectOption('e2e-multiplayer');
    const [scopedInviteResponse] = await Promise.all([
      owner.waitForResponse(
        (response) =>
          response.url().endsWith('/api/org/invites') &&
          response.request().method() === 'POST',
      ),
      owner.locator('#team-scope-submit').click(),
    ]);
    expect(scopedInviteResponse.status()).toBe(201);
    const scopedInvite = await scopedInviteResponse.json();
    expect(scopedInvite).toMatchObject({
      scope_level: 'group',
      scope_name: 'e2e-multiplayer',
    });
    await request.delete(`/api/org/invites/${encodeURIComponent(scopedInvite.token)}`, {
      headers: ownerHeaders,
    });
    await owner.locator('#invite-done-button').click();

    // The invitee performs real work with cookie auth. The owner's browser
    // observes the same card after the normal board refresh.
    const title = `local multiplayer ${Date.now()}`;
    const created = await guest.evaluate(async (cardTitle) => {
      const response = await fetch('/api/board', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'X-Amux-Worker': 'spoofed-card-author',
        },
        body: JSON.stringify({
          title: cardTitle,
          type: 'chore',
          status: 'todo',
          creator: 'spoofed-owner',
        }),
      });
      return { status: response.status, body: await response.json() };
    }, title);
    expect(created.status).toBe(201);
    expect(created.body.creator).toBe('member:guest@example.com');
    const cardId = created.body.id as string;
    createdCards.push(cardId);
    const authoredEdit = await guest.evaluate(async (id) => {
      const response = await fetch(`/api/board/${encodeURIComponent(id)}`, {
        method: 'PATCH',
        headers: {
          'Content-Type': 'application/json',
          'X-Amux-Session': 'spoofed-edit-author',
        },
        body: JSON.stringify({ desc_append: 'member-authored note' }),
      });
      return { status: response.status, body: await response.json() };
    }, cardId);
    expect(authoredEdit.status).toBe(200);
    expect(authoredEdit.body.log).toContain('member:guest@example.com: desc +20 chars');
    const ownerCard = await request.get(`/api/board/${encodeURIComponent(cardId)}`, {
      headers: ownerHeaders,
    });
    expect(ownerCard.status()).toBe(200);
    expect(await ownerCard.json()).toMatchObject({
      creator: 'member:guest@example.com',
      log: expect.stringContaining('member:guest@example.com: desc +20 chars'),
    });
    await owner.evaluate(() => {
      document.getElementById('settings-menu')?.classList.remove('open');
      (window as any).switchView('board');
    });
    await owner.evaluate(() => (window as any).fetchBoard());
    await expect
      .poll(
        () =>
          owner.evaluate(
            `(typeof boardItems === 'undefined' ? [] : boardItems).map(i => String(i.title)).includes(${JSON.stringify(title)})`,
          ),
        { timeout: 5_000 },
      )
      .toBe(true);
    // The board defaults to "Working now"; a human-owned unclaimed card is
    // intentionally in Unowned, so select that real view before asserting the
    // owner can see the invitee's card in the UI.
    await owner.locator('.board-view-chip', { hasText: 'Unowned' }).click();
    await expect(owner.locator('#board-view')).toContainText(title);

    // Observability is shared too: the owner can see which member performed
    // the mutation, not merely that some request came from the Tailscale IP.
    await expect
      .poll(
        async () => {
          const response = await request.get(
            '/api/logs?session=' + encodeURIComponent('member:guest@example.com') + '&limit=100',
            { headers: ownerHeaders },
          );
          const payload = await response.json();
          return (payload.events || []).some(
            (event: any) => event.method === 'POST' && event.target === '/api/board',
          );
        },
        { timeout: 5_000 },
      )
      .toBe(true);
    const memberLogView = await guest.evaluate(async () => {
      const response = await fetch(
        '/api/logs?session=' + encodeURIComponent('member:guest@example.com') + '&limit=100',
      );
      const payload = await response.json();
      return {
        status: response.status,
        sawOwnMutation: (payload.events || []).some(
          (event: any) => event.method === 'POST' && event.target === '/api/board',
        ),
      };
    });
    expect(memberLogView).toEqual({ status: 200, sawOwnMutation: true });

    const members = await (
      await request.get('/api/org/members', { headers: ownerHeaders })
    ).json();
    const member = members.find((entry: any) => entry.email === 'guest@example.com');
    expect(member).toBeTruthy();

    // Rescoping is live: the existing HttpOnly cookie immediately changes from
    // global -> group -> worker without a new invite or login.
    const groupRescope = await request.patch(
      `/api/org/members/${encodeURIComponent(member.id)}`,
      {
        headers: ownerHeaders,
        data: { scope_level: 'group', scope_name: 'e2e-multiplayer' },
      },
    );
    expect(groupRescope.status()).toBe(200);
    const groupView = await guest.evaluate(async ({ groupPeer, outsider }) => {
      const identity = await (await fetch('/api/identity')).json();
      const fleetResponse = await fetch('/api/sessions');
      const fleet = await fleetResponse.json();
      const peer = await fetch(`/api/sessions/${encodeURIComponent(groupPeer)}/info`);
      const other = await fetch(`/api/sessions/${encodeURIComponent(outsider)}/info`);
      const board = await (await fetch('/api/board?all=1')).json();
      return {
        identity,
        names: fleet.map((row: any) => row.name),
        peerStatus: peer.status,
        outsiderStatus: other.status,
        boardTitles: board.map((row: any) => row.title),
      };
    }, { groupPeer, outsider });
    expect(groupView.identity.access_scope).toEqual({ level: 'group', name: 'e2e-multiplayer' });
    expect(groupView.names).toEqual(expect.arrayContaining([memberWorker, groupPeer]));
    expect(groupView.names).not.toContain(outsider);
    expect(groupView.peerStatus).toBe(200);
    expect(groupView.outsiderStatus).toBe(403);
    expect(groupView.boardTitles).not.toContain(title); // global/unassigned card is outside the group

    const workerRescope = await request.patch(
      `/api/org/members/${encodeURIComponent(member.id)}`,
      {
        headers: ownerHeaders,
        data: { scope_level: 'worker', scope_name: memberWorker },
      },
    );
    expect(workerRescope.status()).toBe(200);
    const outsideCardResponse = await request.post('/api/board', {
      headers: ownerHeaders,
      data: {
        title: `owner-card-outside-worker-scope-${Date.now()}`,
        type: 'chore', status: 'todo', session: groupPeer,
      },
    });
    expect(outsideCardResponse.status()).toBe(201);
    const outsideCardId = (await outsideCardResponse.json()).id as string;
    createdCards.push(outsideCardId);
    const workerView = await guest.evaluate(async ({ memberWorker, groupPeer, outsideCardId }) => {
      const identity = await (await fetch('/api/identity')).json();
      const fleet = await (await fetch('/api/sessions')).json();
      const deniedWorker = await fetch(`/api/sessions/${encodeURIComponent(groupPeer)}/info`);
      const deniedExistingCard = await fetch(`/api/board/${encodeURIComponent(outsideCardId)}`);
      const allowedCard = await fetch('/api/board', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          title: `worker-scoped-${Date.now()}`,
          type: 'chore', status: 'todo', session: memberWorker,
        }),
      });
      const allowedBody = await allowedCard.json();
      const deniedCard = await fetch('/api/board', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          title: `outside-worker-scope-${Date.now()}`,
          type: 'chore', status: 'todo', session: groupPeer,
        }),
      });
      return {
        identity,
        names: fleet.map((row: any) => row.name),
        deniedWorkerStatus: deniedWorker.status,
        deniedExistingCardStatus: deniedExistingCard.status,
        allowedCardStatus: allowedCard.status,
        allowedCardId: allowedBody.id,
        deniedCardStatus: deniedCard.status,
      };
    }, { memberWorker, groupPeer, outsideCardId });
    expect(workerView.identity.access_scope).toEqual({ level: 'worker', name: memberWorker });
    expect(workerView.names).toEqual([memberWorker]);
    expect(workerView.deniedWorkerStatus).toBe(403);
    expect(workerView.deniedExistingCardStatus).toBe(403);
    expect(workerView.allowedCardStatus).toBe(201);
    expect(workerView.deniedCardStatus).toBe(403);
    createdCards.push(workerView.allowedCardId);

    await request.delete(`/api/board/${encodeURIComponent(cardId)}`, { headers: ownerHeaders });
    createdCards.splice(createdCards.indexOf(cardId), 1);
    const revoked = await request.delete(`/api/org/members/${encodeURIComponent(member.id)}`, {
      headers: ownerHeaders,
    });
    expect(revoked.status()).toBe(200);
    const afterRevoke = await guest.evaluate(async () => (await fetch('/api/org/members')).status);
    expect(afterRevoke).toBe(401);
    expect(
      await guest.evaluate(
        async (workerName) =>
          (
            await fetch(`/api/sessions/${encodeURIComponent(workerName)}/send`, {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              body: JSON.stringify({ text: 'must remain revoked' }),
            })
          ).status,
        memberWorker,
      ),
    ).toBe(401);

    // The cookie is HttpOnly and survives member deletion. A full reload must
    // remain revoked; it must never fall through to the public owner shell and
    // receive the owner's bearer just because the member lookup now fails.
    await guest.reload();
    await guest.waitForFunction(() => '_AMUX_AUTH_TOKEN' in window);
    expect(await guest.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)).toBe('');
    const afterReload = await guest.evaluate(async () => (await fetch('/api/org/members')).status);
    expect(afterReload).toBe(401);
  } finally {
    for (const card of createdCards) {
      await request.delete(`/api/board/${encodeURIComponent(card)}`, { headers: ownerHeaders });
    }
    for (const worker of createdWorkers) {
      await request.delete(`/api/sessions/${encodeURIComponent(worker)}`, {
        headers: { ...ownerHeaders, 'X-Amux-UI-Token': ownerUiToken },
      });
    }
    // The deliberate mid-test revoke (above) never runs if an earlier
    // assertion throws, which leaves guest@example.com a permanent member —
    // every later spec sharing this project's server/home then finds a
    // non-empty member list where it expects a fresh install. Revoke by
    // email here too so a failure anywhere above this point still leaves the
    // server clean for whichever spec runs next.
    try {
      const remaining = await (
        await request.get('/api/org/members', { headers: ownerHeaders })
      ).json();
      const leftover = (remaining || []).find((entry: any) => entry.email === 'guest@example.com');
      if (leftover) {
        await request.delete(`/api/org/members/${encodeURIComponent(leftover.id)}`, {
          headers: ownerHeaders,
        });
      }
    } catch (e) {
      // Best-effort: do not let cleanup itself mask the real test failure.
    }
    await guestContext.close();
  }
});
