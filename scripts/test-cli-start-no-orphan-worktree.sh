#!/usr/bin/env bash
# DESKT-55. `amux start` must not create a worktree for a start it hands to the
# server. On 2026-09-25 the CLI made eight detached 3.6 GB worktrees under
# ~/.amux/worktrees for project workers (px-bucket-objects-gs3-*): it ran
# `git worktree add --detach $CC_HOME/worktrees/<name> HEAD` for CC_WORKTREE=1,
# then delegated the start to the server, which owns project checkouts and
# refused ("held for human artifact review"). Nothing used or removed them.
#
# This drives the REAL `amux start` (not an extracted paraphrase): a fixture repo,
# a hermetic CC_HOME, a fake tmux that reports nothing running, and a server URL
# that cannot answer, so every delegated start fails before launching anything.
# What is asserted is the filesystem: did a worktree appear. The positive control
# (a plain claude worker, which the CLI launches itself) must still get one, or
# a CLI that never creates worktrees at all would pass every negative cell.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLI="$ROOT/amux"
TMP="$(mktemp -d)"
trap 'rm -rf -- "${TMP:?}"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
fails=0
check() { if [[ "$2" == "$3" ]]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }

repo="$TMP/repo"
git init -q -b main "$repo"
git -C "$repo" -c user.email=t@t -c user.name=t -c core.hooksPath=/dev/null commit -q --allow-empty -m seed

# tmux: "not running" for has-session, success for anything else, so the
# positive control can launch detached without a real multiplexer.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/tmux" <<'EOF'
#!/bin/bash
case "${1:-}" in has-session|list-sessions|list-panes) exit 1 ;; esac
exit 0
EOF
chmod +x "$TMP/bin/tmux"

export CC_HOME="$TMP/home"
mkdir -p "$CC_HOME/sessions"
register() { # <name> <provider> <project> <ephemeral>
  {
    printf 'CC_DIR="%s"\n' "$repo"
    printf 'CC_WORKTREE="1"\n'
    printf 'CC_PROVIDER="%s"\n' "$2"
    if [[ -n "$3" ]]; then printf 'CC_PROJECT="%s"\n' "$3"; fi
    if [[ -n "$4" ]]; then printf 'CC_EPHEMERAL="%s"\n' "$4"; fi
  } > "$CC_HOME/sessions/$1.env"
}
start() { # <name> -> output
  PATH="$TMP/bin:$PATH" AMUX_API="https://127.0.0.1:9" AMUX_CLAUDE_CMD=true \
    bash "$CLI" start "$1" --detach 2>&1 || true
}
has_wt() { [[ -d "$CC_HOME/worktrees/$1" ]] && echo yes || echo no; }

echo "1. the decision"
awk '/^server_owns_start\(\) \{/,/^}$/' "$CLI" > "$TMP/fn.sh"
check "server_owns_start() exists in the CLI" "yes" "$([[ -s "$TMP/fn.sh" ]] && echo yes || echo no)"
# shellcheck source=/dev/null
source "$TMP/fn.sh"
yn() { if "$@"; then echo yes; else echo no; fi; }
check "codex is server-owned"                 "yes" "$(yn server_owns_start codex "" "")"
check "gemini is server-owned"                "yes" "$(yn server_owns_start gemini "" "")"
check "a claude project worker is server-owned"   "yes" "$(yn server_owns_start claude demo "")"
check "a claude ephemeral worker is server-owned" "yes" "$(yn server_owns_start claude "" 1)"
check "a plain claude worker is NOT server-owned" "no"  "$(yn server_owns_start claude "" "")"
check "CC_EPHEMERAL=0 does not count"             "no"  "$(yn server_owns_start claude "" 0)"

echo "2. the real start: no worktree for a start the server owns"
register px-demo claude demo ""
out=$(start px-demo)
check "a project worker's start leaves no CLI worktree"   "no" "$(has_wt px-demo)"
check "and it went to the server (which could not answer)" "yes" "$([[ "$out" == *"server unreachable"* ]] && echo yes || echo no)"
register eph-demo claude "" 1
start eph-demo >/dev/null
check "an ephemeral worker's start leaves no CLI worktree" "no" "$(has_wt eph-demo)"
register cx-demo codex "" ""
start cx-demo >/dev/null
check "a codex worker's start leaves no CLI worktree"     "no" "$(has_wt cx-demo)"
check "no worktree was registered in the repo either"     "1"  "$(git -C "$repo" worktree list | wc -l | tr -d ' ')"

echo "3. control: a plain claude worker still gets its CLI worktree"
register plain claude "" ""
out=$(start plain)
check "the CLI-launched worker has its worktree" "yes" "$(has_wt plain)"
[[ "$(has_wt plain)" == yes ]] || echo "    start output: $(printf '%s' "$out" | tail -3 | tr '\n' ' ')"

echo
if [[ "$fails" -eq 0 ]]; then echo "PASS: cli-start-no-orphan-worktree — all checks passed"; exit 0; fi
echo "cli-start-no-orphan-worktree: $fails check(s) FAILED"; exit 1
