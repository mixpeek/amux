#!/usr/bin/env bash
# Main's CI is the full gate now that PRs run only affected tests (#317), and
# the builder deploys every main commit without waiting for it. So a red main
# must be noticed by someone who will fix it. Run as a shell schedule every few
# minutes: for each workflow, look at main's newest completed run and file ONE
# board card on the amux lane per newly failed run, naming the failing tests and
# the commits since the last green run. DRY=1 prints instead of filing.
set -euo pipefail
REPO=mixpeek/amux
STATE="${MAIN_CI_WATCH_STATE:-${AMUX_HOME:-$HOME/.amux}/state/main-ci-watch.json}"
mkdir -p "$(dirname "$STATE")"; [ -f "$STATE" ] || echo '{}' > "$STATE"
eval "$(~/.amux/github-app/get-token.sh --min-remaining 300)"
U="$(amux url)"
filed=0 considered=0 refused=0
# Truncate by CHARACTERS and drop invalid bytes. `cut -c` counts bytes on macOS,
# so a cut through a multi-byte character (the "›" in every Playwright test
# name) left invalid UTF-8; json.dumps carried it as a lone surrogate and both
# the board and `amux send` refused the body (SCHED-713, 2026-10-16 13:31).
clean() { python3 -c 'import sys; print(sys.argv[1].encode("utf-8","surrogateescape").decode("utf-8","replace")[:int(sys.argv[2])])' "$1" "$2"; }
for wf in rust.yml checks.yml; do
  row=$(gh api "repos/$REPO/actions/workflows/$wf/runs?branch=main&status=completed&per_page=1" \
        --jq '.workflow_runs[0] | [.id, .conclusion, .head_sha, .html_url] | @tsv') || continue
  [ -n "$row" ] || continue
  considered=$((considered + 1))
  IFS=$'\t' read -r id conclusion sha url <<<"$row"
  seen=$(python3 -c "import json,sys; s=json.load(open('$STATE')); print(s.get('$wf',{}).get('seen',''))")
  [ "$seen" = "$id" ] && continue
  if [ "$conclusion" = success ]; then
    python3 - "$STATE" "$wf" "$id" "$sha" <<'PY'
import json,sys
p,wf,i,sha=sys.argv[1:]; s=json.load(open(p)); s[wf]={'seen':i,'green':sha}; json.dump(s,open(p,'w'))
PY
    continue
  fi
  [ "$conclusion" = failure ] || { python3 - "$STATE" "$wf" "$id" <<'PY'
import json,sys
p,wf,i=sys.argv[1:]; s=json.load(open(p)); s.setdefault(wf,{})['seen']=i; json.dump(s,open(p,'w'))
PY
    continue; }
  green=$(python3 -c "import json; print(json.load(open('$STATE')).get('$wf',{}).get('green',''))")
  tests=$(gh api "repos/$REPO/actions/runs/$id/jobs" --jq '.jobs[]|select(.conclusion=="failure")|.id' | head -6 | while read -r j; do
      gh api "repos/$REPO/actions/jobs/$j/logs" 2>/dev/null | grep -av WebServer \
        | grep -aE '^\S+\s+[0-9]+\) \[|^\S+ +test .* FAILED|^\S+ +not ok|  FAIL  ' | sed 's/^[0-9TZ:.-]* //' | head -3 || true
    done | sed 's/^ *//' | sort -u | head -10 || true)
  commits=""
  if [ -n "$green" ]; then
    commits=$(gh api "repos/$REPO/compare/$green...$sha" --jq '.commits[] | "\(.sha[0:9]) \(.commit.message | split("\n")[0])"' 2>/dev/null | tail -15 || true)
  fi
  tests=$(clean "$tests" 3000); commits=$(clean "$commits" 3000)
  first=$(clean "$(printf '%s\n' "$tests" | head -1)" 90)
  title="main CI red: ${wf%.yml} on ${sha:0:9}${first:+: $first}"
  desc=$(printf 'Run: %s\n\nFailing:\n%s\n\nCommits since the last green %s run (%s):\n%s\n\nPRs now run only affected tests (#317), so main is the full gate and the builder already deployed this commit. Fix forward or revert the culprit.' \
    "$url" "${tests:-see the run}" "${wf%.yml}" "${green:0:9}" "${commits:-unknown: no green run recorded yet}")
  landed=0
  if [ "${DRY:-0}" = 1 ]; then
    printf 'DRY card: %s\n%s\n' "$title" "$desc"; landed=1
  else
    # todo first. The lane's WIP gate refuses a new todo while it holds too
    # many (2026-10-16: "holding 76, limit 20", so the first real red run
    # filed nothing): then file it in backlog AND message the lane, because a
    # backlog card alone is not dispatched and a red main holds every deploy.
    for status in todo backlog; do
      body=$(python3 -c "import json,sys; print(json.dumps({'title':sys.argv[1],'desc':sys.argv[2],'status':sys.argv[3],'session':'amux'}))" "$title" "$desc" "$status")
      resp=$(mktemp "${TMPDIR:-/tmp}/main-ci-card.XXXXXX")   # GNU mktemp -t needs X's; this form works on both
      code=$(curl -sk --max-time 20 -o "$resp" -w '%{http_code}' -X POST -H 'Content-Type: application/json' -H 'X-Amux-Session: amux' -d "$body" "$U/api/board") || code=000
      # A board that does not answer in 20s (curl exit 28) is a refusal like any
      # other: under set -e the bare assignment ended the script there, before
      # the message fallback (SCHED-713, 2026-10-16 20:43, "exit 28").
      # The refusal's reason, not just its status: 2026-10-16 13:23 two 400s
      # were the lane's WIP gate ("blocked", holding 348 open cards), which a
      # bare code could not say.
      case "$code" in 2??) filed=$((filed + 1)); landed=1; rm -f "$resp"; break ;; *) echo "board refused a $status card: HTTP $code: $(head -c 240 "$resp" | tr '\n' ' ')" >&2 ;; esac
      rm -f "$resp"
    done
    if [ "$status" = backlog ] || [ "$landed" = 0 ]; then
      # One retry: the 13:23 fallback failed once and the same send from a bare
      # environment went through minutes later. Its own error is kept this time.
      for try in 1 2; do
        if err=$(printf 'Ask: main CI is red and holds every deploy. %s\n%s\nFix forward or revert the culprit.\n' "$title" "$url" | amux send amux --stdin 2>&1 >/dev/null); then landed=1; break; fi
        echo "could not message the amux lane (try $try): ${err:0:240}" >&2
        [ "$try" = 1 ] && sleep 5
      done
    fi
    [ "$landed" = 1 ] || refused=$((refused + 1))
  fi
  # Mark the run seen only once someone was told; an alert that never landed
  # is retried on the next tick.
  if [ "$landed" = 1 ]; then
    python3 - "$STATE" "$wf" "$id" <<'PY'
import json,sys
p,wf,i=sys.argv[1:]; s=json.load(open(p)); s.setdefault(wf,{})['seen']=i; json.dump(s,open(p,'w'))
PY
  fi
done
echo "{\"verdict\":\"main_ci_watch\",\"measured\":true,\"n_considered\":$considered,\"cards_filed\":$filed,\"alerts_not_delivered\":$refused}"
# A red run nobody was told about is a failed tick, so the schedule reports it.
[ "$refused" = 0 ]
