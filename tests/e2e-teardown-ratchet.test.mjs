import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const E2E = fileURLToPath(new URL('../e2e', import.meta.url));

/**
 * AMUX-4737. A `finally` that throws DISCARDS the exception coming out of
 * `try`, so a teardown failure silently REPLACES the error the test was
 * actually reporting.
 *
 * Measured cost: AMUX-4642 and AMUX-4643 sat for two days titled with their
 * teardown's secondary failure ("Back click times out", "request context
 * closed") while the steps that broke were linked-record.spec.ts:78 and
 * files-upload.spec.ts:35. Both were the same dialog bug; three wrong diagnoses
 * were produced before a teardown was made non-throwing, and the real error
 * appeared immediately.
 *
 * THIS IS A RATCHET, NOT A GATE, and the distinction is the honest part. 47
 * unguarded blocks remain. Converting them all at once is a large mechanical
 * change across the whole e2e suite with real regression risk and no way to
 * verify each one except by breaking it. So the number is recorded and may only
 * go DOWN. Lower it when you convert one; it is not allowed to rise.
 *
 * WHAT THE NUMBER COUNTS, precisely, because it is not "unsafe teardowns". It
 * counts `finally` blocks that await something other than `cleanup()`. A block
 * awaiting a helper that is itself non-throwing reads as unguarded here: the
 * scanner cannot see inside a callee. `deleteOwnedWorkers` is exactly that case
 * and was fixed at the helper (it closes the overlay by calling its handler
 * rather than clicking it), so its callers are safe and still counted.
 *
 * That over-count is deliberate rather than a defect to fix with an allowlist:
 * an allowlist of known-safe helpers is a second list to keep true, and the
 * ratchet only needs to stop the number RISING. The baseline was set from a
 * measured run, not an estimate; the first guess was 46 and the scanner found
 * 47.
 *
 * The complementary fix already exists for fixture-managed cleanup:
 * e2e/worker-lifecycle-fixture.ts plus tests/worker-lifecycle-cleanup.test.mjs
 * prove the same property through a real Playwright run
 * ("mid-flow failure remains the only error after guarded fixture removal").
 * `e2e/teardown.ts`'s `cleanup()` is the per-spec form for the blocks no
 * fixture owns.
 */
const BASELINE = 47;

function walk(dir) {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap(e => {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) return walk(p);
    return e.isFile() && p.endsWith('.ts') ? [p] : [];
  });
}

/** The body of each `finally { ... }`, matched by brace depth rather than regex. */
function finallyBodies(src) {
  const out = [];
  const re = /\}\s*finally\s*\{/g;
  let m;
  while ((m = re.exec(src)) !== null) {
    let depth = 1, i = m.index + m[0].length;
    const start = i;
    while (i < src.length && depth > 0) {
      if (src[i] === '{') depth++;
      else if (src[i] === '}') depth--;
      i++;
    }
    out.push({ body: src.slice(start, i - 1), line: src.slice(0, m.index).split('\n').length });
  }
  return out;
}

/**
 * Unguarded = the block does work that can throw, and that work is not entirely
 * routed through `cleanup()`. A block whose every awaited call is a `cleanup(`
 * is already safe, which is what makes the number able to fall.
 */
function unguarded(body) {
  // What runs INSIDE cleanup()'s callback is caught by cleanup() itself, so it
  // is dropped before counting. Without this, every block written the way the
  // assertion below asks for (`await cleanup('x', async () => { await ... })`)
  // read as unguarded: chat-reconnect.spec.ts and files-worker-only.spec.ts
  // took the ratchet from 47 to 52 on 2026-11-27 with five correct blocks.
  body = withoutCleanupArgs(body);
  const doesWork = /\bawait\b|\brequest\./.test(body);
  if (!doesWork) return false;
  const awaits = body.match(/await\s+[A-Za-z_$][\w$.]*/g) || [];
  if (awaits.length === 0) return true;
  return !awaits.every(a => /await\s+cleanup\b/.test(a));
}

/** `body` with the argument list of every `cleanup(...)` call removed, matched by paren depth. */
function withoutCleanupArgs(body) {
  let out = '', i = 0;
  const re = /\bcleanup\s*\(/g;
  let m;
  while ((m = re.exec(body)) !== null) {
    out += body.slice(i, m.index + m[0].length);
    let depth = 1, j = m.index + m[0].length;
    while (j < body.length && depth > 0) {
      if (body[j] === '(') depth++;
      else if (body[j] === ')') depth--;
      j++;
    }
    out += ')';
    i = j;
    re.lastIndex = j;
  }
  return out + body.slice(i);
}

test('a teardown routed through cleanup() is guarded, whatever its callback awaits', () => {
  const wrapped = `\n await cleanup('x', async () => {\n await page.unroute(R);\n await request.delete(u);\n }, info);\n`;
  assert.equal(unguarded(wrapped), false, 'awaits inside cleanup()\'s callback are caught by cleanup()');
  assert.equal(unguarded(`\n await page.close();\n`), true, 'a bare await in finally can throw');
  assert.equal(unguarded(wrapped + ` await page.close();\n`), true, 'an await after the cleanup() call is still bare');
  assert.equal(unguarded(`\n await cleanup('a', () => f(g(1)), info);\n await x.y();\n`), true, 'nested parens in the callback do not hide a later bare await');
});

test('the set of teardowns that can discard a test error only shrinks', () => {
  const found = [];
  for (const file of walk(E2E)) {
    const src = fs.readFileSync(file, 'utf8');
    for (const { body, line } of finallyBodies(src)) {
      if (unguarded(body)) found.push(`${path.relative(E2E, file)}:${line}`);
    }
  }

  // A POSITIVE CONTROL, because a counter that can only read zero proves
  // nothing about the scanner. If this ever finds none, the scanner broke
  // rather than the suite becoming perfect, and the assertion below would then
  // pass for the wrong reason forever.
  assert.ok(found.length > 0,
    'the scanner found NO finally blocks at all, which means it stopped working, ' +
    'not that every teardown became safe');

  assert.ok(found.length <= BASELINE,
    `unguarded teardowns rose to ${found.length} (baseline ${BASELINE}).\n` +
    'A `finally` that throws DISCARDS the test error. Route the cleanup through\n' +
    "e2e/teardown.ts's cleanup(), which reports the teardown failure and lets the\n" +
    'real one escape:\n' +
    `  ${found.slice(0, 8).join('\n  ')}`);

  if (found.length < BASELINE) {
    // Say so rather than pass quietly: a baseline nobody lowers is a baseline
    // that stops meaning anything.
    console.log(
      `teardown ratchet: ${found.length} unguarded, baseline ${BASELINE}. ` +
      `LOWER BASELINE to ${found.length} in tests/e2e-teardown-ratchet.test.mjs.`);
  }
});
