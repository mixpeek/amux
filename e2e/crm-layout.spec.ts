// The CRM looks right on every device, with a real amount of data (owner,
// 2026-11-27: "Test on all devices and make sure that you can visibly see
// everything and it's all pretty", with a screenshot of 87 drafts whose rows
// had squashed so their lines spilled over the borders). A handful of rows
// never showed it, so this seeds a long list with long names, then walks
// every section at phone, tablet, laptop and wide widths, in both themes, and
// measures what a person would see: no box whose content spills out of it,
// nothing past the right edge, the page never scrolls sideways, and the
// record's buttons fit their row. Screenshots of each are attached.
import { test, expect, Page } from './fixtures';

const api = (page: Page, method: string, path: string, body?: any, worker = '') => page.evaluate(async ([m, p, b, w]) => {
  const h: Record<string, string> = b ? { 'Content-Type': 'application/json' } : {};
  if (w) h['X-Amux-Session'] = w;
  const r = await fetch('/api/crm' + p, { method: m, headers: (window as any)._authHeaders(h), body: b ? JSON.stringify(b) : undefined });
  return { status: r.status, body: await r.json().catch(() => ({})) };
}, [method, path, body, worker] as const);

const FIRST = ['Nina', 'Jon', 'Maya', 'Sam', 'Leah', 'Ricardo', 'Manuel Salvatore', 'Valery', 'Muath', 'Todd', 'Mohammad Reza', 'Aldrin'];
const LAST = ['Park', 'Bell', 'Ortiz', 'Reed', 'Wu', 'Martone', 'Litvin', 'Juady', 'Rasband', 'Taesiri', 'Jenson', 'Kulesh'];
const CO = ['Renderwell', 'Northline', 'Patchworks', 'Dockside', 'Fieldnote', 'Relay Lab', 'cyberinference', 'Ruby Cluster', 'Athena-Intel Systems International', 'engram-app'];

// Everything a person sees in the CRM: the problems, in words.
async function layoutProblems(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const out: string[] = [];
    const vw = document.documentElement.clientWidth;
    if (document.documentElement.scrollWidth > vw + 1) out.push('the page scrolls sideways by ' + (document.documentElement.scrollWidth - vw) + 'px');
    const shown = (el: Element) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== 'hidden'; };
    const name = (el: Element) => (el.className + ' "' + ((el as HTMLElement).innerText || '').replace(/\s+/g, ' ').slice(0, 40) + '"');
    const boxes = '#crm-view .crm-row, #crm-view .crm-act, #crm-view .crm-funnel, #crm-view .crm-step, #crm-view .crm-bulk, #crm-view .crm-stat, #crm-view .crm-nav-item, #crm-drawer .crm-card, #crm-drawer .crm-row, #crm-drawer .crm-ix, #crm-drawer .crm-ch, #crm-view .cx-filter, #crm-drawer .btn, #crm-view .btn';
    for (const el of Array.from(document.querySelectorAll(boxes))) {
      if (!shown(el)) continue;
      if (el.scrollHeight > el.clientHeight + 2) out.push('content spills out of ' + name(el) + ' (' + el.scrollHeight + ' > ' + el.clientHeight + ')');
      const r = el.getBoundingClientRect();
      // A control in the one sideways-scrolling bar may sit past the edge; anything else may not.
      if (r.right > vw + 1 && !el.closest('#crm-bar')) out.push(name(el) + ' runs past the right edge');
    }
    for (const el of Array.from(document.querySelectorAll('#crm-drawer .crm-decide, #crm-drawer .crm-dhead, #crm-head, .crm-stats, #crm-drawer .crm-checks, #crm-drawer .crm-tags'))) {
      if (shown(el) && el.scrollWidth > el.clientWidth + 1) out.push('a row is wider than its box: ' + name(el));
    }
    // The record's actions are one row: a second row eats the draft being reviewed.
    const decide = document.querySelector('#crm-drawer .crm-decide');
    if (decide && shown(decide) && decide.getBoundingClientRect().height > 72) out.push('the actions take more than one row (' + Math.round(decide.getBoundingClientRect().height) + 'px)');
    return out.slice(0, 12);
  });
}

test('every CRM section fits and reads cleanly at every width, in both themes, with a long list', async ({ page }, info) => {
  test.setTimeout(300_000);
  const R = info.project.name + '-' + Math.random().toString(36).slice(2, 6);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._crmSet === 'function');

  // A realistic CRM: 36 people at 10 companies, a drafted email or DM for each
  // (some long), a sequence, deals, a follow-up, and decisions in history.
  const seq = await api(page, 'POST', '/sequences', { name: 'GitHub stargazers ' + R, goal: 'Ask how they hand off agent work', steps: [
    { day: 0, channel: 'email', action: 'email' }, { day: 3, channel: 'linkedin', action: 'dm' }, { day: 7, channel: 'post', action: 'letter' }] }, 'amux-gtm');
  expect(seq.status).toBe(201);
  const accounts: string[] = [];
  for (let i = 0; i < CO.length; i++) {
    const a = await api(page, 'POST', '/accounts', { name: CO[i] + ' ' + R, domain: CO[i].toLowerCase().replace(/[^a-z]/g, '') + '-' + R + '.io', industry: i % 2 ? 'Developer tools' : 'Media and entertainment software',
      icp_grade: 'ABCD'[i % 4], icp_reasons: 'GitHub stargazer, a team of 20 to 200 engineers running coding agents' });
    expect(a.status).toBe(201);
    accounts.push(a.body.id);
  }
  const people: string[] = [];
  for (let i = 0; i < 36; i++) {
    const name = FIRST[i % FIRST.length] + ' ' + LAST[(i * 5) % LAST.length] + ' ' + R + '-' + i;
    const p = await api(page, 'POST', '/contacts', { name, email: `p${i}.${R}@${CO[i % CO.length].toLowerCase().replace(/[^a-z]/g, '')}.io`, company: CO[i % CO.length] + ' ' + R, account_id: accounts[i % CO.length],
      role: i % 3 ? 'Founder' : 'Head of Platform Engineering and Developer Experience', icp_grade: 'ABCD'[i % 4], linkedin: 'linkedin.com/in/p' + i + R,
      notes: 'Starred amux after exploring tools for concurrent coding sessions. Ask how the team hands off agent work.' });
    expect(p.status).toBe(201);
    people.push(p.body.id);
    const long = i % 4 === 0;
    const body = (i % 2 ? '' : 'Subject: ' + (long ? 'A longer question about how your team hands off long-running coding agents between people and shifts' : 'you starred amux') + '\n\n')
      + 'Hi ' + FIRST[i % FIRST.length] + ',\n\nThanks for starring amux. ' + (long ? 'You care about self-hosting and open protocols, so you may like that amux runs entirely on your own box: one binary and a SQLite file. '.repeat(3) : '') + 'What do you run in it?\n\nEthan';
    const t = await api(page, 'POST', '/touches', { contact_id: p.body.id, channel: i % 2 ? 'linkedin' : 'email', action: i % 2 ? 'dm' : 'email', body, due_date: '2026-11-2' + (i % 9), sequence: i < 6 ? seq.body.id : '', step: i < 6 ? 1 : 0 }, 'amux-gtm');
    expect(t.status).toBe(201);
  }
  // Six of them enrolled, in a cohort, so the Funnel has something to draw.
  expect((await api(page, 'POST', '/sequences/' + seq.body.id + '/enroll', { contact_ids: people.slice(6, 12), cohort: 'layout-' + R }, 'amux-gtm')).status).toBe(200);
  const owner = (m: string, p: string, b?: any) => api(page, m, p, b);
  const drafts = (await owner('GET', '/touches?status=drafted&limit=1000')).body.filter((t: any) => (t.contact_name || '').includes(R));
  await owner('POST', '/touches/' + drafts[0].id + '/approve');
  await owner('PATCH', '/touches/' + drafts[1].id, { status: 'skipped', result: 'declined by the owner: too generic' });
  await owner('PATCH', '/contacts/' + drafts[2].contact_id, { unsubscribed: true });
  const acct = accounts[0];
  for (const [title, stage, amount] of [['Pilot ' + R, 'demo', 12000], ['Cloud signup ' + R, 'lead', null], ['Enterprise rollout across three regions ' + R, 'proposal', 240000]] as const)
    await owner('POST', '/deals', { title, stage, amount, account_id: acct, next_step: 'Send the security questionnaire back' });
  const today = await page.evaluate(() => (window as any)._crmToday());
  await owner('POST', '/contacts/' + people[3] + '/interactions', { type: 'call', notes: 'Intro call, wants a demo', follow_up_date: today, follow_up_note: 'Pick up our meetup conversation' });

  await page.evaluate(() => { (window as any).switchView('crm'); });
  const sections = ['outbound', 'people', 'companies', 'deals', 'followups', 'sequences', 'funnel', 'activity'];
  const phone = info.project.name !== 'desktop';
  // Desktop covers the laptop, tablet and wide screens; the phone projects their own device.
  const sizes = phone ? [page.viewportSize()!] : [{ width: 1280, height: 800 }, { width: 1024, height: 768 }, { width: 820, height: 1180 }, { width: 1440, height: 900 }, { width: 1920, height: 1080 }];
  const problems: string[] = [];
  for (const size of sizes) {
    await page.setViewportSize(size);
    for (const theme of ['dark', 'light']) {
      // The app's own theme switch (themes paint their colours as CSS variables).
      await page.evaluate(t => (window as any)._applyTheme(t === 'light'), theme);
      await expect(page.locator('body')).toHaveAttribute('data-theme', 'amux-' + theme);
      for (const v of sections) {
        const q = v === 'sequences' || v === 'deals' || v === 'funnel' ? '' : R;
        // _crm is a top-level const: reachable from page script, not from window.
        await page.evaluate(`_crmClose(); _crm.q = ${JSON.stringify(q)}; _crmSet('view', ${JSON.stringify(v)})`);
        const input = page.locator('#crm-q');
        await input.evaluate((el: HTMLInputElement, val) => { el.value = val; }, q);
        await expect(page.locator('#crm-list .crm-row, #crm-list .crm-act, #crm-list .crm-funnel').first()).toBeVisible({ timeout: 15_000 });
        await page.waitForTimeout(300);
        // Open a record where the section has one: the long draft, a person, a company, a deal.
        const opener = v === 'outbound' ? '#crm-list .crm-row' : ['people', 'companies', 'deals'].includes(v) ? '#crm-list .crm-row[data-kind]' : '';
        let opened = false;
        if (opener && (theme === 'dark' || size.width === sizes[0].width)) {
          await page.locator(opener).first().click();
          await expect(page.locator('#crm-drawer .cx-dbody')).toBeVisible({ timeout: 15_000 });
          await page.waitForTimeout(700);
          opened = true;
        }
        const found = await layoutProblems(page);
        for (const f of found) problems.push(`${size.width}x${size.height} ${theme} ${v}${opened ? ' (record open)' : ''}: ${f}`);
        await page.screenshot({ path: info.outputPath(`crm-${size.width}-${theme}-${v}.png`) });
        if (opened && phone) {
          // The record over the list scrolls to its end: nothing hidden under the action row.
          await page.locator('#crm-drawer .cx-dbody').evaluate(el => el.scrollTo(0, el.scrollHeight));
          await page.screenshot({ path: info.outputPath(`crm-${size.width}-${theme}-${v}-end.png`) });
        }
      }
    }
  }
  expect(problems, problems.join('\n')).toEqual([]);
});
