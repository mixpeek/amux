import { test, expect, allowUnusedRoute } from './fixtures';
import { grantClipboard } from './helpers';

// Text-to-SQL "Ask" mode in the Database tab. The e2e servers use a non-secret
// test key, so the real Anthropic API will reject it. We intercept /api/sql/ask
// and return a canned response so the full UI flow can be exercised end to end.

const MOCK_ASK_RESPONSE = {
  sql: 'SELECT name, rows FROM sqlite_master WHERE type = \'table\' ORDER BY name LIMIT 10;',
  columns: ['name', 'rows'],
  rows: [['issues', 42], ['sessions', 17], ['prefs', 3]],
  rowcount: 3,
  truncated: false,
  via: 'mock',
  model_ms: 150,
  query_ms: 2,
  ms: 152,
  measured: true,
};

test.describe('Database Ask mode', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await page.waitForLoadState('networkidle');
  });

  test('Ask button appears in the Database tab mode bar', async ({ page }) => {
    await page.evaluate(() => (window as any).switchView('sql'));
    await page.waitForSelector('#db-mode-ask', { state: 'visible' });
    const btn = page.locator('#db-mode-ask');
    await expect(btn).toBeVisible();
    await expect(btn).toHaveText('Ask');
  });

  test('clicking Ask shows the Ask pane with input and empty state', async ({ page }) => {
    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();

    const pane = page.locator('#db-pane-ask');
    await expect(pane).toBeVisible();

    const input = page.locator('#db-ask-input');
    await expect(input).toBeVisible();
    await expect(input).toHaveAttribute('placeholder', /Ask a question/);

    const sendBtn = page.locator('#db-ask-send');
    await expect(sendBtn).toBeVisible();
    await expect(sendBtn).toHaveText('Ask');

    const empty = pane.locator('.db-ask-empty');
    await expect(empty).toBeVisible();
    await expect(empty).toContainText('plain English');
  });

  test('submitting a question shows loading then results (mocked)', async ({ page }) => {
    await page.route('**/api/sql/ask', async (route) => {
      const body = JSON.parse(route.request().postData() || '{}');
      expect(body.question).toBeTruthy();
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(MOCK_ASK_RESPONSE),
      });
    });

    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();

    const input = page.locator('#db-ask-input');
    await input.fill('Show all tables');
    await page.locator('#db-ask-send').click();

    // Empty state should disappear.
    await expect(page.locator('.db-ask-empty')).not.toBeVisible({ timeout: 5000 });

    // A turn should appear with the question text.
    const turn = page.locator('.db-ask-turn').first();
    await expect(turn).toBeVisible();
    await expect(turn.locator('.db-ask-q')).toContainText('Show all tables');

    // The generated SQL should be shown (as a highlighted code block).
    const sqlPre = turn.locator('.db-ask-sql-code');
    await expect(sqlPre).toBeVisible();
    await expect(sqlPre).toContainText('SELECT');

    // Results grid should be rendered.
    const grid = turn.locator('.db-ask-result-grid table.sqlr');
    await expect(grid).toBeVisible();

    // Meta line should show row count and timing.
    const meta = turn.locator('.db-ask-meta');
    await expect(meta).toContainText('3 rows');
    await expect(meta).toContainText('ms');
  });

  test('history is sent on follow-up questions', async ({ page }) => {
    let callCount = 0;
    await page.route('**/api/sql/ask', async (route) => {
      callCount++;
      const body = JSON.parse(route.request().postData() || '{}');
      if (callCount === 2) {
        expect(body.history).toHaveLength(1);
        expect(body.history[0].question).toBe('first question');
        expect(body.history[0].sql).toBeTruthy();
      }
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(MOCK_ASK_RESPONSE),
      });
    });

    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();

    await page.locator('#db-ask-input').fill('first question');
    await page.locator('#db-ask-send').click();
    await expect(page.locator('.db-ask-turn')).toHaveCount(1);

    await page.locator('#db-ask-input').fill('follow up');
    await page.locator('#db-ask-send').click();
    await expect(page.locator('.db-ask-turn')).toHaveCount(2);
    expect(callCount).toBe(2);
  });

  test('Copy button copies SQL to clipboard', async ({ page, context, browserName }) => {
    // WebKit has no clipboard permissions and asking is a hard error (see
    // grantClipboard), so capture what the button writes instead of reading back.
    await grantClipboard(context, browserName);
    await page.route('**/api/sql/ask', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(MOCK_ASK_RESPONSE),
      });
    });

    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();
    await page.locator('#db-ask-input').fill('test');
    await page.locator('#db-ask-send').click();
    await expect(page.locator('.db-ask-turn')).toHaveCount(1);

    await page.evaluate(() => {
      (window as any).__dbAskWrote = null;
      (navigator.clipboard as any).writeText = (t: string) => {
        (window as any).__dbAskWrote = t;
        return Promise.resolve();
      };
    });
    await page.locator('.db-ask-turn').first().locator('button', { hasText: 'Copy' }).click();
    const clip = await page.evaluate(() => (window as any).__dbAskWrote);
    expect(clip).toContain('SELECT');
  });

  test('SQL is syntax-highlighted with hljs', async ({ page }) => {
    await page.route('**/api/sql/ask', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(MOCK_ASK_RESPONSE),
      });
    });

    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();
    await page.locator('#db-ask-input').fill('test');
    await page.locator('#db-ask-send').click();
    await expect(page.locator('.db-ask-turn')).toHaveCount(1);

    // The code block should have hljs classes applied.
    const code = page.locator('.db-ask-turn').first().locator('.db-ask-sql-code code');
    await expect(code).toBeVisible();
    const cls = await code.getAttribute('class');
    expect(cls).toContain('hljs');
  });

  test('Query button opens Query pane with the SQL', async ({ page }) => {
    await page.route('**/api/sql/ask', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(MOCK_ASK_RESPONSE),
      });
    });

    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();
    await page.locator('#db-ask-input').fill('test');
    await page.locator('#db-ask-send').click();
    await expect(page.locator('.db-ask-turn')).toHaveCount(1);

    await page.locator('.db-ask-turn').first().locator('button', { hasText: 'Query' }).click();

    // Should switch to query mode.
    await expect(page.locator('#db-pane-query')).toBeVisible();
    await expect(page.locator('#db-mode-query')).toHaveClass(/active/);

    // SQL editor should have the generated SQL.
    const editor = page.locator('#sql-editor');
    await expect(editor).toHaveValue(/SELECT/);
  });

  test('Re-run button executes the SQL via /api/sql', async ({ page }) => {
    await page.route('**/api/sql/ask', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(MOCK_ASK_RESPONSE),
      });
    });
    let reranSql = false;
    await page.route('**/api/sql', async (route) => {
      if (route.request().method() === 'POST' && !route.request().url().includes('/ask')) {
        reranSql = true;
        const body = JSON.parse(route.request().postData() || '{}');
        expect(body.write).toBe(false);
        await route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({
            columns: ['count'],
            rows: [[99]],
            rowcount: 1,
            truncated: false,
            ms: 1,
          }),
        });
      } else {
        await route.continue();
      }
    });

    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();
    await page.locator('#db-ask-input').fill('test');
    await page.locator('#db-ask-send').click();
    await expect(page.locator('.db-ask-turn')).toHaveCount(1);

    await page.locator('.db-ask-turn').first().locator('button', { hasText: 'Re-run' }).click();

    // The meta should update to reflect the re-run.
    await expect(page.locator('.db-ask-turn').first().locator('.db-ask-meta')).toContainText('1 row');
    await expect(page.locator('.db-ask-turn').first().locator('.db-ask-meta')).toContainText('manual re-run');
    expect(reranSql).toBe(true);
  });

  test('API error is displayed gracefully', async ({ page }) => {
    await page.route('**/api/sql/ask', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          sql: 'SELECT bad FROM nonexistent;',
          error: 'no such table: nonexistent',
          via: 'mock',
          ms: 100,
          measured: true,
        }),
      });
    });

    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();
    await page.locator('#db-ask-input').fill('bad query');
    await page.locator('#db-ask-send').click();

    const turn = page.locator('.db-ask-turn').first();
    await expect(turn).toBeVisible();
    // SQL should still be shown (as highlighted code, editable on click).
    await expect(turn.locator('.db-ask-sql-code')).toContainText('SELECT bad');
    // Error should be visible.
    await expect(turn.locator('.db-ask-err')).toContainText('no such table');
  });

  test('Enter key submits the question', async ({ page }) => {
    let asked = false;
    await page.route('**/api/sql/ask', async (route) => {
      asked = true;
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(MOCK_ASK_RESPONSE),
      });
    });

    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();
    await page.locator('#db-ask-input').fill('enter test');
    await page.locator('#db-ask-input').press('Enter');

    await expect(page.locator('.db-ask-turn')).toHaveCount(1);
    expect(asked).toBe(true);
  });

  test('Cmd+Enter also submits', async ({ page }) => {
    let asked = false;
    await page.route('**/api/sql/ask', async (route) => {
      asked = true;
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(MOCK_ASK_RESPONSE),
      });
    });

    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();
    await page.locator('#db-ask-input').fill('cmd enter test');
    await page.locator('#db-ask-input').press('Meta+Enter');

    await expect(page.locator('.db-ask-turn')).toHaveCount(1);
    expect(asked).toBe(true);
  });

  test('input is cleared and button disabled during request', async ({ page }) => {
    let resolveRoute: (() => void) | null = null;
    await page.route('**/api/sql/ask', async (route) => {
      await new Promise<void>((r) => { resolveRoute = r; });
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(MOCK_ASK_RESPONSE),
      });
    });

    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();
    await page.locator('#db-ask-input').fill('loading test');
    await page.locator('#db-ask-send').click();

    // During the request: input cleared, button shows ellipsis.
    await expect(page.locator('#db-ask-input')).toHaveValue('');
    await expect(page.locator('#db-ask-send')).toBeDisabled();
    await expect(page.locator('#db-ask-send')).toHaveText('…');

    // Loading indicator visible.
    await expect(page.locator('.db-ask-loading')).toBeVisible();

    resolveRoute!();

    // After response: button re-enabled.
    await expect(page.locator('#db-ask-send')).toBeEnabled();
    await expect(page.locator('#db-ask-send')).toHaveText('Ask');
  });

  test('empty input does not submit', async ({ page }) => {
    let asked = false;
    await page.route('**/api/sql/ask', async (route) => {
      asked = true;
      await route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
    });
    // The point of this test is that the stub never fires.
    allowUnusedRoute(page, '**/api/sql/ask');

    await page.evaluate(() => (window as any).switchView('sql'));
    await page.locator('#db-mode-ask').click();
    await page.locator('#db-ask-send').click();
    await page.waitForTimeout(300);
    expect(asked).toBe(false);
  });
});
