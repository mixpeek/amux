// The owner, 2026-11-13: "Make sure I can disable/enable notifications on the
// amux ui and the device/client respects it". Settings > Alerts > This device
// and the bell panel's speaker button are one switch, and with it off nothing
// reaches the device: no native notification for a Messages post, no
// background push subscription left behind, and in the Mac app the app's own
// switch (amuxNotifyPrefs) is the one the page reads and sets.
import { test, expect } from './fixtures';

// The post the page asks for (?after=<id - 1>), so each call finds its own.
const postsFor = (r: any) => {
  const id = Number(new URL(r.request().url()).searchParams.get('after')) + 1;
  return r.fulfill({ json: { posts: [{ id, author_key: 'w:someone', author_name: 'Someone', body: 'hello', mentions: [] }] } });
};

async function openAlerts(page) {
  await page.evaluate(() => { (window as any).toggleSettings(); (window as any)._settingsTab('notifications'); });
  await expect(page.locator('#notif-device-native-cb')).toBeAttached();
}

// A hidden page with permission granted and a Notification that counts.
const countNotifications = () => {
  (window as any)._shown = [];
  class N { onclick: any; constructor(t: string) { (window as any)._shown.push(t); } static permission = 'granted'; static requestPermission = async () => 'granted'; }
  (window as any).Notification = N;
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => true });
};

test('the Settings switch and the bell button are one switch, and a Messages post respects it', async ({ page }) => {
  // First load only: the reload below must see what the switch saved.
  await page.addInitScript(() => { localStorage.setItem('amux_walkthrough_done', '1'); if (localStorage.getItem('amux_notifs') === null) localStorage.setItem('amux_notifs', '1'); });
  await page.route('**/api/conversations/*/posts*', postsFor);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvMaybeNotify === 'function');
  await openAlerts(page);
  const cb = page.locator('#notif-device-native-cb');
  await expect(cb).toBeChecked();
  await expect(page.locator('#notif-native-btn')).toHaveAttribute('aria-pressed', 'true');

  await page.evaluate(countNotifications);
  await page.evaluate(() => (window as any)._cvMaybeNotify({ id: 5, conv: 'd:someone' }));
  expect(await page.evaluate(() => (window as any)._shown.length), 'on: a post addressed to you notifies').toBe(1);

  // Off from Settings: the bell button follows, the device keeps it, and the
  // next post raises nothing.
  await page.locator('#notif-device-native-cb + .theme-track').click();
  await expect(cb).not.toBeChecked();
  await expect(page.locator('#notif-native-btn')).toHaveAttribute('aria-pressed', 'false');
  expect(await page.evaluate(() => localStorage.getItem('amux_notifs'))).toBe('0');
  await page.evaluate(() => (window as any)._cvMaybeNotify({ id: 6, conv: 'd:someone' }));
  expect(await page.evaluate(() => (window as any)._shown.length), 'off: nothing reaches the device').toBe(1);

  // In-app pop-ups have their own switch in the same place.
  const bcb = page.locator('#notif-device-banner-cb');
  await expect(bcb).toBeChecked();
  await page.locator('#notif-device-banner-cb + .theme-track').click();
  await expect(bcb).not.toBeChecked();
  expect(await page.evaluate(() => localStorage.getItem('amux_notif_banners'))).toBe('0');
  expect(await page.evaluate(() => (window as any).amuxNotifyAllowed('banner'))).toBe(false);

  await page.reload();
  await openAlerts(page);
  await expect(page.locator('#notif-device-native-cb')).not.toBeChecked();
  await expect(page.locator('#notif-device-banner-cb')).not.toBeChecked();
});

test('a device switched off ends a push subscription left behind, even when the server cannot be told', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.setItem('amux_notifs', '0');
    (window as any)._unsubscribed = 0;
    const sub = { endpoint: 'https://push.example/left-behind', unsubscribe: async () => { (window as any)._unsubscribed++; return true; } };
    const reg = { pushManager: { getSubscription: async () => ((window as any)._unsubscribed ? null : sub) } };
    Object.defineProperty(ServiceWorkerContainer.prototype, 'ready', { configurable: true, get: () => Promise.resolve(reg) });
  });
  let told = 0;
  await page.route('**/api/push/unsubscribe', r => { told++; return r.fulfill({ status: 500, json: { error: 'down' } }); });
  await page.goto('/');
  await page.waitForFunction(() => (window as any)._unsubscribed === 1, null, { timeout: 15000 });
  expect(told, 'the server was asked to drop it').toBe(1);
});

test('in the Mac app the page shows and sets the app\'s switch', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    localStorage.setItem('amux_notifs', '0');   // the browser's value, which the app overrides
    (window as any)._prefs = []; (window as any)._posted = [];
    (window as any).webkit = { messageHandlers: {
      amuxNotifyPrefs: { postMessage: (m: any) => (window as any)._prefs.push(m) },
      amuxNotify: { postMessage: (m: any) => (window as any)._posted.push(m) },
    } };
  });
  await page.route('**/api/conversations/*/posts*', postsFor);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._cvMaybeNotify === 'function');
  expect(await page.evaluate(() => (window as any)._prefs[0]), 'the page asks the app').toEqual({ op: 'get' });
  await page.evaluate(() => (window as any)._notifDesktopState(true));
  await openAlerts(page);
  await expect(page.locator('#notif-device-native-cb')).toBeChecked();
  await page.evaluate(() => (window as any)._cvMaybeNotify({ id: 5, conv: 'd:someone' }));
  expect(await page.evaluate(() => (window as any)._posted.length), 'on: the app is asked to notify').toBe(1);

  await page.locator('#notif-device-native-cb + .theme-track').click();
  expect(await page.evaluate(() => (window as any)._prefs.at(-1))).toEqual({ op: 'set', on: false });
  await expect(page.locator('#notif-device-native-cb')).not.toBeChecked();
  await page.evaluate(() => (window as any)._cvMaybeNotify({ id: 6, conv: 'd:someone' }));
  expect(await page.evaluate(() => (window as any)._posted.length), 'off: the app is not asked').toBe(1);

  // The menu or the app's own Settings turning it back on reaches the page.
  await page.evaluate(() => (window as any)._notifDesktopState(true));
  await expect(page.locator('#notif-device-native-cb')).toBeChecked();
});
