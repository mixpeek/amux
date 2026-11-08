// Every binary opens to a preview, not a box with a Download button (owner,
// 2026-11-08: "make sure we have native previews of every file type", over a
// TIFF upload). A TIFF is converted to an image, an archive lists its entries,
// a database shows its tables, a font shows a specimen, and anything else
// shows its first bytes in hex. Download stays on each.
import { test, expect, Page } from './fixtures';
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { scratchpadDir } from './scratchpad-dir';
import { cleanup } from './teardown';

/** A 2x2 RGB baseline TIFF, uncompressed. */
function tiff(): Buffer {
  const b = Buffer.alloc(152);
  b.write('II*\0', 0, 'latin1');
  b.writeUInt32LE(8, 4);
  const entries: [number, number, number, number][] = [
    [256, 3, 1, 2], [257, 3, 1, 2], [258, 3, 3, 134], [259, 3, 1, 1], [262, 3, 1, 2],
    [273, 4, 1, 140], [277, 3, 1, 3], [278, 3, 1, 2], [279, 4, 1, 12], [284, 3, 1, 1],
  ];
  b.writeUInt16LE(entries.length, 8);
  entries.forEach(([tag, type, count, value], i) => {
    const o = 10 + i * 12;
    b.writeUInt16LE(tag, o); b.writeUInt16LE(type, o + 2); b.writeUInt32LE(count, o + 4);
    if (type === 3 && count === 1) b.writeUInt16LE(value, o + 8); else b.writeUInt32LE(value, o + 8);
  });
  b.writeUInt32LE(0, 130);
  [8, 8, 8].forEach((v, i) => b.writeUInt16LE(v, 134 + i * 2));
  Buffer.from([255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]).copy(b, 140);
  return b;
}

function has(bin: string): boolean {
  try { execFileSync('sh', ['-c', `command -v ${bin}`], { stdio: 'ignore' }); return true; } catch { return false; }
}

async function open(page: Page, file: string) {
  await page.evaluate((f) => (window as any).openFilePreview(f), file);
  await expect(page.locator('#file-body .binprev-head a', { hasText: 'Download' })).toBeVisible();
}

test('binaries open to a preview: image, archive, database, font, and hex for the rest', async ({ page, request }) => {
  // Six files, two of them converted by a tool on first open.
  test.setTimeout(120_000);
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openFilePreview === 'function');
  const auth = { Authorization: `Bearer ${await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN)}` };
  const dir = path.join(await scratchpadDir(request, auth), `binprev-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`);
  fs.mkdirSync(dir, { recursive: true });
  try {
    // 1. TIFF: drawn as an image wherever a converter is installed; the bytes otherwise.
    const tif = path.join(dir, 'photo.tiff');
    fs.writeFileSync(tif, tiff());
    await open(page, tif);
    await expect(page.locator('#file-body .binprev-meta')).toContainText('TIFF image');
    const canConvert = (process.platform === 'darwin' && has('sips')) || has('magick') || has('convert') || has('ffmpeg');
    if (canConvert) {
      const img = page.locator('#file-body .binprev-main img');
      await expect(img).toBeVisible({ timeout: 30000 });
      await expect.poll(() => img.evaluate((i: HTMLImageElement) => i.naturalWidth)).toBeGreaterThan(0);
    } else {
      await expect(page.locator('#file-body .binprev-main')).toContainText('No image preview');
      await expect(page.locator('#file-body .binprev-hex')).toContainText('49 49 2a 00');
    }

    // 2. A zip and a tar list what is inside them.
    const zip = path.join(dir, 'bundle.zip');
    execFileSync('python3', ['-c', 'import sys,zipfile\nz=zipfile.ZipFile(sys.argv[1],"w")\nz.writestr("docs/read me.txt","hello")\nz.writestr("a.bin",b"\\0"*10)\nz.close()', zip]);
    await open(page, zip);
    await expect(page.locator('#file-body .binprev-main table')).toContainText('docs/read me.txt');
    await expect(page.locator('#file-body .binprev-main')).toContainText('2 entries');
    const tar = path.join(dir, 'bundle.tar');
    fs.writeFileSync(path.join(dir, 'inside.txt'), 'x'.repeat(100));
    execFileSync('tar', ['-cf', tar, '-C', dir, 'inside.txt']);
    await open(page, tar);
    await expect(page.locator('#file-body .binprev-main table')).toContainText('inside.txt');

    // 3. A SQLite database: its tables and first rows, read-only.
    const db = path.join(dir, 'app.sqlite');
    execFileSync('python3', ['-c', 'import sys,sqlite3\nc=sqlite3.connect(sys.argv[1])\nc.execute("create table people(name text, n int)")\nc.execute("insert into people values (\'ada\', 7)")\nc.commit()', db]);
    await open(page, db);
    await expect(page.locator('#file-body .binprev-table summary')).toContainText('people');
    await expect(page.locator('#file-body .binprev-table summary')).toContainText('1 row');
    await expect(page.locator('#file-body .binprev-table td').first()).toHaveText('ada');

    // 4. A font: a specimen in that font.
    const font = path.join(dir, 'face.woff2');
    fs.copyFileSync(path.join(__dirname, '..', 'crates', 'amux-dashboard', 'static', 'vendor', 'fonts', 'carlito-400.woff2'), font);
    await open(page, font);
    await expect(page.locator('#file-body .binprev-font')).toContainText('The quick brown fox');
    expect(await page.locator('#file-body .binprev-font').evaluate((e) => getComputedStyle(e).fontFamily)).toContain('amux-prev-');

    // 5. Anything else of a known kind: its kind and first bytes.
    const bin = path.join(dir, 'blob.bin');
    fs.writeFileSync(bin, Buffer.concat([Buffer.from('\x7fELF'), Buffer.alloc(40, 0), Buffer.from('tail')]));
    await open(page, bin);
    await expect(page.locator('#file-body .binprev-meta')).toContainText('ELF executable');
    await expect(page.locator('#file-body .binprev-hex')).toContainText('00000000  7f 45 4c 46');
  } finally {
    await cleanup('remove the preview fixtures', async () => fs.rmSync(dir, { recursive: true, force: true }));
  }
});
