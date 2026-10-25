#!/usr/bin/env bash
# Live end-to-end test of amux's connectors and email path (AMUX-5756).
#
# Proves each claim with a real call and checks that the connector list agrees
# with what the test measured. Exits 1 if anything is broken, so a shell
# schedule can watch it. Exits 3 when the only problems are revoked Google
# grants (needs_reauth): those need the owner to approve again and no code
# change can fix them. The Google OAuth client is in Testing, so a grant for
# an account outside its Workspace expires after 7 days (AMUX-5019); the
# schedule maps 3 to a logged run instead of a failure. Sends mail ONLY between owned accounts (default:
# ethan@mixpeek.com to itself); nothing leaves the company.
#
#   scripts/test-connectors-live.sh [--account ethan@mixpeek.com] [--no-send]
set -euo pipefail
ACCOUNT=ethan@mixpeek.com
SEND=1
while [ $# -gt 0 ]; do
  case "$1" in
    --account) ACCOUNT=$2; shift 2 ;;
    --no-send) SEND=0; shift ;;
    *) echo "unknown flag $1" >&2; exit 2 ;;
  esac
done
U=$(amux url)
H=(-H "X-Amux-Session: ${AMUX_SESSION:-connector-test}")
pass=0; fail=0; skip=0; owner=0
ok()   { pass=$((pass+1)); echo "PASS $*"; }
bad()  { fail=$((fail+1)); echo "FAIL $*"; }
note() { skip=$((skip+1)); echo "SKIP $*"; }
own()  { owner=$((owner+1)); echo "NEEDS-OWNER $*"; }
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {'d': d}))" "$1"; }

# 1. Every connector: its live test, and the list agreeing with it.
ids=$(curl -sk --max-time 20 "$U/api/connectors" | j "' '.join(c['id'] for c in d['connectors'])") || { bad "connector list unreadable"; ids=""; }
for id in $ids; do
  r=$(curl -sk --max-time 60 -X POST "${H[@]}" "$U/api/connectors/$id/test") || r='{}'
  t_ok=$(printf '%s' "$r" | j "d.get('ok')" 2>/dev/null) || t_ok=""
  t_st=$(printf '%s' "$r" | j "d.get('status')" 2>/dev/null) || t_st=""
  t_detail=$(printf '%s' "$r" | j "(d.get('detail') or '')[:140]" 2>/dev/null) || t_detail=""
  # Accounts that failed for a reason other than a revoked grant or a grant
  # that never covered this connector's scopes (not set up, cc1089274).
  t_other=$(printf '%s' "$r" | j "sum(1 for a in d.get('accounts') or [] if not a.get('ok') and a.get('status') not in ('needs_reauth','needs_auth'))" 2>/dev/null) || t_other=""
  t_reauth=$(printf '%s' "$r" | j "' '.join(a['account'] for a in d.get('accounts') or [] if a.get('status')=='needs_reauth')" 2>/dev/null) || t_reauth=""
  l_st=$(curl -sk --max-time 20 "$U/api/connectors" | j "[c['status'] for c in d['connectors'] if c['id']=='$id'][0]") || l_st=""
  case "$t_st" in
    needs_credentials|needs_auth) note "$id not set up ($t_st): $t_detail" ;;
    *) if [ "$t_ok" = "True" ]; then ok "$id live test ($t_detail)"
       elif [ -n "$t_reauth" ] && [ "$t_other" = "0" ]; then own "$id: grant revoked for $t_reauth (approve again: POST /api/connectors/google/auth?account=<email>)"
       else bad "$id live test: $t_detail"; fi ;;
  esac
  if [ "$t_ok" != "True" ] && [ "$l_st" = "connected" ] && [ "$t_st" != "needs_credentials" ]; then
    bad "$id list says connected while its test failed ($t_st)"
  else
    ok "$id list agrees with its test (list=$l_st test_ok=$t_ok)"
  fi
done

# 2. Gmail accounts: each connected account's token health.
curl -sk --max-time 20 "$U/api/gmail/accounts" | python3 -c "
import json,sys
d=json.load(sys.stdin)
for a in d.get('accounts',[]):
    h=d.get('health',{}).get(a,'unknown')
    tag='PASS' if h=='ok' else 'NEEDS-OWNER' if h=='needs_reauth' else 'FAIL'
    hint=' (approve again: GET /api/gmail/auth?account='+a+')' if h=='needs_reauth' else ''
    print(tag, 'gmail account', a, 'health', h+hint)" | while read -r line; do echo "$line"; done
health=$(curl -sk --max-time 20 "$U/api/gmail/accounts" | j "' '.join(str(sum(1 for v in d.get('health',{}).values() if p(v))) for p in (lambda v: v=='needs_reauth', lambda v: v not in ('ok','needs_reauth')))") || health="0 1"
owner=$((owner + ${health% *})); fail=$((fail + ${health#* }))

# 3. Inbox and search on the test account.
n=$(curl -sk --max-time 60 "$U/api/email/inbox?account=$ACCOUNT&count=3&days=7" | j "len(d) if isinstance(d,list) else -1") || n=-1
[ "${n:--1}" -ge 1 ] && ok "inbox $ACCOUNT returned $n message(s)" || bad "inbox $ACCOUNT: $n"
n=$(curl -sk --max-time 60 "$U/api/email/search?q=in:inbox&days=7&limit=3&account=$ACCOUNT" | j "len(d) if isinstance(d,list) else -1") || n=-1
[ "${n:--1}" -ge 1 ] && ok "search $ACCOUNT returned $n message(s)" || bad "search $ACCOUNT: $n"

# 4. Round trip: send to self, find it, read the exact body back, reply in-thread, check the log.
if [ "$SEND" = 1 ]; then
  tag="amux-connector-e2e-$(date +%s)"
  body=$'Line one.\n\nParagraph two after a blank line.\n\n- a bullet'
  payload=$(python3 -c "import json,sys; print(json.dumps({'to':sys.argv[1],'from':sys.argv[1],'subject':sys.argv[2],'body':sys.argv[3],'signature':False}))" "$ACCOUNT" "$tag" "$body")
  r=$(curl -sk --max-time 60 -X POST "${H[@]}" -H 'Content-Type: application/json' -d "$payload" "$U/api/email/send") || r='{}'
  thread=$(printf '%s' "$r" | j "d.get('thread_id') or ''") || thread=""
  [ "$(printf '%s' "$r" | j "d.get('ok')")" = "True" ] && ok "send $tag (thread $thread)" || bad "send: $(printf '%s' "$r" | head -c 200)"
  mid=""
  for _ in 1 2 3 4 5 6; do
    sleep 5
    mid=$(curl -sk --max-time 60 "$U/api/email/search?q=$tag&days=1&limit=5&account=$ACCOUNT" | j "d[0]['message_id'] if isinstance(d,list) and d else ''") || mid=""
    [ -n "$mid" ] && break
  done
  [ -n "$mid" ] && ok "sent message found by search" || bad "sent message not found by search within 30s"
  if [ -n "$mid" ]; then
    enc=$(python3 -c "import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1], safe=''))" "$mid")
    got=$(curl -sk --max-time 60 "$U/api/email/message/$enc" | j "d.get('body') or ''") || got=""
    [ "$got" = "$body" ] && ok "read-back body is byte-identical (blank lines kept)" || bad "read-back body differs: $(printf '%q' "$got" | head -c 160)"
    rp=$(python3 -c "import json,sys; print(json.dumps({'message_id':sys.argv[1],'body':'Reply from the connector e2e.','from':sys.argv[2],'reply_all':False,'signature':False,'allow_self':True}))" "$mid" "$ACCOUNT")
    r=$(curl -sk --max-time 60 -X POST "${H[@]}" -H 'Content-Type: application/json' -d "$rp" "$U/api/email/reply") || r='{}'
    rt=$(printf '%s' "$r" | j "d.get('thread_id') or ''") || rt=""
    if [ "$(printf '%s' "$r" | j "d.get('ok')")" = "True" ] && [ "$rt" = "$thread" ]; then ok "reply landed in the same thread ($rt)"; else bad "reply: $(printf '%s' "$r" | head -c 200)"; fi
  fi
  logged=$(curl -sk --max-time 20 "$U/api/email/log?days=1&limit=20" | j "sum(1 for e in d.get('log',[]) if '$tag' in (e.get('subject') or ''))") || logged=0
  [ "${logged:-0}" -ge 2 ] && ok "send and reply both in the audit log ($logged rows)" || bad "audit log rows for $tag: $logged (expected 2)"
fi

echo "connector-e2e: $pass passed, $fail failed, $owner need the owner to approve again, $skip not set up"
[ "$fail" -eq 0 ] || exit 1
[ "$owner" -eq 0 ] || exit 3
