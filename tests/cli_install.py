"""Exercise the real CLI publisher with temporary destinations and fault injection."""
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "scripts/install-cli.sh"
OLD = b"#!/usr/bin/env bash\nprintf 'old client\\n'\n"
NEW = b"#!/usr/bin/env bash\nprintf 'new client\\n'\n"


class InstallCLI(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="amux-cli-install-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin with spaces"
        self.bin.mkdir()
        self.dest = self.bin / "amux"
        self.dest.write_bytes(OLD)
        self.dest.chmod(0o755)
        self.source = self.root / "source"
        self.source.write_bytes(NEW)
        self.shims = self.root / "shims"
        self.shims.mkdir()
        self.env = {"PATH": f"{self.shims}:/usr/bin:/bin:/usr/local/bin",
                    "HOME": str(self.root), "AMUX_HOME": str(self.root / "data")}
        self.log = self.root / "data/logs/cli-install.log"

    def shim(self, name, body):
        path = self.shims / name
        path.write_text("#!/bin/bash\nset -euo pipefail\n" + body)
        path.chmod(0o755)

    def install(self, source=None):
        return subprocess.run(["/bin/bash", str(INSTALLER), str(self.bin),
                               str(source or self.source)], env=self.env,
                              text=True, capture_output=True, timeout=15)

    def refused(self, reason):
        inode = self.dest.stat().st_ino
        result = self.install()
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(reason, result.stderr)
        self.assertIn(reason, self.log.read_text())
        self.assertNotIn("cli_install_complete", self.log.read_text())
        self.assertEqual(self.dest.stat().st_ino, inode)
        self.assertEqual(self.dest.read_bytes(), OLD)
        self.assertEqual(list(self.bin.glob(".amux.install.*")), [])

    def test_valid_install_replaces_inode_and_keeps_open_reader(self):
        inode = self.dest.stat().st_ino
        with self.dest.open("rb", buffering=0) as reader:
            self.assertEqual(reader.read(10), OLD[:10])
            result = self.install()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(reader.read(), OLD[10:])
        self.assertNotEqual(self.dest.stat().st_ino, inode)
        self.assertEqual(self.dest.read_bytes(), NEW)
        self.assertEqual(self.dest.stat().st_mode & 0o777, 0o755)
        self.assertIn("cli_install_published checksum=", self.log.read_text())
        self.assertIn("cli_install_complete files=1", self.log.read_text())

    def test_syntax_error_preserves_installed_client(self):
        self.source.write_bytes(b"#!/usr/bin/env bash\nif true; then\n")
        self.refused("reason=bash_syntax")

    def test_quoted_conflict_markers_are_rejected_despite_valid_syntax(self):
        self.source.write_bytes(b"#!/usr/bin/env bash\ncat <<'EOF'\n<<<<<<< HEAD\na\n=======\nb\n>>>>>>> branch\nEOF\n")
        self.assertEqual(subprocess.run(["/bin/bash", "-n", self.source]).returncode, 0)
        self.refused("reason=conflict_markers")

    def test_empty_source_is_rejected(self):
        self.source.write_bytes(b"")
        self.refused("reason=missing_or_empty_source")

    def test_unmerged_index_is_rejected_even_after_syntax_clean_edit(self):
        repo = self.root / "repo"
        repo.mkdir()
        def git(*args, check=True):
            return subprocess.run(["git", "-C", str(repo), *args], check=check,
                                  capture_output=True, env={**self.env, "GIT_CONFIG_NOSYSTEM": "1"})
        git("init", "-b", "main")
        git("config", "user.name", "CLI fixture")
        git("config", "user.email", "fixture@example.invalid")
        self.source = repo / "amux"
        self.source.write_bytes(OLD)
        git("add", "amux")
        git("commit", "-m", "base")
        git("checkout", "-b", "peer")
        self.source.write_bytes(NEW)
        git("commit", "-am", "peer")
        git("checkout", "main")
        self.source.write_bytes(OLD + b"echo main\n")
        git("commit", "-am", "main")
        self.assertNotEqual(git("merge", "peer", check=False).returncode, 0)
        self.source.write_bytes(NEW)
        self.refused("reason=unmerged_source")

    def test_enospc_during_snapshot_preserves_installed_client(self):
        self.shim("cat", "printf '#!/usr/bin/env bash\\n'\necho 'No space left on device' >&2\nexit 1\n")
        self.refused("stage=snapshot")

    def test_failed_marker_probe_is_not_a_clean_verdict(self):
        self.shim("grep", "exit 2\n")
        self.refused("reason=marker_check_failed")

    def test_source_changes_during_validation_do_not_change_published_bytes(self):
        self.env["CLI_FIXTURE_SOURCE"] = str(self.source)
        self.shim("bash", '/bin/bash "$@"\nprintf "broken edit (\\n" > "$CLI_FIXTURE_SOURCE"\n')
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.source.read_bytes(), b"broken edit (\n")
        self.assertEqual(self.dest.read_bytes(), NEW)

    def test_failed_publication_preserves_installed_client(self):
        self.shim("python3", "echo 'injected rename failure' >&2\nexit 1\n")
        self.refused("stage=publication")

    def test_destination_becoming_directory_cannot_report_success(self):
        self.dest.unlink()
        self.env["CLI_FIXTURE_DEST"] = str(self.dest)
        self.shim("bash", '/bin/bash "$@"\nmkdir "$CLI_FIXTURE_DEST"\n')
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stage=publication", result.stderr)
        self.assertEqual(list(self.dest.iterdir()), [])
        self.assertEqual(list(self.bin.glob(".amux.install.*")), [])

    def test_replacing_symlink_never_changes_its_target(self):
        foreign = self.root / "peer draft"
        foreign.write_bytes(OLD)
        self.dest.unlink()
        self.dest.symlink_to(foreign)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.dest.is_symlink())
        self.assertEqual(self.dest.read_bytes(), NEW)
        self.assertEqual(foreign.read_bytes(), OLD)

    def test_concurrent_publications_are_complete_snapshots(self):
        candidates = [NEW + (f"# candidate {i}\n" * 10000).encode() for i in range(6)]
        allowed = [OLD, *candidates]
        stop = threading.Event()
        observations = []
        def observe():
            while not stop.is_set():
                observations.append(self.dest.read_bytes() in allowed)
        reader = threading.Thread(target=observe)
        reader.start()
        processes = []
        try:
            for i, content in enumerate(candidates):
                path = self.root / f"source{i}"
                path.write_bytes(content)
                processes.append(subprocess.Popen(["/bin/bash", str(INSTALLER), str(self.bin), str(path)],
                                                  env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE))
            for process in processes:
                _, err = process.communicate(timeout=15)
                self.assertEqual(process.returncode, 0, err)
        finally:
            stop.set()
            reader.join(timeout=5)
            for process in processes:
                if process.poll() is None:
                    process.kill()
                    process.wait()
        self.assertGreater(len(observations), 0)
        self.assertTrue(all(observations), "reader saw partial or mixed published bytes")

    def test_make_target_installs_real_cli_without_cargo(self):
        self.shim("cargo", "echo 'Cargo must not run' >&2\nexit 99\n")
        result = subprocess.run(["make", "-C", str(ROOT), "install-cli", f"BIN_DIR={self.bin}"],
                                env=self.env, text=True, capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.dest.read_bytes(), (ROOT / "amux").read_bytes())
        self.assertFalse(self.dest.is_symlink())
        helper = self.bin / "scripts/amux-grid.sh"
        self.assertEqual(helper.read_bytes(), (ROOT / "scripts/amux-grid.sh").read_bytes())
        for name in ("amux-remote", "amux-cloud"):
            self.assertEqual((self.bin / name).read_bytes(), (ROOT / name).read_bytes(), name)
            self.assertFalse((self.bin / name).is_symlink(), name)
        self.shim("curl", "exit 7\n")
        self.shim("tmux", "exit 1\n")
        result = subprocess.run([str(self.dest), "grid", "--help"], cwd=self.root,
                                env=self.env, text=True, capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("usage: amux grid", result.stdout)

    def test_invalid_packaged_helper_cannot_publish_the_main_cli(self):
        package = self.root / "package"
        (package / "scripts").mkdir(parents=True)
        self.source = package / "amux"
        self.source.write_bytes(NEW)
        (package / "scripts/amux-grid.sh").write_bytes(b"#!/usr/bin/env bash\nif true; then\n")
        self.refused("reason=bash_syntax")
        self.assertFalse((self.bin / "scripts/amux-grid.sh").exists())

    def test_freshness_recommends_guarded_install_instead_of_symlink(self):
        repo = self.root / "repo"
        (repo / ".claude").mkdir(parents=True)
        subprocess.run(["git", "init", "-q", str(repo)], check=True, env=self.env)
        (repo / "amux").write_bytes(NEW)
        hook = repo / ".claude/session-freshness.sh"
        hook.write_bytes((ROOT / ".claude/session-freshness.sh").read_bytes())
        installed = self.root / ".local/bin"
        installed.mkdir(parents=True)
        (installed / "amux").write_bytes(OLD)
        self.shim("curl", "exit 1\n")
        result = subprocess.run(["/bin/bash", str(hook)], env=self.env,
                                text=True, capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f'install-cli BIN_DIR="{installed}"', result.stdout)
        self.assertIn("after reviewing a resolved checkout", result.stdout)
        self.assertNotIn("ln -sfn", result.stdout)

    def test_general_installer_calls_the_same_guarded_publisher(self):
        # Execute the shipped installation stanza with the Rust install command
        # stubbed, without running Cargo, launchd or the rest of machine setup.
        text = (ROOT / "install.sh").read_text()
        stanza = text.split("# ── 3. Install binaries", 1)[1].split("case \":$PATH:\"", 1)[0]
        stanza = stanza[stanza.index("\n") + 1:]
        self.source.write_bytes(b"#!/usr/bin/env bash\nif true; then\n")
        repo = self.root / "installer-source"
        (repo / "scripts").mkdir(parents=True)
        publisher = repo / "scripts/install-cli.sh"
        publisher.write_bytes(INSTALLER.read_bytes())
        publisher.chmod(0o755)
        (repo / "amux").write_bytes(self.source.read_bytes())
        self.shim("install", "exit 0\n")
        # Stage 2 owns these Rust-only operands. This fixture starts at stage 3;
        # provide the private stage and stub only Rust artifact verification,
        # while executing the real Bash publisher and its syntax refusal.
        rust_stage = self.root / "private-rust-stage"
        rust_stage.mkdir()
        (repo / "scripts/install-artifact-manifest.py").write_text("raise SystemExit(0)\n")
        env = {**self.env, "BIN_DIR": str(self.bin), "SCRIPT_DIR": str(repo),
               "TARGET_DIR": str(self.root / "unused"), "INSTALL_ARTIFACT_DIR": str(rust_stage)}
        result = subprocess.run(["/bin/bash", "-ec", "say() { :; };\n" + stanza],
                                env=env, text=True, capture_output=True, timeout=15)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("reason=bash_syntax", result.stderr)
        self.assertEqual(self.dest.read_bytes(), OLD)


if __name__ == "__main__":
    unittest.main(verbosity=2)
