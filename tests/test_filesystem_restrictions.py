"""Unit tests for filesystem path restrictions.

These tests replicate the _is_path_allowed logic from amux-server.py in
isolation because importing the full server triggers initialization side
effects (mkcert, tmux detection, directory creation).
"""

from pathlib import Path


# ── Replicated filesystem restriction logic ──────────────────────────────────

_SENSITIVE_PATHS = {
    ".ssh", ".gnupg", ".aws", ".kube", ".netrc", ".npmrc",
    ".docker", ".config/gcloud", ".config/gh",
}

_BLOCKED_SYSTEM_PATHS = frozenset({
    "/etc/shadow", "/etc/sudoers", "/etc/master.passwd",
    "/private/etc/shadow", "/private/etc/sudoers",
    "/var/db/sudo", "/private/var/db/sudo",
})

_BLOCKED_SYSTEM_PREFIXES = (
    "/etc/ssh/", "/private/etc/ssh/",
    "/var/run/secrets/", "/run/secrets/",
)


def _is_path_allowed(p: Path) -> bool:
    """Standalone replica of _is_path_allowed from amux-server.py."""
    try:
        resolved = p.resolve()
    except (OSError, ValueError):
        return False
    resolved_str = str(resolved)
    if resolved_str in _BLOCKED_SYSTEM_PATHS:
        return False
    if any(resolved_str.startswith(pfx) for pfx in _BLOCKED_SYSTEM_PREFIXES):
        return False
    home = Path.home().resolve()
    try:
        rel = resolved.relative_to(home)
        parts = rel.parts
        for sensitive in _SENSITIVE_PATHS:
            sens_parts = Path(sensitive).parts
            if parts[:len(sens_parts)] == sens_parts:
                return False
    except ValueError:
        pass  # outside home — allow (e.g. /tmp, project dirs)
    return True


# ── Tests: sensitive home directories ────────────────────────────────────────

def test_ssh_keys_blocked():
    assert _is_path_allowed(Path("~/.ssh/id_rsa").expanduser()) is False
    assert _is_path_allowed(Path("~/.ssh/id_ed25519").expanduser()) is False
    assert _is_path_allowed(Path("~/.ssh/known_hosts").expanduser()) is False
    assert _is_path_allowed(Path("~/.ssh").expanduser()) is False


def test_gnupg_blocked():
    assert _is_path_allowed(Path("~/.gnupg/private-keys-v1.d").expanduser()) is False
    assert _is_path_allowed(Path("~/.gnupg/trustdb.gpg").expanduser()) is False


def test_cloud_credentials_blocked():
    assert _is_path_allowed(Path("~/.aws/credentials").expanduser()) is False
    assert _is_path_allowed(Path("~/.aws/config").expanduser()) is False
    assert _is_path_allowed(Path("~/.kube/config").expanduser()) is False
    assert _is_path_allowed(Path("~/.config/gcloud/creds.json").expanduser()) is False
    assert _is_path_allowed(Path("~/.config/gh/hosts.yml").expanduser()) is False


def test_auth_files_blocked():
    assert _is_path_allowed(Path("~/.netrc").expanduser()) is False
    assert _is_path_allowed(Path("~/.npmrc").expanduser()) is False
    assert _is_path_allowed(Path("~/.docker/config.json").expanduser()) is False


# ── Tests: system paths ──────────────────────────────────────────────────────

def test_etc_shadow_blocked():
    assert _is_path_allowed(Path("/etc/shadow")) is False
    assert _is_path_allowed(Path("/etc/sudoers")) is False


def test_etc_ssh_dir_blocked():
    assert _is_path_allowed(Path("/etc/ssh/sshd_config")) is False
    assert _is_path_allowed(Path("/etc/ssh/ssh_host_rsa_key")) is False


def test_run_secrets_blocked():
    assert _is_path_allowed(Path("/run/secrets/db_password")) is False


def test_macos_private_variants_blocked():
    """macOS /etc resolves to /private/etc — both forms must be blocked."""
    assert _is_path_allowed(Path("/private/etc/shadow")) is False
    assert _is_path_allowed(Path("/private/etc/sudoers")) is False
    assert _is_path_allowed(Path("/private/etc/ssh/sshd_config")) is False


# ── Tests: allowed paths ─────────────────────────────────────────────────────

def test_project_files_allowed():
    assert _is_path_allowed(Path("~/repo/amux/amux-server.py").expanduser()) is True
    assert _is_path_allowed(Path("~/Documents/readme.md").expanduser()) is True


def test_downloads_allowed():
    assert _is_path_allowed(Path("~/Downloads/file.zip").expanduser()) is True


def test_tmp_allowed():
    assert _is_path_allowed(Path("/tmp/test.txt")) is True


def test_amux_data_dir_allowed():
    assert _is_path_allowed(Path("~/.amux/board.json").expanduser()) is True
    assert _is_path_allowed(Path("~/.amux/sessions").expanduser()) is True


def test_other_config_dirs_allowed():
    """Only specific .config subdirs are blocked, not all of .config."""
    assert _is_path_allowed(Path("~/.config/alacritty/alacritty.toml").expanduser()) is True
    assert _is_path_allowed(Path("~/.config/wezterm/wezterm.lua").expanduser()) is True


def test_etc_hosts_allowed():
    """/etc/hosts is not in the blocklist — only security-critical files."""
    assert _is_path_allowed(Path("/etc/hosts")) is True


# ── Tests: traversal and edge cases ──────────────────────────────────────────

def test_traversal_to_ssh_blocked():
    assert _is_path_allowed(Path("~/.config/gh/../../.ssh/id_rsa").expanduser()) is False


def test_traversal_from_safe_to_sensitive():
    """Traversal from safe dir back into user's own .ssh is blocked."""
    assert _is_path_allowed(Path("~/Documents/../.ssh/id_rsa").expanduser()) is False


def test_deeply_nested_sensitive_path_blocked():
    assert _is_path_allowed(Path("~/.ssh/keys/backup/old_key").expanduser()) is False
    assert _is_path_allowed(Path("~/.aws/sso/cache/token.json").expanduser()) is False


if __name__ == "__main__":
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    for t in tests:
        t()
        print(f"  PASS: {t.__name__}")
    print(f"\nAll {len(tests)} filesystem restriction tests passed!")
