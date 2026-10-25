#!/usr/bin/env bash
# Main's CI is the full gate now that PRs run only affected tests (#317), and
# the builder deploys only a main commit that passed it. So a red main holds
# every deploy and must be noticed by someone who will fix it. Run as a shell
# schedule every few minutes: for each workflow, look at main's newest completed
# run. The first failed run opens ONE board card on the amux lane, naming the
# failing tests and the commits since the last green run; each later failed run
# is appended to that card, and the next green run closes it. DRY=1 prints
# instead of filing.
#
# One card per red STRETCH, not per run (2026-10-24). The per-run version filed
# 12 cards in two days, all discarded as duplicates, and SCHED-713 was switched
# off from the dashboard on 10-17. Main then stayed red for a week with nobody
# told, and the server sat 73 commits behind on 08c8c6973.
#
# A first attempt that failed ONLY in e2e jobs is re-run once before anyone is
# told. In that same week every one of five consecutive main runs failed in
# exactly one e2e spec, a different one each time, in code the commit did not
# touch, so no commit ever went green and the gate deployed nothing. Test-level
# retries stay 0 (e2e/playwright.config.ts says why); a job re-run keeps the
# first attempt's evidence (artifacts carry the attempt), a real regression
# fails attempt 2 as well and is reported then, and every re-run prints the
# flaky test here as verdict=flaky_rerun.
set -euo pipefail
REPO=mixpeek/amux
STATE="${MAIN_CI_WATCH_STATE:-${AMUX_HOME:-$HOME/.amux}/state/main-ci-watch.json}"
mkdir -p "$(dirname "$STATE")"; [ -f "$STATE" ] || echo '{}' > "$STATE"
eval "$(~/.amux/github-app/get-token.sh --min-remaining 300)"
U="$(amux url)"
filed=0 appended=0 closed=0 reruns=0 considered=0 refused=0
st_get() { python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2],{}).get(sys.argv[3],''))" "$STATE" "$1" "$2"; }
st_set() { python3 -c "
import json,sys
p,wf,k,v=sys.argv[1:]; s=json.load(open(p)); d=s.setdefault(wf,{})
if v: d[k]=v
else: d.pop(k,None)
json.dump(s,open(p,'w'))" "$STATE" "$1" "$2" "$3"; }
# Is the stretch's card still open? Closed, discarded or archived means the
# next red run starts a new one rather than writing into a retired card.
card_open() {
  [ -n "$1" ] || return 1
  curl -sk --max-time 20 "$U/api/board/$1" 2>/dev/null | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: sys.exit(1)
sys.exit(0 if d.get('id') and not d.get('archived') and d.get('status') not in ('done','verified','discarded') else 1)"
}
board_patch() {  # id json -> prints the HTTP code
  local resp code; resp=$(mktemp "${TMPDIR:-/tmp}/main-ci-patch.XXXXXX")
  code=$(curl -sk --max-time 20 -o "$resp" -w '%{http_code}' -X PATCH -H 'Content-Type: application/json' -H 'X-Amux-Session: amux' -d "$2" "$U/api/board/$1") || code=000
  case "$code" in 2??) ;; *) echo "board refused a PATCH on $1: HTTP $code: $(head -c 240 "$resp" | tr '\n' ' ')" >&2 ;; esac
  rm -f "$resp"; echo "$code"
}
# Truncate by CHARACTERS and drop invalid bytes. `cut -c` counts bytes on macOS,
# so a cut through a multi-byte character (the "›" in every Playwright test
# name) left invalid UTF-8; json.dumps carried it as a lone surrogate and both
# the board and `amux send` refused the body (SCHED-713, 2026-10-16 13:31).
clean() { python3 -c 'import sys; print(sys.argv[1].encode("utf-8","surrogateescape").decode("utf-8","replace")[:int(sys.argv[2])])' "$1" "$2"; }
for wf in rust.yml checks.yml; do
  row=$(gh api "repos/$REPO/actions/workflows/$wf/runs?branch=main&status=completed&per_page=1" \
        --jq '.workflow_runs[0] | [.id, .conclusion, .head_sha, .html_url, .run_attempt] | @tsv') || continue
  [ -n "$row" ] || continue
  considered=$((considered + 1))
  IFS=$'\t' read -r id conclusion sha url attempt <<<"$row"
  attempt=${attempt:-1}
  # A re-run keeps the run id, so an attempt is what makes a run new.
  seen=$(st_get "$wf" seen)
  # A bare id is the pre-attempt state file: it means attempt 1 only.
  if [ "$seen" = "$id.$attempt" ] || { [ "$attempt" = 1 ] && [ "$seen" = "$id" ]; }; then continue; fi
  card=$(st_get "$wf" card)
  if [ "$conclusion" = success ]; then
    # Green again: close the stretch's card with the run that proves it.
    if [ -n "$card" ] && [ "${DRY:-0}" != 1 ] && card_open "$card"; then
      ev=$(printf 'main %s is green again at %s: %s' "${wf%.yml}" "${sha:0:9}" "$url")
      body=$(python3 -c "import json,sys; print(json.dumps({'status':'done','evidence':sys.argv[1],'desc_append':sys.argv[1]}))" "$ev")
      case "$(board_patch "$card" "$body")" in 2??) closed=$((closed + 1)) ;; *) refused=$((refused + 1)); continue ;; esac
    fi
    st_set "$wf" seen "$id.$attempt"; st_set "$wf" green "$sha"; st_set "$wf" card ""
    continue
  fi
  [ "$conclusion" = failure ] || { st_set "$wf" seen "$id.$attempt"; continue; }
  if [ "$conclusion" = failure ] && [ "$attempt" = 1 ]; then
    failed_jobs=$(gh api "repos/$REPO/actions/runs/$id/jobs?per_page=100" --jq '.jobs[]|select(.conclusion=="failure")|.name' 2>/dev/null || true)
    if [ -n "$failed_jobs" ] && ! printf '%s\n' "$failed_jobs" | grep -qv '^e2e'; then
      flaky=$(gh api "repos/$REPO/actions/runs/$id/jobs" --jq '.jobs[]|select(.conclusion=="failure")|.id' | head -6 | while read -r j; do
          gh api "repos/$REPO/actions/jobs/$j/logs" 2>/dev/null | grep -av WebServer | grep -aE '^\S+\s+[0-9]+\) \[' | sed 's/^[0-9TZ:.-]* //' | head -2 || true
        done | sed 's/^ *//' | sort -u | head -4 || true)
      if [ "${DRY:-0}" = 1 ] || gh api -X POST "repos/$REPO/actions/runs/$id/rerun-failed-jobs" >/dev/null 2>&1; then
        reruns=$((reruns + 1))
        echo "{\"verdict\":\"flaky_rerun\",\"workflow\":\"${wf%.yml}\",\"sha\":\"${sha:0:9}\",\"run\":\"$url\",\"tests\":$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$(clean "$flaky" 600)")}"
        st_set "$wf" seen "$id.$attempt"
        continue
      fi
      echo "could not re-run the failed e2e jobs of $url; reporting it as red" >&2
    fi
  fi
  green=$(st_get "$wf" green)
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
  desc=$(printf 'Run: %s\n\nFailing:\n%s\n\nCommits since the last green %s run (%s):\n%s\n\nPRs run only affected tests (#317), so main is the full gate. The builder deploys only a main commit that passed it, so until main is green the server stays on the last green commit and nothing newer reaches it (GET /api/pull). Fix forward or revert the culprit. Later red runs are appended here and the next green run closes this card.' \
    "$url" "${tests:-see the run}" "${wf%.yml}" "${green:0:9}" "${commits:-unknown: no green run recorded yet}")
  landed=0
  if [ "${DRY:-0}" != 1 ] && card_open "$card"; then
    # Still the same red stretch: add this run to its card instead of a new one.
    note=$(printf 'Still red: %s on %s\nRun: %s\nFailing:\n%s' "${wf%.yml}" "${sha:0:9}" "$url" "${tests:-see the run}")
    body=$(python3 -c "import json,sys; print(json.dumps({'desc_append':sys.argv[1]}))" "$note")
    case "$(board_patch "$card" "$body")" in 2??) appended=$((appended + 1)); landed=1 ;; esac
  fi
  if [ "$landed" = 1 ]; then
    :
  elif [ "${DRY:-0}" = 1 ]; then
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
      case "$code" in 2??) filed=$((filed + 1)); landed=1
        nid=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('id',''))" "$resp" 2>/dev/null || true)
        [ -n "$nid" ] && st_set "$wf" card "$nid"
        rm -f "$resp"; break ;; *) echo "board refused a $status card: HTTP $code: $(head -c 240 "$resp" | tr '\n' ' ')" >&2 ;; esac
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
    st_set "$wf" seen "$id.$attempt"
  fi
done
echo "{\"verdict\":\"main_ci_watch\",\"measured\":true,\"n_considered\":$considered,\"cards_filed\":$filed,\"cards_appended\":$appended,\"cards_closed\":$closed,\"flaky_reruns\":$reruns,\"alerts_not_delivered\":$refused}"
# A red run nobody was told about is a failed tick, so the schedule reports it.
[ "$refused" = 0 ]
