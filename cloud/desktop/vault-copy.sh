#!/bin/bash
# Copy this machine's amux vault secrets to another amux host you reach over SSH. Run it in your
# own terminal: reading a vault value is owner-only by design, so a worker cannot run it.
#
#   bash cloud/desktop/vault-copy.sh ethan@1.2.3.4            # dry run: lists what would copy
#   bash cloud/desktop/vault-copy.sh ethan@1.2.3.4 --apply    # copies
#
# Values go local amux (reveal, audited) -> this process -> the host's loopback through an SSH
# tunnel (so the host sees an owner request) and are never printed or written to disk.
# An item already on the host with the same key, scope and target is left alone.
# Written from copying a Mac's vault to mixpeek-dev.amux.io (2026-10-31): without the owner
# token every reveal answered 403.
set -euo pipefail
TARGET="${1:?usage: vault-copy.sh <user@host> [--apply]}"
APPLY=0; [[ "${2:-}" == "--apply" ]] && APPLY=1
LOCAL="$(amux url)"
PORT="${VAULT_COPY_PORT:-18824}"
ssh -o ExitOnForwardFailure=yes -f -N -L "$PORT:127.0.0.1:8824" "$TARGET"
trap 'pkill -f "ssh -o ExitOnForwardFailure=yes -f -N -L $PORT:127.0.0.1:8824 $TARGET" || true' EXIT
REMOTE="https://127.0.0.1:$PORT"
APPLY=$APPLY LOCAL="$LOCAL" REMOTE="$REMOTE" python3 - <<'EOF'
import json, os, ssl, urllib.request
ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
# The local amux has an owner token (~/.amux/auth_token); a request without it is not proof of
# ownership, so reveal needs it. It is sent only to the local server, never printed.
_tok_path = os.path.expanduser("~/.amux/auth_token")
LOCAL_TOKEN = open(_tok_path).read().strip() if os.path.exists(_tok_path) else ""
def call(base, method, path, body=None):
    h = {"Content-Type": "application/json"}
    if base == os.environ["LOCAL"] and LOCAL_TOKEN:
        h["Authorization"] = "Bearer " + LOCAL_TOKEN
    r = urllib.request.Request(base + path, method=method, data=json.dumps(body).encode() if body is not None else None,
                               headers=h)
    try:
        with urllib.request.urlopen(r, context=ctx, timeout=30) as resp:
            return resp.status, json.load(resp)
    except urllib.error.HTTPError as e:
        try: return e.code, json.load(e)
        except Exception: return e.code, {}
local, remote, apply = os.environ["LOCAL"], os.environ["REMOTE"], os.environ["APPLY"] == "1"
st, mine = call(local, "GET", "/api/vault/secrets")
assert st == 200, f"local vault list: HTTP {st} {mine}"
st, theirs = call(remote, "GET", "/api/vault/secrets")
assert st == 200, f"remote vault list: HTTP {st} {theirs}"
have = {(i.get("key"), i.get("scope"), i.get("target") or "") for i in theirs.get("items", [])}
copied = skipped = failed = 0
for it in mine.get("items", []):
    key, scope, target = it.get("key"), it.get("scope") or "global", it.get("target") or ""
    label = f"{key} [{scope}{':' + target if target else ''}]"
    if (key, scope, target) in have:
        print("already there:", label); skipped += 1; continue
    if not apply:
        print("would copy:", label); continue
    st, rv = call(local, "POST", f"/api/vault/secrets/{it['id']}/reveal", {})
    if st != 200 or "value" not in rv:
        print("could not read:", label, st, rv.get("error")); failed += 1; continue
    body = {"key": key, "value": rv["value"], "scope": scope, "target": target,
            "description": it.get("description") or "", "kind": it.get("kind") or "secret"}
    st, out = call(remote, "POST", "/api/vault/secrets", body)
    rv = body = None
    if st in (200, 201):
        print("copied:", label); copied += 1
    else:
        print("not copied:", label, st, (out or {}).get("error")); failed += 1
print(f"done: {copied} copied, {skipped} already there, {failed} failed" + ("" if apply else " (dry run; add --apply)"))
EOF
