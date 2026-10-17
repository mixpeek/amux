#!/usr/bin/env bash
# `amux resources` and `amux boundaries` print from real-shaped payloads.
#
# Both verbs format JSON with an embedded python program. The first version
# shipped with escaped quotes that python rejects, and nothing ran the verbs,
# so both crashed on first use. This serves fixture payloads from a local
# http.server (it ignores query strings) and checks the lines each verb must
# print, with both the PATH python3 and /usr/bin/python3 (3.9 on macOS).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
cleanup() { [[ -n "${SRV:-}" ]] && kill "$SRV" 2>/dev/null || true; rm -rf "${TMP:?}"; }
trap cleanup EXIT

mkdir -p "$TMP/www/api"
cat > "$TMP/www/api/resources" <<'JSON'
{"measured":true,"n_considered":3,"host":{"hostname":"box","cpu_count":8,"ram_total_mb":16384,"disk_free_gb":100,"load_avg":[4.0,6.0,5.0],"ram_percent":70,"swap_used_mb":1024,"swap_total_mb":4096},
 "pressure":{"level":"strained","reasons":["swap 85% used"],"measured":true,"why_unmeasured":null},
 "workers_running":2,"workers_rss_mb":3072,
 "top_by_memory":[{"name":"alpha","cpu_percent":150,"rss_mb":2048,"n_processes":12},{"name":"beta","cpu_percent":5,"rss_mb":1024,"n_processes":3}],
 "worker":{"name":"alpha","measured":true,"cpu_percent":150,"rss_mb":2048,"n_processes":12,"share_of_worker_memory":0.667,
   "profile":{"declared":true,"cpus":1.0,"mem_gb":4.0},"over_profile":["cpu 1.5 cores over the declared 1"]}}
JSON
cat > "$TMP/www/api/boundaries" <<'JSON'
{"measured":true,"n_considered":2,"source":"installed","override_file":"/x/boundaries.toml","override_present":false,"errors":[],
 "hook":{"guard_installed":true,"wired_in_settings":false,"registry_matches_build":true},
 "boundaries":[{"id":"payments","title":"Every payment is authorized","enabled":true,"enforced_by":["server: vault"],"last_24h":{"boundary_refused":2}},
               {"id":"resources","title":"Know what you use","enabled":false,"enforced_by":[],"last_24h":{}}],
 "not_covered_by_hook":{"isolated_workers":["iso1","iso2"]}}
JSON

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
(cd "$TMP/www" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
SRV=$!
for _ in $(seq 1 50); do curl -s -o /dev/null "http://127.0.0.1:$PORT/api/resources" && break; sleep 0.1; done

# The CLI replaces a localhost AMUX_API that endpoint.json does not name, so
# the fixture server gets its own CC_HOME whose endpoint.json names it.
mkdir -p "$TMP/home"
printf '{"canonical_url":"http://127.0.0.1:%s","retired_ports":[]}\n' "$PORT" > "$TMP/home/endpoint.json"

pass=0 fail=0
check() {  # check <label> <haystack> <needle>
  if grep -qF -- "$3" <<<"$2"; then pass=$((pass + 1)); echo "ok   $1"; else fail=$((fail + 1)); echo "FAIL $1: missing '$3'"; printf '%s\n' "$2" | sed 's/^/     /'; fi
}

pythons=(python3)
[[ -x /usr/bin/python3 ]] && pythons+=(/usr/bin/python3)
for py in "${pythons[@]}"; do
  bin="$TMP/bin-$(basename "$py")-$RANDOM"; mkdir -p "$bin"; ln -s "$(command -v "$py")" "$bin/python3"
  run() { PATH="$bin:$PATH" CC_HOME="$TMP/home" AMUX_API="http://127.0.0.1:$PORT" AMUX_URL="http://127.0.0.1:$PORT" AMUX_SESSION=alpha bash "$ROOT/amux" "$@" 2>&1 || true; }
  r=$(run resources)
  check "[$py] resources: machine line" "$r" "Machine  box: 8 cores, 16 GB RAM, 100 GB disk free"
  check "[$py] resources: load per core" "$r" "(0.75 per core over 5 min)"
  check "[$py] resources: level and reason" "$r" "level   strained  (swap 85% used)"
  check "[$py] resources: top worker" "$r" "alpha"
  check "[$py] resources: own usage" "$r" "using   150% CPU, 2.00 GB in 12 processes (67% of all worker memory)"
  check "[$py] resources: over profile" "$r" "OVER    cpu 1.5 cores over the declared 1"
  b=$(run boundaries)
  check "[$py] boundaries: guard line" "$b" "Guard    installed=True wired=False registry_matches_build=True"
  check "[$py] boundaries: not-live warning" "$b" "WARNING: the guard is not live"
  check "[$py] boundaries: firing count" "$b" "24h     boundary_refused 2"
  check "[$py] boundaries: disabled flag" "$b" "[resources] Know what you use  (DISABLED)"
  check "[$py] boundaries: isolated count" "$b" "Not covered by the guard (isolated): 2 workers"
done

echo "$pass passed, $fail failed over ${#pythons[@]} python interpreter(s)"
[[ "$fail" -eq 0 && "$pass" -gt 0 ]]
