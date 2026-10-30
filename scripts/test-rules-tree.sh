#!/usr/bin/env bash
# scripts/check_rules_tree.py on a scratch tree: it passes a well-formed tree and refuses each
# way a rule file can leave it. Then the real tree passes.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
CHK="$ROOT/scripts/check_rules_tree.py"
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }

fresh() {
  rm -rf "$T/r"; mkdir -p "$T/r/a/b" && cd "$T/r" && git init -q
  printf '<!-- rules: root owner=v -->\n# V\n### A1. One\n### A2. Two\n' > VISION.md
  printf '<!-- rules: parent=VISION.md owner=v enforces=all -->\n# root\n' > CLAUDE.md
  printf '<!-- rules: parent=../../CLAUDE.md owner=v enforces=A2 -->\n# b\n' > a/b/CLAUDE.md
  ln -s CLAUDE.md AGENTS.md; ln -s CLAUDE.md a/b/AGENTS.md
  git add -A >/dev/null
}
run() { python3 "$CHK" --root "$T/r" > "$T/out" 2>&1; }

fresh; run && ok "a well-formed tree passes" || bad "a well-formed tree failed: $(cat "$T/out")"

fresh; printf '# no header\n' > a/b/CLAUDE.md
run && bad "a missing header passed" || { grep -q "a/b/CLAUDE.md: first line" "$T/out" && ok "a missing header is refused" || bad "missing header: $(cat "$T/out")"; }

fresh; printf '<!-- rules: parent=VISION.md owner=v enforces=A2 -->\n' > a/b/CLAUDE.md
run && bad "a wrong parent passed" || { grep -q "nearest rule file above it is ../../CLAUDE.md" "$T/out" && ok "a wrong parent is refused" || bad "wrong parent: $(cat "$T/out")"; }

fresh; printf '<!-- rules: parent=../../CLAUDE.md owner=v enforces=A9 -->\n' > a/b/CLAUDE.md
run && bad "an unknown invariant passed" || { grep -q "enforces A9, which VISION.md does not define" "$T/out" && ok "an unknown invariant is refused" || bad "unknown id: $(cat "$T/out")"; }

fresh; rm a/b/AGENTS.md
run && bad "a missing AGENTS.md passed" || { grep -q "AGENTS.md beside it must be a symlink" "$T/out" && ok "a missing AGENTS.md is refused" || bad "agents: $(cat "$T/out")"; }

fresh; printf '<!-- rules: root owner=v -->\n### A1. One\n### A1. Again\n' > VISION.md
run && bad "a duplicate invariant passed" || { grep -q "defined twice: A1" "$T/out" && ok "an invariant defined twice is refused" || bad "dup: $(cat "$T/out")"; }

# A folder with no rule file between: the parent is the nearest one above, not the folder above.
fresh; mkdir -p a/b/c/d; printf '<!-- rules: parent=../../CLAUDE.md owner=v enforces=A1 -->\n' > a/b/c/d/CLAUDE.md
ln -s CLAUDE.md a/b/c/d/AGENTS.md; git add -A >/dev/null
run && ok "a parent two folders up is accepted" || bad "skip-level parent: $(cat "$T/out")"

python3 "$CHK" --root "$ROOT" > "$T/real" 2>&1 && ok "the real tree passes" || bad "the real tree: $(cat "$T/real")"
python3 "$CHK" --root "$ROOT" --map | grep -q "crates/amux-server/CLAUDE.md" && ok "--map prints the tree from the headers" || bad "--map"

echo "test-rules-tree: $fails failed"
exit "$fails"
