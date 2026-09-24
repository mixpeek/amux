// "All shown" delete: the count to type is never shown as a placeholder (it
// read as prefilled, so OK sent an empty box and every delete was refused).
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';
const app = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const body = app.slice(app.indexOf('async function runVisibleWorkerAction('), app.indexOf('function closeBulkActions('));
test('typed confirmation only for more than 3, with an empty placeholder', () => {
  assert.match(body, /if \(a\.danger && names\.length > 3\)/);
  assert.match(body, /showPrompt\('Type the number ' \+ names\.length \+ ' to delete ' \+ names\.length \+ ' workers', ''\)/);
  assert.doesNotMatch(body, /String\(names\.length\)\);\n/);
});
test('the prompt is focused and Enter submits', () => {
  const prompt = app.slice(app.indexOf('async function showPrompt('), app.indexOf('// ── Bulk actions modal ──'));
  assert.match(prompt, /event\.key===\\'Enter\\'/);
  assert.match(prompt, /\.focus\(\)/);
});
