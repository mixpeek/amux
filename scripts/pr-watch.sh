#!/usr/bin/env bash
# Wake the PR handler only when mixpeek/amux's open pull requests change.
#
# Runs as a `shell` schedule, so a quiet tick costs no model turn. It reads
# the open PRs (number, head sha, the ids of comments and reviews by anyone
# except the handler's own identity, and whether CI is still to finish,
# green or red) plus fork CI runs waiting for a
# maintainer's approval, fingerprints them, and when the fingerprint differs
# from the last one it handed off, runs the handler schedule
# (POST /api/schedules/$PR_WATCH_HANDLER/run). That schedule is a `tmux`
# schedule whose prompt does the reviewing and merging.
#
# Prints one verdict line per run, so the schedule's run history says what
# happened: verdict=woke|unchanged|no_prs|unmeasured|wake_refused.
#
# Env:
#   PR_WATCH_HANDLER  schedule id to run (required)
#   PR_WATCH_REPO     default mixpeek/amux
#   PR_WATCH_STATE    default ~/.amux/state/pr-watch.fingerprint
#   PR_WATCH_GH       gh binary, for tests
#   PR_WATCH_URL      amux base URL, default `amux url`
#   PR_WATCH_SELF     the login the handler comments as (default esteininger).
#                     Its own comments are left out of the fingerprint, so a
#                     review the handler posts does not wake it again; a
#                     contributor's reply or push does.
set -euo pipefail

repo=${PR_WATCH_REPO:-mixpeek/amux}
state=${PR_WATCH_STATE:-$HOME/.amux/state/pr-watch.fingerprint}
gh=${PR_WATCH_GH:-gh}
self=${PR_WATCH_SELF:-esteininger}
handler=${PR_WATCH_HANDLER:?set PR_WATCH_HANDLER to the handler schedule id}

# The App token keeps these reads off the user budget every lane shares.
if [ -z "${PR_WATCH_GH:-}" ] && [ -x "$HOME/.amux/github-app/get-token.sh" ]; then
  eval "$("$HOME/.amux/github-app/get-token.sh")" || true
fi

# One line per PR: number, head, the ids of comments and reviews by anyone
# but $self, and CI as one word: wait (none yet, or running), green or red.
# Never the per-job states, so a run wakes the handler once, when it
# finishes, and not as it starts or on each job along the way.
PR_LINE='.[] | "\(.number) \(.headRefOid) \([(.comments[], .reviews[]) | select(.author.login != $self) | .id] | sort | join(","))"
  + " ci:" + ([.statusCheckRollup[]? | (.conclusion // .state // .status // "") | ascii_upcase] as $c
    | if ($c | length) == 0 then "wait"
      elif any($c[]; IN("", "IN_PROGRESS", "QUEUED", "PENDING", "EXPECTED", "WAITING", "REQUESTED")) then "wait"
      elif any($c[]; IN("FAILURE", "ERROR", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED", "STARTUP_FAILURE")) then "red"
      else "green" end)'
if ! raw=$("$gh" pr list -R "$repo" --state open -L 100 \
      --json number,headRefOid,comments,reviews,statusCheckRollup 2>&1) \
   || ! prs=$(printf '%s' "$raw" | jq -r --arg self "$self" "$PR_LINE" 2>&1); then
  echo "pr-watch: could not list PRs on $repo: ${prs:-$raw} verdict=unmeasured"
  exit 1
fi
if ! waiting=$("$gh" api "repos/$repo/actions/runs?status=action_required&per_page=50" \
      --jq '.workflow_runs[] | "run \(.id) \(.head_sha)"' 2>&1); then
  echo "pr-watch: could not list runs awaiting approval on $repo: $waiting verdict=unmeasured"
  exit 1
fi

n=$(printf '%s\n' "$prs" | grep -c . || true)
if [ "$n" -eq 0 ]; then
  echo "pr-watch: 0 open PRs on $repo verdict=no_prs"
  exit 0
fi

fp=$(printf '%s\n%s\n' "$prs" "$waiting" | sort | shasum -a 256 | cut -c1-16)
prev=$(cat "$state" 2>/dev/null || true)
nums=$(printf '%s\n' "$prs" | awk '{printf "#%s ", $1}')
if [ "$fp" = "$prev" ]; then
  echo "pr-watch: $n open PR(s) ($nums) unchanged since the last hand-off verdict=unchanged"
  exit 0
fi

url=${PR_WATCH_URL:-$(amux url)}
code=$(curl -sk --max-time 30 -o /dev/null -w '%{http_code}' -X POST \
  -H "X-Amux-Session: ${AMUX_SESSION:-pr-watch}" "$url/api/schedules/$handler/run")
case "$code" in
  2??)
    mkdir -p "$(dirname "$state")"
    printf '%s\n' "$fp" > "$state.tmp" && mv "$state.tmp" "$state"
    echo "pr-watch: $n open PR(s) ($nums) changed, ran $handler verdict=woke" ;;
  *)
    # The fingerprint is not saved, so the next tick tries again.
    echo "pr-watch: $handler refused the run (http $code) verdict=wake_refused"
    exit 1 ;;
esac
