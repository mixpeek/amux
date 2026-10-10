// Exercise the shipped executable, including the macOS /tmp symlink boundary.
import assert from 'node:assert/strict';
import {mkdtempSync, mkdirSync, copyFileSync, symlinkSync, rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {pathToFileURL} from 'node:url';
import {spawnSync} from 'node:child_process';

const home = mkdtempSync(join(tmpdir(), 'amux-driver-entry-'));
try {
  const real = join(home, 'real');
  mkdirSync(real);
  const driver = join(real, 'driver.mjs');
  copyFileSync(new URL('./browser-route-driver.mjs', import.meta.url), driver);
  const alias = join(home, 'alias.mjs');
  const aliasDir = join(home, 'alias-dir');
  symlinkSync(driver, alias, 'file');
  symlinkSync(real, aliasDir, 'dir');
  for (const entry of [driver, alias, join(aliasDir, 'driver.mjs')]) {
    const result = spawnSync(process.execPath, [entry], {
      input: JSON.stringify({context: {}, verb: 'state'}), encoding: 'utf8', timeout: 10000,
    });
    assert.equal(result.status, 0, result.stderr || result.error?.message);
    assert.deepEqual(JSON.parse(result.stdout), {error: 'route requires an explicit session', status: 400});
  }
  // Importing from another entrypoint must not read stdin or emit a response.
  const importer = `await import(${JSON.stringify(pathToFileURL(alias).href)}); process.stdout.write('imported');`;
  const imported = spawnSync(process.execPath, ['--input-type=module', '-e', importer], {
    input: '{invalid input', encoding: 'utf8', timeout: 10000,
  });
  assert.equal(imported.status, 0, imported.stderr || imported.error?.message);
  assert.equal(imported.stdout, 'imported');
  console.log('PASS browser driver entrypoint: canonical, file alias, directory alias and import-only boundaries');
} finally {
  rmSync(home, {recursive: true, force: true});
}
