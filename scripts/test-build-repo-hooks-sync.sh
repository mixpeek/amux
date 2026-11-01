#!/usr/bin/env bash
# AF-1129: the repo's installed git hooks follow the activated commit, so a
# landed hook change reaches real pushes without a manual install.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BUILDER="$ROOT/scripts/rust-auto-build.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
REPO="$TMP/repo"
FAKE_HOME="$TMP/home"
INSTALL="$TMP/bin/amux-server-rs"
fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }

git init -q -b main "$REPO"
mkdir -p "$REPO/crates" "$REPO/scripts/git-hooks" "$FAKE_HOME/.cargo/bin" "$(dirname "$INSTALL")"
printf '[workspace]\n' > "$REPO/Cargo.toml"
printf 'fixture\n' > "$REPO/crates/input.rs"
printf '#!/usr/bin/env python3\nprint("new push hook")\n' > "$REPO/scripts/git-hooks/pre-push"
printf '#!/bin/sh\necho new commit hook\n' > "$REPO/scripts/git-hooks/post-commit"
printf '#!/bin/sh\necho never installed\n' > "$REPO/scripts/git-hooks/pre-commit"
cat > "$REPO/scripts/safe-cargo.sh" <<'CARGO'
#!/bin/sh
out=release; prev=
for a in "$@"; do [ "$prev" = --profile ] && out="$a"; prev="$a"; done
mkdir -p "$CARGO_TARGET_DIR/$out"
printf '%s\n' '#!/bin/sh' 'printf "%s\\n" new-build' > "$CARGO_TARGET_DIR/$out/amux-server"
chmod 0755 "$CARGO_TARGET_DIR/$out/amux-server"
CARGO
chmod +x "$REPO/scripts/safe-cargo.sh"
commit() { (cd "$REPO" && git add -A && git -c user.name=t -c user.email=t@t commit -qm "$1"); }
commit fixture
printf '#!/bin/sh\nprintf old-build\\n\n' > "$INSTALL"; chmod 0755 "$INSTALL"
HOOKS="$REPO/.git/hooks"
rm -f "${HOOKS:?}"/*.sample
printf '#!/usr/bin/env python3\nprint("old push hook")\n' > "$HOOKS/pre-push"
printf '#!/bin/sh\necho old commit hook\n' > "$HOOKS/post-commit"

run() {
  env -u AMUX_URL HOME="$FAKE_HOME" AMUX_REPO="$REPO" AMUX_RS_INSTALL="$INSTALL" AMUX_RS_BUILD_STAMP="$TMP/stamp" \
  AMUX_RS_BUILD_LOG="$TMP/build.log" AMUX_RS_BUILD_LOCK="$TMP/build.lock" AMUX_RS_BUILD_PROVENANCE="$TMP/provenance.json" \
  AMUX_RS_ACTIVATION_REF=HEAD AMUX_BUILD_MIN_FREE_GB=0 AMUX_BUILD_DEBUG_MAX_GB=999999 AMUX_RS_SHELL_DEFER_MAX_S=0 \
  AMUX_ACTIVATION_ROLLBACK=0 AMUX_RS_RUNS_URL=http://127.0.0.1:9/none bash "$BUILDER" >/dev/null 2>&1 || true
}

# 1. Installed hooks take the activated commit's bytes.
run
grep -q 'new push hook' "$HOOKS/pre-push" && ok "pre-push follows the activated commit" || bad "pre-push stayed old"
grep -q 'new commit hook' "$HOOKS/post-commit" && ok "post-commit follows it too" || bad "post-commit stayed old"
[ -x "$HOOKS/pre-push" ] && ok "the hook stays executable" || bad "the hook lost its mode"
grep -q 'hook=pre-push verdict=repo_hook_synced' "$TMP/build.log" && ok "the sync is logged" || bad "no sync verdict"
# 2. A hook this repo never installed is not created.
[ ! -e "$HOOKS/pre-commit" ] && ok "an uninstalled hook is left uninstalled" || bad "a hook appeared"
# 3. Committed bytes, not the working tree.
printf '#!/usr/bin/env python3\nprint("uncommitted draft")\n' > "$REPO/scripts/git-hooks/pre-push"
run
grep -q 'uncommitted draft' "$HOOKS/pre-push" && bad "a working-tree draft was installed" || ok "a working-tree draft is not installed"
git -C "$REPO" checkout -q -- scripts/git-hooks/pre-push
# 4. No change, no line.
: > "$TMP/build.log"
run
grep -q 'repo_hook_synced' "$TMP/build.log" && bad "an unchanged hook was rewritten" || ok "an unchanged hook is left alone"
# 5. A commit whose hook does not parse leaves the working one in place.
printf '#!/usr/bin/env python3\ndef broken(:\n' > "$REPO/scripts/git-hooks/pre-push"
commit "breaks the hook"
run
grep -q 'new push hook' "$HOOKS/pre-push" && ok "a hook that does not parse is not installed" || bad "a broken hook was installed"
grep -q 'verdict=repo_hook_source_invalid' "$TMP/build.log" && ok "and that is logged" || bad "no invalid verdict"
ls "$HOOKS"/.repo-hook.* >/dev/null 2>&1 && bad "a temp file was left behind" || ok "no temp file is left behind"
# 6. A tracked hooks dir belongs to the repo: hands off.
printf '#!/usr/bin/env python3\nprint("fixed again")\n' > "$REPO/scripts/git-hooks/pre-push"
commit "fixes the hook"
git -C "$REPO" config core.hooksPath "$REPO/scripts/git-hooks"
before=$(cat "$HOOKS/pre-push")
run
[ "$(cat "$HOOKS/pre-push")" = "$before" ] && ok "core.hooksPath set: nothing is touched" || bad "synced past core.hooksPath"

echo "test-build-repo-hooks-sync: $fails failed"
exit "$fails"
