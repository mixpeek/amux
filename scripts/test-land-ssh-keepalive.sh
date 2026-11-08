#!/usr/bin/env bash
# test-land-ssh-keepalive.sh [amux]: amux land's push carries an ssh keepalive,
# keeps an ssh command the owner set, and names a connection closed mid-push.
# git connects before pre-push runs, and GitHub closes an idle connection at about
# 6 minutes, so a longer hook lost the push after every leg passed (2026-11-02).
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lsk.XXXXXX")"; mkdir -p "$H/.amux/logs/land"
trap 'cd /; rm -rf -- "${H:?}"' EXIT
unset GIT_SSH_COMMAND
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
git config amux.landMinIntervalS 0
echo a > f; git add f; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
mkdir -p .git/hooks
cat > .git/hooks/pre-push <<HK
#!/bin/sh
printf '%s' "\$GIT_SSH_COMMAND" > "$H/seen"
[ -f "$H/closed" ] && { echo "Connection to ssh.github.com closed by remote host."; exit 1; }
exit 0
HK
chmod +x .git/hooks/pre-push
fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fail=1; fi; }
land() { HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_LAND_NET_RETRIES=0 AMUX_WORKER=lsk bash "$AM" land --tries 1 --no-batch >>"$H/land.out" 2>&1 </dev/null; }
commit() { rm -f "$H/seen"; git fetch -q origin; git reset -q --hard origin/main; echo "$1" > f; git commit -qm "$1" -- f; }

commit b; land
case "$(cat "$H/seen")" in *ServerAliveInterval=30*) check "the default push sends keepalives" yes yes ;; *) check "the default push sends keepalives" "$(cat "$H/seen")" yes ;; esac

commit c; GIT_SSH_COMMAND="ssh -o Owner=1" land
check "an ssh command in the env keeps precedence" "$(cat "$H/seen")" "ssh -o Owner=1"

commit d; git config core.sshCommand "ssh -o Config=1"; land
check "core.sshCommand keeps precedence" "$(cat "$H/seen")" "ssh -o Config=1"
git config --unset core.sshCommand

commit d2; GIT_SSH="/opt/my ssh" land
check "GIT_SSH keeps precedence" "$(cat "$H/seen")" '/opt/my\ ssh'

commit e; touch "$H/closed"; land
check "a connection closed mid-push is named in the log" "$(grep -c 'verdict=push_connection_lost' "$H/.amux/logs/land.log")" 1
[ "$fail" = 0 ] || tail -20 "$H/land.out"
exit $fail
