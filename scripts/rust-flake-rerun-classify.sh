#!/usr/bin/env bash
# Decide whether a red main `rust` run may be re-run once (rust-flake-rerun.yml).
#
# stdin: one failed job per line, "<job id><TAB><job name>".
# A job is re-runnable when it is an e2e job (a flaky spec), or when its log
# shows the nextest test archive failed to unpack: the artifact arrived
# truncated ("incomplete frame"), so not one test ran and the failure says
# nothing about the code (run 40314557922, shard 3, 2026-10-26). The `test`
# job only aggregates the shards, so it is excused when every other failed
# job is. Anything else (compile, clippy, a unit test) is never re-run.
#
# Prints one verdict line and exits 0 when the run may be re-run, 1 when not.
# FLAKE_LOG_CMD fetches one job's log by id (default: the GitHub API).
set -euo pipefail
log_cmd=${FLAKE_LOG_CMD:-'gh api "repos/$REPO/actions/jobs/$1/logs"'}
fetch_log() { bash -c "$log_cmd" _ "$1" 2>/dev/null || true; }

blocking="" excused="" aggregate=0 n=0
while IFS=$'\t' read -r id name; do
  [ -n "${name:-}" ] || continue
  n=$((n + 1))
  case "$name" in
    e2e*) excused="$excused$name(e2e);"; continue ;;
    test) aggregate=1; continue ;;
  esac
  if fetch_log "$id" | grep -q 'failed to unpack .*target/'; then
    excused="$excused$name(archive_unpack);"
  else
    blocking="$blocking$name;"
  fi
done

if [ "$n" -eq 0 ]; then
  echo "verdict=flake_rerun_skipped reason=no_failed_jobs measured=true n_considered=0"
  exit 1
fi
if [ -n "$blocking" ]; then
  echo "verdict=flake_rerun_skipped reason=non_flake_failure measured=true n_considered=$n jobs=$blocking"
  exit 1
fi
if [ -z "$excused" ] && [ "$aggregate" -eq 1 ]; then
  echo "verdict=flake_rerun_skipped reason=aggregate_only measured=true n_considered=$n"
  exit 1
fi
echo "verdict=flake_rerun_allowed measured=true n_considered=$n jobs=$excused"
