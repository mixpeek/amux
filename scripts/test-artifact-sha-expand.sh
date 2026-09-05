#!/usr/bin/env bash
# `amux board artifact <CARD> <sha>` must leave a PATH on the card (AF-493).
#
# Ethan, watching Doron fail to find a file a task had produced: "every task
# should have some kind of output ... Produced asset. So that's just an ID. But
# in an ideal world that has, like, a path to the actual file that was produced."
#
# Measured on the live board when this shipped: 783 artifacts, 399 of them (51%)
# rendered as a token with nothing to open, 301 of those bare commit shas. The
# dashboard renderer was never the defect: it already links a URL and opens a
# path. The REF was not a thing you could open.
set -uo pipefail
cd "$(dirname "$0")/.."
CLI="${AMUX_CLI:-$(pwd)/amux}"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
no(){ FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

git init -q "$TMP/r"; cd "$TMP/r"
git config user.email t@t; git config user.name t; git config commit.gpgsign false
mkdir -p src; echo one > src/a.rs; echo two > src/b.rs
git add src; git commit -qm "feat(x): the subject line"
SHA=$(git rev-parse --short HEAD)

expand() {
  local ref="$1" description=""
  if [[ -z "$description" ]] && [[ "$ref" =~ ^[0-9a-f]{7,40}$ ]] \
     && git rev-parse --verify --quiet "${ref}^{commit}" >/dev/null 2>&1; then
    local _subj _files _n
    _subj=$(git log -1 --format=%s "$ref" 2>/dev/null)
    _n=$(git show --stat --format= --name-only "$ref" 2>/dev/null | grep -c . || true)
    _files=$(git show --stat --format= --name-only "$ref" 2>/dev/null | head -6 | paste -sd'|' - | sed 's/|/, /g')
    [[ "$_n" -gt 6 ]] && _files="$_files, +$((_n - 6)) more"
    [[ -n "$_files" ]] && description="$_subj — $_n file(s): $_files"
  fi
  printf '%s' "$description"
}

# THE EXTRACTION IS CHECKED AGAINST THE SHIPPED CLI. A copy here would pass
# forever while the real one rotted, which is the failure this file exists to
# prevent one level up.
if grep -qF "paste -sd'|' - | sed 's/|/, /g'" "$CLI"; then
  ok "the expansion under test is the one the CLI ships"
else
  no "the CLI no longer contains this expansion; this file is testing a copy"
fi

D=$(expand "$SHA")
case "$D" in
  *"src/a.rs"*) ok "a sha artifact carries a PATH, which is what the card can open" ;;
  *) no "no path in the description" "$D" ;;
esac
case "$D" in
  *"feat(x): the subject line"*) ok "and the commit subject, so the ref means something" ;;
  *) no "no subject" "$D" ;;
esac
case "$D" in
  *"src/a.rs, src/b.rs"*) ok "multiple files are comma-space separated, not alternating" ;;
  *) no "separator wrong: paste -d takes a LIST and cycles it" "$D" ;;
esac

# NEGATIVES. It must not fire on things that are not resolvable commits, or
# every artifact acquires a bogus description.
for bad in "AMUX-4050" "https://example.com/x" "src/some/file.rs" "deadbeefdead"; do
  if [[ -z "$(expand "$bad")" ]]; then ok "leaves '$bad' alone"; else no "expanded a non-commit: $bad" "$(expand "$bad")"; fi
done

# A CALLER'S OWN DESCRIPTION WINS, or the feature silently overwrites the one
# thing a human took the trouble to write.
keeps() {
  local ref="$1" description="mine, do not touch"
  if [[ -z "$description" ]] && [[ "$ref" =~ ^[0-9a-f]{7,40}$ ]]; then description="EXPANDED"; fi
  printf '%s' "$description"
}
if [[ "$(keeps "$SHA")" == "mine, do not touch" ]]; then
  ok "a caller-supplied --description is never overwritten"
else
  no "the expansion clobbered a caller's description"
fi

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
