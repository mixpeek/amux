import {test,expect,Page} from './fixtures';

// Offline mode being unavailable is CONNECTION STATE, so it lives on the
// connection badge and in the Connection modal. It used to be a fixed red bar at
// z-index 9999 that sat on top of modal action rows (AMUX-2584, and the Create
// Worker modal's Create button on 2026-09-24). Ethan: "we already have a status
// thing in the top left. we need to be more conservative about real estate."
test.use({viewport:{width:375,height:667},serviceWorkers:'block'});

async function failOfflineMode(page:Page) {
  await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
  await page.route('**/api/offline-origin',r=>r.fulfill({json:{proxied:false,why:'the certificate is not trusted',good_origin:'https://amux.example.invalid'}}));
  await page.goto('/');
  await page.waitForFunction(()=>typeof (window as any)._swOfferGoodOrigin==='function');
  await page.evaluate(()=>(window as any)._swOfferGoodOrigin());
}

test('a failed service worker marks the badge and overlays nothing',async({page})=>{
  await failOfflineMode(page);
  const badge=page.locator('#conn-status').first();
  await expect(badge).toHaveClass(/no-offline/);
  await expect(badge).toHaveAttribute('aria-label',/offline mode off/);
  await expect(page.locator('#sw-fail-bar')).toHaveCount(0);
  // The badge TEXT stays the connection state; the marker is decoration.
  await expect(badge).toHaveText(/^(Live|Polling|Offline|\d+ pending|Sync error|Access required)$/);
  // The reason and the repair action are one tap away, in the Connection modal.
  await page.evaluate(()=>(window as any).showConnHistory());
  const notice=page.locator('#conn-hist-modal .conn-sw-notice');
  await expect(notice).toContainText('Offline mode is off on this device');
  await expect(notice).toContainText('the certificate is not trusted');
  await expect(notice.getByRole('button',{name:'Open'})).toBeVisible();
});

for (const [label,open,button] of [
  ['board edit Save','openBoardAdd("todo")','.be-save'],
  ['Create Worker Create','openCreate()','button[onclick="submitCreate()"]'],
] as const) test(`${label} is fully tappable at 375px with offline mode off`,async({page})=>{
  await failOfflineMode(page);
  await page.evaluate(open);
  const hits=await page.evaluate(sel=>{
    const b=document.querySelector(sel)!;b.scrollIntoView({block:'nearest'});const a=b.getBoundingClientRect();
    const pts=[[a.left+4,a.top+4],[a.right-4,a.top+4],[a.left+4,a.bottom-4],[a.right-4,a.bottom-4],[a.left+a.width/2,a.top+a.height/2]];
    return pts.map(([x,y])=>b.contains(document.elementFromPoint(x,y)));
  },button);
  expect(hits.length).toBe(5);
  expect(hits.every(Boolean),`every corner of ${button} must hit the button`).toBe(true);
});
