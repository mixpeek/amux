#!/usr/bin/env python3
"""Exercise the shipped build script in a tiny real Cargo worktree, sharing the bounded target."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]


class CargoWorktreeProvenance(unittest.TestCase):
    def test_detached_worktree_reuses_build_and_tracks_commits(self):
        with tempfile.TemporaryDirectory(prefix='amux-provenance-test-') as temp:
            root = Path(temp)
            repo = root / 'repo'
            env = dict(os.environ, GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull,
                       GIT_AUTHOR_NAME='Fixture', GIT_AUTHOR_EMAIL='fixture@example.test',
                       GIT_COMMITTER_NAME='Fixture', GIT_COMMITTER_EMAIL='fixture@example.test',
                       CARGO_BUILD_JOBS='2')

            def run(args, cwd=repo):
                return subprocess.run(args, cwd=cwd, env=env, text=True,
                                      capture_output=True, check=True, timeout=120).stdout

            subprocess.run(['git', 'init', '-q', '-b', 'main', str(repo)], env=env, check=True)
            crate = repo / 'crates/probe'
            (crate / 'src').mkdir(parents=True)
            (repo / 'Cargo.toml').write_text('[workspace]\nmembers=["crates/probe"]\nresolver="2"\n')
            (repo / '.gitignore').write_text('Cargo.lock\n')
            (crate / 'Cargo.toml').write_text('[package]\nname="amux-provenance-probe"\nversion="0.0.0"\nedition="2021"\n')
            (crate / 'src/lib.rs').write_text('pub const COMMIT: &str = env!("AMUX_BUILD_COMMIT_FULL");\n')
            shutil.copyfile(REPO / 'crates/amux-server/build.rs', crate / 'build.rs')
            run(['git', 'add', '.']); run(['git', 'commit', '-qm', 'base'])
            worktree = root / 'worktree'
            run(['git', 'worktree', 'add', '-q', '--detach', str(worktree), 'HEAD'])

            def build():
                output = run(['bash', str(REPO / 'scripts/safe-cargo.sh'), 'check',
                              '-p', 'amux-provenance-probe', '--message-format=json'], worktree)
                events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
                artifacts = [e for e in events if e.get('reason') == 'compiler-artifact'
                             and e['target']['kind'] == ['lib']]
                self.assertEqual(len(artifacts), 1, output)
                scripts = [e for e in events if e.get('reason') == 'build-script-executed']
                self.assertEqual(len(scripts), 1, output)
                return artifacts[0]['fresh'], dict(scripts[0]['env'])['AMUX_BUILD_COMMIT_FULL']

            first, sha = build()
            self.assertFalse(first)
            self.assertEqual(sha, run(['git', 'rev-parse', 'HEAD'], worktree).strip())
            fresh, repeat_sha = build()
            self.assertTrue(fresh, 'unchanged detached worktree rebuilt because of a missing Git watch path')
            self.assertEqual(repeat_sha, sha)
            # HEAD changes without touching crate bytes must still update identity.
            run(['git', 'commit', '--allow-empty', '-qm', 'new identity'], worktree)
            fresh, changed_sha = build()
            self.assertFalse(fresh)
            self.assertNotEqual(changed_sha, sha)
            self.assertEqual(changed_sha, run(['git', 'rev-parse', 'HEAD'], worktree).strip())
            self.assertTrue(build()[0], 'unchanged new commit rebuilt again')
            # Packed branches have no loose ref file: tracking a nonexistent one
            # would recreate the same rebuild loop outside detached worktrees.
            run(['git', 'switch', '-qc', 'feature/provenance'], worktree)
            run(['git', 'pack-refs', '--all', '--prune'], worktree)
            build()
            self.assertTrue(build()[0], 'packed branch rebuilt without changes')
            run(['git', 'commit', '--allow-empty', '-qm', 'packed branch advanced'], worktree)
            fresh, branch_sha = build()
            self.assertFalse(fresh)
            self.assertEqual(branch_sha, run(['git', 'rev-parse', 'HEAD'], worktree).strip())
            self.assertTrue(build()[0])


if __name__ == '__main__':
    unittest.main()
