#!/usr/bin/env bash
# Install a validated snapshot of the Bash client, never a symlink into edits.
# Usage: scripts/install-cli.sh [BIN_DIR [SOURCE]]
# SOURCE defaults to this checkout's amux. An amux source includes its grid helper.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin_dir="${1:-${AMUX_INSTALL_BIN:-$HOME/.local/bin}}"
source_cli="${2:-$root/amux}"
destination="$bin_dir/amux"
audit_dir="${AMUX_HOME:-$HOME/.amux}/logs"
candidates=()
source_files=("$source_cli")
destinations=("$destination")
if [[ "$(basename "$source_cli")" == amux ]]; then
  source_files+=("$(dirname "$source_cli")/scripts/amux-grid.sh")
  destinations+=("$bin_dir/scripts/amux-grid.sh")
fi
stage=preflight

log() {
  local line
  line="$(date -u '+%Y-%m-%dT%H:%M:%SZ') $* source=$source_cli destination=$destination"
  printf '%s\n' "$line" >&2
  if ! { mkdir -p "$audit_dir" && printf '%s\n' "$line" >> "$audit_dir/cli-install.log"; } 2>/dev/null; then
    printf '%s\n' 'WARN cli_install_audit_unavailable: installation verdict is on stderr' >&2
  fi
}
finish() {
  local rc=$?
  trap - EXIT
  local path
  for path in "${candidates[@]}"; do rm -f -- "$path"; done
  if [[ "$rc" != 0 ]]; then log "WARN cli_install_failed stage=$stage exit=$rc"; fi
  exit "$rc"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
refuse() { log "WARN cli_install_refused reason=$1"; exit 1; }

[[ $# -le 2 ]] || refuse usage
# Validate the full payload before replacing any installed file. The helper is
# needed because grid resolves scripts/amux-grid.sh beside the installed CLI.
for ((i=0; i<${#source_files[@]}; i++)); do
  source_cli="${source_files[$i]}"
  destination="${destinations[$i]}"
  stage=preflight
  [[ -f "$source_cli" && -s "$source_cli" ]] || refuse missing_or_empty_source
  [[ ! -d "$destination" ]] || refuse destination_is_directory
  # A manually edited, syntax-clean resolution is still unresolved until staged.
  # Do not let an installer choose a side of that unresolved index on its own.
  source_dir="$(cd "$(dirname "$source_cli")" && pwd)"
  if git -C "$source_dir" rev-parse --git-dir >/dev/null 2>&1; then
    unmerged="$(git -C "$source_dir" ls-files -u -- "$(basename "$source_cli")")"
    [[ -z "$unmerged" ]] || refuse unmerged_source
  fi

  stage=snapshot
  mkdir -p "$bin_dir"
  candidate="$(mktemp "$bin_dir/.amux.install.XXXXXX")"
  candidates+=("$candidate")
  cat "$source_cli" > "$candidate"
  # Validate the private snapshot, not a source that can change between checking
  # and copying. Explicit markers also catch otherwise valid quoted/heredoc text.
  stage=validation
  IFS= read -r shebang < "$candidate" || refuse missing_shebang
  case "$shebang" in '#!/usr/bin/env bash'|'#!/bin/bash') ;; *) refuse not_a_bash_script ;; esac
  marker_result=0
  LC_ALL=C grep -aEq '^(<<<<<<<|=======|>>>>>>>|\|\|\|\|\|\|\|)( |$)' "$candidate" || marker_result=$?
  case "$marker_result" in 0) refuse conflict_markers ;; 1) ;; *) refuse marker_check_failed ;; esac
  if ! bash -n "$candidate"; then
    refuse bash_syntax
  fi
  chmod 0755 "$candidate"
  xattr -d com.apple.provenance "$candidate" 2>/dev/null || true
done

# Publish the helper first, so the main entrypoint never appears without it.
for ((i=${#source_files[@]}-1; i>=0; i--)); do
  source_cli="${source_files[$i]}"
  destination="${destinations[$i]}"
  candidate="${candidates[$i]}"
  checksum="$(cksum < "$candidate")"
  stage=publication
  mkdir -p "$(dirname "$destination")"
  # Both names are on the destination filesystem. Replace fails if the
  # destination became a directory; there is no copy/unlink fallback.
  python3 - "$candidate" "$destination" <<'PYRENAME'
import os
import sys
os.replace(sys.argv[1], sys.argv[2])
PYRENAME
  log "INFO cli_install_published checksum=$checksum"
done
log "INFO cli_install_complete files=${#source_files[@]}"
