// A held email's attachments are on its approval card (Ethan, 2026-11-09:
// "make sure amux supports attachments too in the approval flow"). Each file
// is a chip with its name and size; a click previews the bytes that will be
// sent, read from the held draft, in the composer's own preview dialog. A file
// the send cannot read says so on the chip.
import { test, expect } from './fixtures';

const ID = 'apr_attach000001';

function approval() {
  return {
    id: ID, session: 'gtm-engine', age_s: 60, expires: false, expires_in_s: null, requests: 1,
    preview: { endpoint: 'reply', from: 'info@example.com', to: 'buyer@example.org', subject: 'Re: Quote',
      reply_all: false, body: 'Quote attached.', external_recipients: ['buyer@example.org'] },
    review: { body: 'Quote attached.', body_complete: true, includes_signature: true, complete: false, attachment_count: 2,
      attachments: [
        { index: 0, filename: 'terms.txt', content_type: 'text/plain', size: 2048, ok: true },
        { index: 1, filename: 'gone.pdf', content_type: null, size: null, ok: false },
      ] },
  };
}

async function open(page: any, previews = true) {
  const asked: string[] = [];
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/email\/approvals(?:\?.*)?$/, (r: any) => r.fulfill({ json: { pending: [approval()], scheduled: [] } }));
  if (previews) await page.route(/\/api\/email\/approvals\/[^/]+\/attachments\/\d+/, (r: any) => {
    asked.push(new URL(r.request().url()).pathname);
    return r.fulfill({ status: 200, contentType: 'text/plain', body: 'Net 30. Prices hold for 60 days.' });
  });
  await page.route(/\/api\/grants(?:\?.*)?$/, (r: any) => r.fulfill({ json: { grants: [] } }));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._approvalsRefresh === 'function');
  await page.evaluate(() => (window as any)._approvalsRefresh());
  return asked;
}

test('each held attachment is named on the card and previews the bytes that will send', async ({ page }) => {
  const asked = await open(page);
  const chips = page.locator('#email-approvals-banner .appr-attach-chip');
  await expect(chips).toHaveCount(2);
  await expect(chips.nth(0)).toContainText('terms.txt');
  await expect(chips.nth(0)).toContainText('2 KB');
  await expect(chips.nth(1)).toContainText('gone.pdf');
  await expect(chips.nth(1)).toContainText('cannot be read');
  await expect(chips.nth(1)).toHaveClass(/appr-attach-bad/);

  await chips.nth(0).click();
  const modal = page.locator('.modal.attach-preview');
  await expect(modal).toBeVisible();
  await expect(modal).toContainText('terms.txt');
  await expect(modal).toContainText('attached, sends with this email');
  await expect(modal.locator('.attach-preview-text')).toHaveText('Net 30. Prices hold for 60 days.');
  expect(asked).toEqual([`/api/email/approvals/${ID}/attachments/0`]);
  await modal.locator('.modal-close').click();

  // The unreadable one opens, says why, and asks the server for nothing.
  await chips.nth(1).click();
  await expect(page.locator('.modal.attach-preview')).toContainText('cannot be read, so it will not send');
  expect(asked).toHaveLength(1);
});

test('on a phone the chips fit the card and meet the touch floor', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 760 });
  await open(page, false);
  const chip = page.locator('#email-approvals-banner .appr-attach-chip').first();
  await expect(chip).toBeVisible();
  const box = await chip.boundingBox();
  expect(box!.height).toBeGreaterThanOrEqual(44);
  expect(box!.x + box!.width).toBeLessThanOrEqual(375);
});
