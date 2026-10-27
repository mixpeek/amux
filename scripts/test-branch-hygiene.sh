#!/usr/bin/env bash
# Tests scripts/branch-hygiene.py against a throwaway bare remote.
# Each case names the rule it pins, so a mutation reddens a named line.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
BH="$HERE/branch-hygiene.py"
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
fail=0; pass=0
ok()  { echo "PASS $1"; pass=$((pass+1)); }
bad() { echo "FAIL $1: $2"; fail=$((fail+1)); }

export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
git init -q --bare "$T/remote.git"
git clone -q "$T/remote.git" "$T/w" 2>/dev/null
cd "$T/w"
git checkout -q -b main; git commit -q --allow-empty -m base; git push -q origin main
mk() {  # name, merged(yes/no), age_h
  git checkout -q -b "$1" main
  GIT_COMMITTER_DATE="$(date -u -v-"$3"H +%s 2>/dev/null || date -u -d "-$3 hours" +%s) +0000" \
    git commit -q --allow-empty -m "$1"
  git push -q origin "$1"
  if [ "$2" = yes ]; then git checkout -q main; git merge -q --ff-only "$1"; git push -q origin main; fi
  git checkout -q main
}
mk gs12-merged yes 1
mk staging yes 1
mk gs12-old no 100
mk ci/old no 72
mk gs12-new no 2
cd "$T"
git clone -q "$T/remote.git" "$T/c" 2>/dev/null

J=$(python3 "$BH" --repo "$T/c" --json --owner gs12=mxp-gs12 --owner ci/=mixpeek-cicd)
get() { printf '%s' "$J" | python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }

[ "$(get "d['measured']")" = True ] && ok measured || bad measured "$J"
[ "$(get "d['n_considered']")" = 5 ] && ok n_considered_counts_every_branch || bad n_considered_counts_every_branch "$(get "d['n_considered']")"
[ "$(get "[r['branch'] for r in d['merged']]")" = "['gs12-merged']" ] && ok merged_is_ancestor_of_main || bad merged_is_ancestor_of_main "$(get "d['merged']")"
[ "$(get "[r['branch'] for r in d['kept']]")" = "['staging']" ] && ok keep_regex_protects_staging || bad keep_regex_protects_staging "$(get "d['kept']")"
[ "$(get "sorted(r['branch'] for r in d['stale'])")" = "['ci/old', 'gs12-old']" ] && ok stale_is_unmerged_over_age || bad stale_is_unmerged_over_age "$(get "d['stale']")"
[ "$(get "d['fresh']")" = 1 ] && ok fresh_unmerged_not_reported || bad fresh_unmerged_not_reported "$(get "d['fresh']")"
[ "$(get "sorted((r['branch'],r['owner']) for r in d['stale'])")" = "[('ci/old', 'mixpeek-cicd'), ('gs12-old', 'mxp-gs12')]" ] && ok owner_prefix_mapping || bad owner_prefix_mapping "$(get "d['stale']")"

# Delete: only merged and not kept.
python3 "$BH" --repo "$T/c" --delete-merged >/dev/null
left=$(git -C "$T/remote.git" for-each-ref refs/heads --format='%(refname:short)' | sort | tr '\n' ' ')
[ "$left" = "ci/old gs12-new gs12-old main staging " ] && ok delete_only_merged_unkept || bad delete_only_merged_unkept "$left"

# A fetch that cannot run is unmeasured, never an empty report.
rc=0; J=$(python3 "$BH" --repo "$T/nope" --json) || rc=$?
[ $rc = 2 ] && [ "$(get "d['measured']")" = False ] && ok unreachable_repo_is_unmeasured || bad unreachable_repo_is_unmeasured "rc=$rc $J"

echo "branch-hygiene: $pass passed, $fail failed"
[ $fail = 0 ]
