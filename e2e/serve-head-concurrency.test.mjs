// Two e2e servers for two different commits, started at once, each serve their
// own commit. serve-head.sh builds every one in one shared worktree; when its
// lock covered only the checkout, a second run re-pointed the worktree while the
// first was building and the first served the second's binary (2026-10-31: two
// land gates, the new specs of each red, all green alone). The fake cargo below
// reads the source only after a pause, which is that window.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, execFileSync } from 'node:child_process';

// A git hook (pre-push runs this suite) exports GIT_DIR, GIT_INDEX_FILE and
// friends. Inherited, they point every git below at the checkout being pushed:
// the first version of this test rewrote that checkout's index and set its
// user.name to "t". No child here sees a GIT_* variable.
const CLEAN = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('GIT_')));

const SCRIPT = process.env.SERVE_HEAD_UNDER_TEST
  || new URL('./serve-head.sh', import.meta.url).pathname;

function repoAt(root) {
  const git = (...a) => execFileSync('git', a, { cwd: root, stdio: 'pipe', env: CLEAN }).toString().trim();
  fs.mkdirSync(root, { recursive: true });
  git('init', '-q');
  git('config', 'user.email', 't@t'); git('config', 'user.name', 't');
  fs.mkdirSync(path.join(root, 'e2e'));
  fs.copyFileSync(SCRIPT, path.join(root, 'e2e', 'serve-head.sh'));
  fs.writeFileSync(path.join(root, 'Cargo.toml'), '[workspace]\n');
  fs.writeFileSync(path.join(root, 'marker'), 'A\n');
  git('add', '-A'); git('commit', '-qm', 'A');
  const a = git('rev-parse', 'HEAD');
  fs.writeFileSync(path.join(root, 'marker'), 'B\n');
  git('commit', '-qam', 'B');
  const b = git('rev-parse', 'HEAD');
  return { git, a, b };
}

function serve(repo, env) {
  return new Promise(resolve => {
    const p = spawn('bash', [path.join(repo, 'e2e', 'serve-head.sh')], { env, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    p.stdout.on('data', d => { out += d; });
    p.stderr.on('data', d => { out += d; });
    p.on('close', code => resolve({ code, out }));
  });
}

test('two runs for different commits each serve their own commit, and a repeat reuses the build', async () => {
  const t = fs.mkdtempSync(path.join(os.tmpdir(), 'serve-head-'));
  const fake = path.join(t, 'fakebin');
  fs.mkdirSync(fake);
  const builds = path.join(t, 'builds.log');
  fs.writeFileSync(path.join(fake, 'cargo'), `#!/bin/bash
[ "$1" = build ] || { echo "fake cargo: unexpected $*" >&2; exit 3; }
sleep 3
m=$(cat marker)
echo "$m" >> ${JSON.stringify(builds)}
mkdir -p "$CARGO_TARGET_DIR/debug"
printf '#!/bin/sh\\necho SERVED %s\\n' "$m" > "$CARGO_TARGET_DIR/debug/amux-server"
chmod +x "$CARGO_TARGET_DIR/debug/amux-server"
`, { mode: 0o755 });
  const src = path.join(t, 'src');
  const { git, a, b } = repoAt(src);
  const r1 = path.join(t, 'r1'), r2 = path.join(t, 'r2');
  git('worktree', 'add', '-q', '--detach', r1, a);
  git('worktree', 'add', '-q', '--detach', r2, b);
  const env = { PATH: `${fake}:${CLEAN.PATH}`, HOME: t, AMUX_HOME: path.join(t, 'home'),
    AMUX_E2E_WORKTREE_DIR: path.join(t, 'wt'), AMUX_E2E_HEAD_BIN_DIR: path.join(t, 'bin'),
    AMUX_E2E_HEAD_TARGET_DIR: path.join(t, 'target') };
  fs.mkdirSync(env.AMUX_HOME);
  try {
    const first = serve(r1, env);
    await new Promise(r => setTimeout(r, 1000));
    const second = serve(r2, env);
    const [x, y] = await Promise.all([first, second]);
    assert.match(x.out, /SERVED A/, `the run at A served A:\n${x.out}`);
    assert.match(y.out, /SERVED B/, `the run at B served B:\n${y.out}`);
    const again = await serve(r1, env);
    assert.match(again.out, /SERVED A/, again.out);
    assert.deepEqual(fs.readFileSync(builds, 'utf8').trim().split('\n').sort(), ['A', 'B'],
      'one build per commit; the repeat used the copy');
  } finally {
    fs.rmSync(t, { recursive: true, force: true });
  }
});
