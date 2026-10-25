#!/bin/bash
# Default grpc to fork-safe in every Python that loads a shared site-packages,
# whatever started it (Mac crashes RCA 20261018-105230).
#
# Every crash report since 10-17 is the same stack: a Python with grpc loaded
# from /usr/local/lib/python3.11/site-packages forks, grpc's fork handler
# re-initialises c-ares in the child, and c-ares calls dns_configuration_copy
# into libxpc before exec ("crashed on child side of fork pre-exec").
# 98b8994c5 exports GRPC_ENABLE_FORK_SUPPORT=false and GRPC_DNS_RESOLVER=native
# from worker shells, but a shell's env reaches only shells started after it
# landed: 4 hours later no live Python process carried them and the crashes went
# on (09:19, 09:33, 09:47, 10:26). A .pth file is read by each interpreter at
# startup, so it reaches running lanes' next Python without a restart.
# setdefault: a value the caller set still wins.
#
# Usage: scripts/install-python-grpc-defaults.sh [site-packages...]
#   default: the site-packages of every python3.N on PATH that has grpc installed.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
src="$here/python/amux-grpc-fork-safe.pth"
[ -f "$src" ] || { echo "install-python-grpc-defaults: FAIL $src missing" >&2; exit 1; }
dirs=("$@")
if [ ${#dirs[@]} -eq 0 ]; then
  for py in /usr/local/opt/python@3.*/bin/python3.* /opt/homebrew/opt/python@3.*/bin/python3.*; do
    [ -x "$py" ] || continue
    d=$("$py" -c 'import grpc, os; print(os.path.dirname(os.path.dirname(grpc.__file__)))' 2>/dev/null) || continue
    case " ${dirs[*]-} " in *" $d "*) ;; *) dirs+=("$d") ;; esac
  done
fi
n=0
for d in ${dirs[@]+"${dirs[@]}"}; do
  [ -d "$d" ] || { echo "install-python-grpc-defaults: skip $d (not a directory)"; continue; }
  if cmp -s "$src" "$d/amux-grpc-fork-safe.pth"; then echo "install-python-grpc-defaults: ok $d (already current)"; n=$((n+1)); continue; fi
  tmp=$(mktemp "$d/.amux-grpc-fork-safe.XXXXXX") && cp "$src" "$tmp" && mv "$tmp" "$d/amux-grpc-fork-safe.pth"
  echo "install-python-grpc-defaults: installed $d/amux-grpc-fork-safe.pth"; n=$((n+1))
done
echo "install-python-grpc-defaults: ${n} site-packages carry the grpc fork-safe defaults (verdict=python_grpc_defaults_installed)"
