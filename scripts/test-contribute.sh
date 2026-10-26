#!/bin/bash
# `amux contribute` sends local amux work upstream as a pull request, and
# --then-pull replaces the local work with origin/main only after that PR is
# open with the exact snapshot as its head. GitHub is a fake `gh` and local bare
# repositories (https://github.com/ is rewritten to them), so nothing leaves
# the machine.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
AMUX="$HERE/amux"
T=$(mktemp -d); trap 'rm -rf -- "${T:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
g() { git -c user.email=t@t -c user.name=t -c init.defaultBranch=main "$@"; }

mkdir -p "$T/remotes/mixpeek" "$T/remotes/tester" "$T/bin" "$T/home"
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0="url.$T/remotes/.insteadOf" GIT_CONFIG_VALUE_0="https://github.com/"
g init -q --bare "$T/remotes/mixpeek/amux.git"
g clone -q "https://github.com/mixpeek/amux.git" "$T/seed" 2>/dev/null
(cd "$T/seed" && g checkout -q -b main && echo one > f && g add f && g commit -qm one && g push -q origin main)
g clone -q --bare "$T/remotes/mixpeek/amux.git" "$T/remotes/tester/amux.git"

# The fake gh. FAKE_GH_AUTHED / FAKE_GH_PUSH / FAKE_GH_PR_SHA steer it.
cat > "$T/bin/gh" <<'GH'
#!/bin/bash
R="$FAKE_GH_ROOT"
case "$1 $2" in
  "auth status") [ "${FAKE_GH_AUTHED:-1}" = 1 ] ;;
  "auth git-credential") exit 0 ;;
  "api user") case "$*" in *noreply*) echo "1+tester@users.noreply.github.com" ;; *) echo tester ;; esac ;;
  "api repos/mixpeek/amux") echo "${FAKE_GH_PUSH:-true}" ;;
  "repo fork") exit 0 ;;
  "repo list") echo tester/amux ;;
  "pr create")
    printf '%s\n' "$@" > "$R/pr-create.args"
    echo "https://github.com/mixpeek/amux/pull/7" ;;
  "pr view")
    head=$(grep -A1 -x -- --head "$R/pr-create.args" | tail -1)
    case "$head" in *:*) owner=${head%%:*}; br=${head#*:} ;; *) owner=mixpeek; br=$head ;; esac
    sha=$(git ls-remote "$R/remotes/$owner/amux.git" "refs/heads/$br" | cut -f1)
    echo "OPEN ${FAKE_GH_PR_SHA:-$sha}" ;;
  *) echo "fake gh: unhandled $*" >&2; exit 2 ;;
esac
GH
chmod +x "$T/bin/gh"
# Every run in one second, as on a fast CI host: branch names must still differ.
cat > "$T/bin/date" <<'D'
#!/bin/bash
case "$*" in *%H%M%S*) echo 20261025-170351 ;; *) exec /bin/date "$@" ;; esac
D
chmod +x "$T/bin/date"
export FAKE_GH_ROOT="$T" PATH="$T/bin:$PATH"
run() { HOME="$T/home" AMUX_HOME="$T/home/.amux" "$AMUX" contribute "$@" 2>&1; }
fresh() { # a checkout behind origin/main with a local commit, an edit and an untracked file
  cd "$T"; rm -rf -- "${T:?}/me"; g clone -q -b main "https://github.com/mixpeek/amux.git" "$T/me" 2>/dev/null
  (cd "$T/seed" && echo "two$RANDOM" > f && g commit -qam two && g push -q origin main)
  cd "$T/me"; echo mine >> f && g commit -qam "local commit"; echo dirty >> f; echo new > untracked.txt
}

echo "1. GitHub not connected: exit 4, nothing replaced"
fresh
set +e; out=$(FAKE_GH_AUTHED=0 run --then-pull --json); rc=$?; set -e
check "exit 4"                     4 "$rc"
check "verdict"                    1 "$(grep -c '"verdict": "github_not_connected"' <<<"$out")"
check "local commit kept"          "local commit" "$(git log -1 --format=%s)"
check "untracked kept"             yes "$([ -e untracked.txt ] && echo yes || echo no)"

echo "2. push rights: PR from a branch on mixpeek/amux, then the checkout matches origin/main"
out=$(run --then-pull --json) || { echo "$out"; exit 1; }
br=$(grep -A1 -x -- --head "$T/pr-create.args" | tail -1)
check "branch named for the host"  1 "$(grep -c '^contrib/' <<<"$br")"
check "and for the snapshot"       1 "$(grep -c -- "-$(git --git-dir="$T/remotes/mixpeek/amux.git" rev-parse --short=7 "refs/heads/$br")\$" <<<"$br")"
snap=$(git --git-dir="$T/remotes/mixpeek/amux.git" rev-parse "refs/heads/$br")
check "snapshot has the edit"      "$(printf 'one\nmine\ndirty')" "$(git --git-dir="$T/remotes/mixpeek/amux.git" show "$snap:f")"
check "snapshot has untracked"     new "$(git --git-dir="$T/remotes/mixpeek/amux.git" show "$snap:untracked.txt")"
check "PR title is the commit"     1 "$(grep -cx 'local commit' "$T/pr-create.args")"
check "json carries the PR url"    1 "$(grep -c '"pr_url": "https://github.com/mixpeek/amux/pull/7"' <<<"$out")"
check "then matches origin/main"   "$(git rev-parse origin/main)" "$(git rev-parse HEAD)"
check "tree clean"                 "" "$(git status --porcelain)"
check "pre-override backup kept"   1 "$(git for-each-ref refs/amux/pre-override | wc -l | tr -d ' ')"

echo "3. a PR whose head is not the snapshot: nothing replaced"
fresh
set +e; out=$(FAKE_GH_PR_SHA=0000000 run --then-pull --json); rc=$?; set -e
check "exit 1"                     1 "$rc"
check "verdict"                    1 "$(grep -c '"verdict": "pr_unverified"' <<<"$out")"
check "local commit kept"          "local commit" "$(git log -1 --format=%s)"
check "edit kept"                  1 "$(grep -c dirty f)"

echo "4. no push rights: the branch goes to the user's fork"
fresh
out=$(FAKE_GH_PUSH=false run --json) || { echo "$out"; exit 1; }
head=$(grep -A1 -x -- --head "$T/pr-create.args" | tail -1)
check "head is tester:branch"      1 "$(grep -c '^tester:contrib/' <<<"$head")"
check "branch is on the fork"      1 "$(git --git-dir="$T/remotes/tester/amux.git" for-each-ref "refs/heads/${head#tester:}" | wc -l | tr -d ' ')"
check "without --then-pull, kept"  "local commit" "$(git log -1 --format=%s)"

echo "5. nothing local: no PR, and --then-pull just matches origin/main"
rm -f -- "${T:?}/pr-create.args"
g reset -q --hard origin/main; git clean -qfd
out=$(run --then-pull --json) || { echo "$out"; exit 1; }
check "verdict"                    1 "$(grep -c '"verdict": "nothing_to_send"' <<<"$out")"
check "no PR opened"               no "$([ -e "$T/pr-create.args" ] && echo yes || echo no)"
check "logged"                     5 "$(wc -l < "$T/home/.amux/logs/contribute.log" | tr -d ' ')"

[ "$fails" = 0 ] && echo PASS || { echo "FAIL ($fails)"; exit 1; }
