#!/usr/bin/env bash
# RR-0116 / RR-0117b — the CI-runnable perf measurement.
#
# `scripts/perf-baseline.sh` measures against a copy of the LIVE database,
# which exists on Ethan's machine and nowhere else. CI has no such file, so
# this wrapper:
#
#   1. SEEDS a synthetic database at the plan's stated scale (RR-0110 asks for
#      "FTS5 over 10k entities returns < 50ms", so 10k is the floor, not a
#      round number picked for convenience),
#   2. runs the EXISTING perf-baseline.sh against it — the measurement harness
#      is not duplicated, only fed,
#   3. adds the two metrics that harness does not emit and RR-0117b gates:
#      `search_avg_ms` and `binary_bytes`,
#   4. prints one merged JSON object for scripts/perf-compare.py.
#
# A synthetic corpus is NOT the same signal as the 640k-row live DB, and the
# emitted JSON says so in `_corpus` so a number from here is never mistaken for
# a number from there. Cross-corpus comparison is the reason the CI-measured
# metrics start life in perf-compare.py's "recording only" bucket rather than
# gating on a baseline that was measured somewhere else.
set -euo pipefail

SEED_CARDS="${SEED_CARDS:-10000}"
SEED_MESSAGES="${SEED_MESSAGES:-2000}"
OUT="${1:-/dev/stdout}"
BIN="${AMUX_RS_BIN:-./target/release/amux-server}"
WORK="$(mktemp -d /tmp/amux-perfci.XXXXXX)"
PORT="${PERF_CI_PORT:-18944}"
cleanup() { [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

[ -x "$BIN" ] || { echo "FATAL: no server binary at $BIN (build with cargo build --release -p amux-server)" >&2; exit 2; }

echo "== seeding a synthetic corpus: $SEED_CARDS cards, $SEED_MESSAGES messages" >&2
SEED_DB="$WORK/seed.db"
# The server owns schema creation: boot it once against an empty file so the
# migrations (including 0013's search index + triggers) run, then seed through
# the real tables so the triggers do the indexing exactly as they will in prod.
AMUX_HOME="$WORK" AMUX_DB="$SEED_DB" AMUX_RS_PORT=$((PORT + 1)) "$BIN" >"$WORK/seed-server.log" 2>&1 &
SEED_PID=$!
for _ in $(seq 1 60); do
  curl -sk --max-time 2 "https://localhost:$((PORT + 1))/health" >/dev/null 2>&1 && break
  sleep 0.5
done
kill "$SEED_PID" 2>/dev/null || true
wait "$SEED_PID" 2>/dev/null || true
[ -f "$SEED_DB" ] || { echo "FATAL: server did not create $SEED_DB" >&2; sed -n '1,60p' "$WORK/seed-server.log" >&2; exit 2; }

python3 - "$SEED_DB" "$SEED_CARDS" "$SEED_MESSAGES" <<'PY'
import sqlite3, sys, random, time
db, cards, msgs = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
random.seed(1110)  # deterministic corpus: a perf number that moves because the
                   # data moved is not a perf signal.
words = ("deploy migration index gateway worker schedule board card token session "
         "regression baseline latency snapshot journal memory reviewer archive "
         "quokka numbat pangolin okapi aardvark").split()
def text(n): return " ".join(random.choice(words) for _ in range(n))
c = sqlite3.connect(db)
now = int(time.time())
c.executemany(
    "INSERT INTO issues (id, title, desc, status, session, creator, created, updated, log)"
    " VALUES (?,?,?,?,?,?,?,?,?)",
    [(f"PERF-{i}", text(6), text(60), random.choice(("todo","doing","done")),
      f"lane-{i % 40}", "seed", now - i, now - i, f"`09:{i%60:02d}` seeded {text(4)}\n")
     for i in range(cards)])
c.executemany(
    "INSERT INTO _amux_messages (id, from_actor, target, body, created_at, delivery)"
    " VALUES (?,?,?,?,?,?)",
    [(f"msg_perf_{i}", '{"id":"lane-a"}', '{"id":"lane-b"}', text(40),
      "2026-08-09T00:00:00+00:00", "{}") for i in range(msgs)])
c.commit()
n_docs = c.execute("SELECT COUNT(*) FROM search_docs").fetchone()[0]
n_fts  = c.execute("SELECT COUNT(*) FROM search_fts").fetchone()[0]
c.close()
# The seed must actually have exercised the triggers; a corpus that never got
# indexed would make the search benchmark measure an empty index and pass.
assert n_docs >= cards + msgs, f"index not maintained during seed: {n_docs} docs for {cards + msgs} rows"
assert n_fts == n_docs, f"fts rows {n_fts} != docs {n_docs}"
print(f"   seeded, index holds {n_docs} docs", file=sys.stderr)
PY

# perf-baseline.sh opens its input with `file:...?mode=ro`, and SQLite CANNOT
# open a WAL-mode database read-only when the -shm file is absent: it needs to
# create shared memory and read-only forbids it, so the whole run dies with a
# bare "unable to open database file". The live DB never shows this because a
# server is holding it open. Switching the throwaway seed back to a rollback
# journal makes it readable the same way a quiescent copy is.
sqlite3 "$SEED_DB" "PRAGMA journal_mode=delete;" >/dev/null

echo "== running scripts/perf-baseline.sh against the synthetic corpus" >&2
# LOUD ON FAILURE (AMUX-2872). The old pipeline (`... | grep '^{' | tail -1`)
# inside a command substitution swallowed BOTH streams of a dying
# perf-baseline.sh, and under pipefail the assignment itself exited this
# script before the FATAL guard on the next line could fire — the nightly
# perf job died for 12 straight runs with literally zero output between the
# announcement and exit 1, which is why nobody could chase it. Capture both
# streams, check the exit, and print the dying words before any grep.
set +e
AMUX_LIVE_DB="$SEED_DB" AMUX_RS_BIN="$BIN" bash scripts/perf-baseline.sh \
  >"$WORK/base-out.txt" 2>"$WORK/base-err.txt"
BASE_RC=$?
set -e
if [ "$BASE_RC" -ne 0 ]; then
  echo "FATAL: perf-baseline.sh exited $BASE_RC — its full output:" >&2
  sed 's/^/  [stdout] /' "$WORK/base-out.txt" >&2 || true
  sed 's/^/  [stderr] /' "$WORK/base-err.txt" >&2 || true
  exit 2
fi
BASE_JSON="$(grep -E '^\{' "$WORK/base-out.txt" | tail -1 || true)"
[ -n "$BASE_JSON" ] || {
  echo "FATAL: perf-baseline.sh exited 0 but emitted no JSON line — its full output:" >&2
  sed 's/^/  [stdout] /' "$WORK/base-out.txt" >&2 || true
  sed 's/^/  [stderr] /' "$WORK/base-err.txt" >&2 || true
  exit 2
}

echo "== measuring /api/search (RR-0110 target: < 50ms over 10k entities)" >&2
AMUX_HOME="$WORK" AMUX_DB="$SEED_DB" AMUX_RS_PORT=$PORT "$BIN" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 60); do
  curl -sk --max-time 2 "https://localhost:$PORT/health" >/dev/null 2>&1 && break
  sleep 0.5
done
TOKEN=$(cat "$WORK/auth-token" 2>/dev/null || echo "")
# Assert the index is actually populated BEFORE timing it. Benchmarking an
# empty index returns a fast, meaningless number that looks like a pass.
STATUS_JSON=$(curl -sk -H "Authorization: Bearer $TOKEN" "https://localhost:$PORT/api/search/status")
DOCS=$(printf '%s' "$STATUS_JSON" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("docs_total",0))')
[ "$DOCS" -ge "$SEED_CARDS" ] || { echo "FATAL: search index holds $DOCS docs, expected >= $SEED_CARDS — the benchmark would have measured an empty index" >&2; exit 2; }
echo "   index holds $DOCS docs" >&2

total=0; worst=0
for term in deploy migration quokka "index gateway" regression; do
  t=$(curl -sk -o /dev/null -w '%{time_total}' -H "Authorization: Bearer $TOKEN" \
      --get --data-urlencode "q=$term" "https://localhost:$PORT/api/search")
  ms=$(python3 -c "print(int(float('$t')*1000))")
  total=$((total + ms)); [ "$ms" -gt "$worst" ] && worst=$ms
done
SEARCH_AVG=$((total / 5))
BINARY_BYTES=$(wc -c < "$BIN" | tr -d ' ')

python3 - "$BASE_JSON" "$SEARCH_AVG" "$worst" "$BINARY_BYTES" "$DOCS" "$OUT" <<'PY'
import json, sys
base = json.loads(sys.argv[1])
base["search_avg_ms"] = int(sys.argv[2])
base["search_worst_ms"] = int(sys.argv[3])
base["binary_bytes"] = int(sys.argv[4])
base["_corpus"] = {
    "kind": "synthetic",
    "indexed_docs": int(sys.argv[5]),
    "note": "seeded by scripts/perf-ci.sh; NOT the 640k-row live DB that docs/perf-baseline.json was measured against, so cross-corpus deltas are not regression signals",
}
out = sys.argv[6]
text = json.dumps(base, indent=2)
if out == "/dev/stdout":
    print(text)
else:
    open(out, "w").write(text)
PY

echo "== wrote merged measurement to $OUT" >&2
