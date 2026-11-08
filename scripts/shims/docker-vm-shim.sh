#!/bin/bash
# amux docker-vm shim (DESKT-72). Installed by the amux server as both `colima`
# and `limactl` in ~/.amux/shims/docker-vm/, and put first on a worker's PATH
# when AMUX_DOCKER_VM=<profile> is set at the worker's worker, group or global
# scope. The same server code sets DOCKER_CONTEXT=colima-<profile>.
#
# Why: on 2026-10-01 eight lanes had each started a private colima VM (16 to
# 32G apiece). With five running the Mac reached load 116 on 28 cores, memory
# pressure 2 and 10G of free disk, and each VM held its own copy of the same
# Docker build cache. A rule in one repo's brief reached only the lanes that read
# it; this reaches every worker of every provider whose scope sets the knob.
#
# What it does: `colima start|create` of any profile other than the shared one,
# and `limactl start|create` of any instance other than colima-<profile>, is
# refused with a message that names the shared VM and the knob, and the refusal
# is logged. Everything else (status, list, ssh, stop, the shared VM itself)
# goes straight to the real binary. AMUX_DOCKER_VM_ENFORCE=0 at a scope turns
# the refusal off for that worker and keeps the routing.
set -u
me=$(basename "$0")
shimdir=$(cd "$(dirname "$0")" && pwd -P)

# ANOTHER COPY OF AN AMUX SHIM IS NOT THE REAL BINARY (cpu RCA 20261030-065821).
# Skipping only this directory let two copies on one PATH (~/.amux/shims and a
# demo home's shims) each take the other for the real binary and exec it back
# and forth: one `pgrep` ran that way for 7h44m and the Mac allocated ~875
# pids a second. A candidate that is a script mentioning amux is a shim copy,
# whatever its age; real binaries are not scripts.
_amux_shim_copy() { # <path>
  local first=""
  IFS= read -r first < "$1" 2>/dev/null || return 1
  case "$first" in '#!'*) grep -qs 'amux' "$1" ;; *) return 1 ;; esac
}
# The real binary is the first one on PATH that is not in this directory.
real=""
IFS=: read -r -a dirs <<< "${PATH:-}"
for d in "${dirs[@]}"; do
  [ -n "$d" ] || continue
  dd=$(cd "$d" 2>/dev/null && pwd -P) || continue
  [ "$dd" = "$shimdir" ] && continue
  if [ -x "$d/$me" ] && ! _amux_shim_copy "$d/$me"; then real="$d/$me"; break; fi
done
if [ -z "$real" ]; then
  echo "amux docker-vm shim: no real $me found on PATH" >&2
  exit 127
fi

want=${AMUX_DOCKER_VM:-}
if [ -z "$want" ] || [ "${AMUX_DOCKER_VM_ENFORCE:-1}" = 0 ]; then
  exec "$real" "$@"
fi

refuse() { # <what was asked>
  local log="${AMUX_HOME:-$HOME/.amux}/logs/docker-vm-refusals.log"
  mkdir -p "$(dirname "$log")" 2>/dev/null
  printf '%s session=%s refused=%q\n' "$(date '+%F %T')" "${AMUX_SESSION:-unknown}" "$1" >> "$log" 2>/dev/null
  cat >&2 <<MSG
amux: refused \`$1\`. This worker shares one Docker VM, colima profile '$want'
(AMUX_DOCKER_VM, set at its worker, group or global scope in amux).

Use the shared VM instead:
  colima start -p $want        # if it is not already running
  docker ...                   # DOCKER_CONTEXT is already colima-$want
Keep your stack apart with a Compose project: docker compose -p <your-lane> ...

If this worker really needs its own VM, set AMUX_DOCKER_VM_ENFORCE=0 at its
scope and restart it.
MSG
  exit 64
}

sub=${1:-}
case "$me:$sub" in
  colima:start|colima:create)
    profile=""
    # `colima start [profile]` takes the profile as the first positional.
    if [ "$#" -ge 2 ] && [ "${2#-}" = "$2" ]; then profile=$2; fi
    prev=""
    for a in "$@"; do
      case "$prev" in -p|--profile) profile=$a ;; esac
      case "$a" in --profile=*) profile=${a#--profile=} ;; esac
      prev=$a
    done
    [ -n "$profile" ] || profile=default
    [ "$profile" = "$want" ] || refuse "colima $sub (profile $profile)"
    ;;
  limactl:start|limactl:create)
    inst=""
    for a in "${@:2}"; do
      case "$a" in --name=*) inst=${a#--name=} ;; -*) ;; *) [ -n "$inst" ] || inst=$a ;; esac
    done
    inst=$(basename "${inst%.yaml}")
    [ "$inst" = "colima-$want" ] || refuse "limactl $sub ${inst:-<unnamed>}"
    ;;
esac
exec "$real" "$@"
