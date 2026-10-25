#!/bin/bash
# The .pth installed by install-python-grpc-defaults.sh sets the grpc fork-safe
# defaults in any interpreter that reads it, and never overrides a caller's value.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
T=$(mktemp -d); trap 'rm -rf -- "${T:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
command -v python3 >/dev/null || { echo "FAIL: python3 missing, no cell ran"; exit 1; }
mkdir -p "$T/site"
"$HERE/install-python-grpc-defaults.sh" "$T/site" > "$T/out"
check "installer names the verdict" yes "$(grep -q 'verdict=python_grpc_defaults_installed' "$T/out" && echo yes || echo no)"
check "installed" yes "$( [ -f "$T/site/amux-grpc-fork-safe.pth" ] && echo yes || echo no)"
probe() { env -u GRPC_ENABLE_FORK_SUPPORT -u GRPC_DNS_RESOLVER "$@" python3 -c "import site,os; site.addsitedir('$T/site'); print(os.environ.get('GRPC_ENABLE_FORK_SUPPORT'), os.environ.get('GRPC_DNS_RESOLVER'))"; }
check "an interpreter that reads it gets the defaults" "false native" "$(probe)"
check "a caller's value wins" "true ares" "$(probe GRPC_ENABLE_FORK_SUPPORT=true GRPC_DNS_RESOLVER=ares)"
check "a child process inherits them (the fork path)" "false native" "$(env -u GRPC_ENABLE_FORK_SUPPORT -u GRPC_DNS_RESOLVER python3 -c "import site,subprocess; site.addsitedir('$T/site'); print(subprocess.run(['sh','-c','printf \"%s %s\" \"\$GRPC_ENABLE_FORK_SUPPORT\" \"\$GRPC_DNS_RESOLVER\"'],capture_output=True,text=True).stdout)")"
"$HERE/install-python-grpc-defaults.sh" "$T/site" > "$T/out2"
check "a second install is a no-op" yes "$(grep -q 'already current' "$T/out2" && echo yes || echo no)"
[ "$fails" -eq 0 ] && echo "PASS: python grpc fork-safe defaults" || { echo "FAIL: $fails cell(s)"; exit 1; }
