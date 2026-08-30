import { test, expect } from '@playwright/test';

// AF-235 — the escape hatch for a stuck attachment.
//
// Diagnosed by @Dygreens on PR #124, from a real user report: "I can't hit send
// because there are still pictures that seem to be trying to upload and no
// delete button. No remove button no way for me to send this prompt." The PR
// conflicted after AMUX-3372 restructured the upload path onto sinks; the
// defects were re-verified live and fixed here, keeping their diagnosis.
//
// THE TEST DRIVES A STALLED / FAILING TRANSFER, because that is the only state
// the bug exists in — a spec using an instantly-completing file passes against
// the broken code. `_runUpload(f, sink)` and `_blockedByAttachment(files)` both
// take their collaborators as parameters, so the fakes below are the real code
// paths with the network and the sink swapped, not a paraphrase of them.

const FAKE_FILE = `(() => {
  const blob = new Blob(['x'.repeat(64)], { type: 'image/png' });
  const f = new File([blob], 'photo.png', { type: 'image/png' });
  return f;
})()`;

test('an upload survives the peek being closed mid-transfer', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  const r = await page.evaluate(async ([mkFile]) => {
    const w = window as any;
    const file = eval(mkFile as string);
    // A sink whose `has()` is ALWAYS FALSE. That is exactly what closing the
    // peek used to produce: `_peekFilesStash` sets `peekFiles = []`, so the
    // membership test the loop used to run ("is my placeholder still in the
    // array?") answered no and the transfer returned at chunk 0. The chip was
    // left in the stashed array at 0% with nothing left to drive it.
    const dropped: any[] = [];
    const sink = { push: () => {}, has: () => false, drop: (p: any) => dropped.push(p), render: () => {} };
    const orig = w.fetch;
    w.fetch = async (u: any) => {
      const url = String(u);
      if (url.includes('/api/upload/start')) return new Response(JSON.stringify({ id: 'test1', chunks: 1 }), { status: 200 });
      if (url.includes('/chunk/')) return new Response('{}', { status: 200 });
      if (url.includes('/finish')) return new Response(JSON.stringify({ path: '/u/photo.png', url: '/u/photo.png' }), { status: 200 });
      return orig(u);
    };
    const f: any = { name: 'photo.png', path: null, chunk: 0, totalChunks: 1, file,
                     error: null, inflight: false, cancelled: false, aborter: null };
    await w._runUpload(f, sink);
    w.fetch = orig;
    return { path: f.path, error: f.error, inflight: f.inflight, dropped: dropped.length };
  }, [FAKE_FILE]);

  expect(r.path, 'the upload must land its path even though the sink no longer holds it').toBe('/u/photo.png');
  expect(r.error).toBeNull();
  expect(r.inflight, 'inflight must be cleared on the way out').toBe(false);
});

test('an explicit cancel stops the transfer, and is not reported as a failure', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  const r = await page.evaluate(async ([mkFile]) => {
    const w = window as any;
    const file = eval(mkFile as string);
    const sink = { push: () => {}, has: () => true, drop: () => {}, render: () => {} };
    const orig = w.fetch;
    let chunks = 0;
    w.fetch = async (u: any) => {
      const url = String(u);
      if (url.includes('/api/upload/start')) return new Response(JSON.stringify({ id: 't', chunks: 3 }), { status: 200 });
      if (url.includes('/chunk/')) { chunks++; w._cancelUpload(f); return new Response('{}', { status: 200 }); }
      if (url.includes('/finish')) return new Response(JSON.stringify({ path: '/u/x', url: '/u/x' }), { status: 200 });
      return orig(u);
    };
    const f: any = { name: 'photo.png', path: null, chunk: 0, totalChunks: 3, file,
                     error: null, inflight: false, cancelled: false, aborter: null };
    await w._runUpload(f, sink);
    w.fetch = orig;
    return { path: f.path, error: f.error, chunks };
  }, [FAKE_FILE]);

  expect(r.chunks, 'the loop must stop at the cancel, not run all three chunks').toBe(1);
  expect(r.path, 'a cancelled upload never lands a path').toBeNull();
  expect(r.error, 'a deliberate cancel is not an error state').toBeNull();
});

test('a failed upload KEEPS its chip, with an error and a retryable file', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  const r = await page.evaluate(async ([mkFile]) => {
    const w = window as any;
    const file = eval(mkFile as string);
    const dropped: any[] = [];
    const sink = { push: () => {}, has: () => true, drop: (p: any) => dropped.push(p), render: () => {} };
    const orig = w.fetch; const origToast = w.showToast;
    w.showToast = () => {};
    let fail = true;
    w.fetch = async (u: any) => {
      const url = String(u);
      if (url.includes('/api/upload/start')) return new Response(JSON.stringify({ id: 't', chunks: 1 }), { status: 200 });
      if (url.includes('/chunk/')) {
        if (fail) return new Response(JSON.stringify({ error: 'unknown upload' }), { status: 404 });
        return new Response('{}', { status: 200 });
      }
      if (url.includes('/finish')) return new Response(JSON.stringify({ path: '/u/photo.png', url: '/u/photo.png' }), { status: 200 });
      return orig(u);
    };
    const f: any = { name: 'photo.png', path: null, chunk: 0, totalChunks: 1, file,
                     error: null, inflight: false, cancelled: false, aborter: null };
    await w._runUpload(f, sink);
    const afterFail = { error: f.error, dropped: dropped.length, hasFile: !!f.file };
    // RETRY IS REAL: the File was retained, so re-driving the same chip works.
    fail = false;
    await w._runUpload(f, sink);
    w.fetch = orig; w.showToast = origToast;
    return { afterFail, path: f.path, error: f.error };
  }, [FAKE_FILE]);

  expect(r.afterFail.dropped, 'the chip must NOT be spliced out behind a toast').toBe(0);
  expect(r.afterFail.error, 'the failure must be recorded on the chip').toBeTruthy();
  expect(r.afterFail.hasFile, 'the File is retained so Retry can be real').toBe(true);
  expect(r.path, 'retry re-drives the same chip to completion').toBe('/u/photo.png');
  expect(r.error).toBeNull();
});

test('the send guard names the file instead of saying "wait" about a dead transfer', async ({ page }) => {
  await page.goto('/');
  await page.waitForLoadState('networkidle');
  const r = await page.evaluate(async () => {
    const w = window as any;
    const said: string[] = [];
    const origToast = w.showToast;
    w.showToast = (m: string) => said.push(m);
    const done = { name: 'ok.png', path: '/u/ok.png' };
    const stuck = { name: 'stuck.png', path: null, inflight: false, error: 'chunk 0 failed' };
    const busy = { name: 'busy.png', path: null, inflight: true, error: null };

    const r1 = w._blockedByAttachment([done]);            // nothing pending
    const r2 = w._blockedByAttachment([done, stuck]);     // dead transfer
    const r3 = w._blockedByAttachment([done, busy]);      // genuinely uploading
    w.showToast = origToast;
    return { r1, r2, r3, said };
  });

  expect(r.r1, 'a fully-uploaded set must not block the send').toBe(false);
  expect(r.r2, 'a stranded chip still blocks — but it must say why').toBe(true);
  expect(r.r3).toBe(true);
  // The whole point: "Wait for upload to finish" is a LIE about a dead
  // transfer, and it is what kept the reporting user waiting.
  expect(r.said[0], 'the stranded message must name the file').toContain('stuck.png');
  expect(r.said[0]).not.toContain('Wait for upload to finish');
  expect(r.said[0], 'and say what to do about it').toMatch(/Retry|remove/);
  expect(r.said[1], 'a genuinely in-flight upload is described honestly').toContain('busy.png');
});
