import {test, expect} from './fixtures';

// Ethan, 2026-10-26: "make sure whatever view i last used is saved local. for
// cal". Every calendar view comes back on reload, on desktop and phone, and
// so does the date being looked at.
const VIEWS = ['dayGridMonth', 'timeGridWeek', 'timeGridDay', 'timeGrid3Day', 'listWeek'];

test('the calendar reopens on the view and date last used', async ({page}) => {
  await page.goto('/#view=calendar');
  await page.waitForFunction(() => (window as any).eval('typeof _fcInstance !== "undefined" && !!_fcInstance'), null, {timeout: 20000});
  const shown = () => page.evaluate(() => { const c = (window as any).eval('_fcInstance');
    return {view: c.view.type, start: c.view.currentStart.toDateString(), end: c.view.currentEnd.toDateString()}; });
  for (const view of VIEWS) {
    await page.evaluate(v => { (window as any).eval('_fcInstance').changeView(v, '2026-03-11'); }, view);
    const before = await shown();
    expect(before.view).toBe(view);
    await page.reload();
    await page.waitForFunction(() => (window as any).eval('typeof _fcInstance !== "undefined" && !!_fcInstance'), null, {timeout: 20000});
    expect(await shown(), `reload kept ${view} on the same dates`).toEqual(before);
  }
});
