#!/usr/bin/env bash
# sysmon.sh — writes a live system-utilization markdown file every 5 seconds.
# Pin the output file as a floating overlay for a menu-bar-style monitor.
#
# Usage: scripts/sysmon.sh [output_path]
# Default output: ~/.amux/local/System-Monitor.md

OUT="${1:-$HOME/.amux/local/System-Monitor.md}"
mkdir -p "$(dirname "$OUT")"

bar() {
  local pct="${1:-0}" width=30 fill empty i result
  if (( pct > 100 )); then pct=100; fi
  if (( pct < 0 )); then pct=0; fi
  fill=$(( pct * width / 100 ))
  empty=$(( width - fill ))
  result='`'
  for (( i=0; i<fill; i++ )); do result+='█'; done
  for (( i=0; i<empty; i++ )); do result+='░'; done
  result+='`'
  printf '%s' "$result"
}

hb() {
  local bytes="${1:-0}"
  if (( bytes >= 1073741824 )); then
    printf "%.1f GB" "$(echo "scale=1; $bytes / 1073741824" | bc)"
  elif (( bytes >= 1048576 )); then
    printf "%.0f MB" "$(echo "scale=0; $bytes / 1048576" | bc)"
  else
    printf "%.0f KB" "$(echo "scale=0; $bytes / 1024" | bc)"
  fi
}

vmf() {
  echo "$_vm" | awk "/$1/"'{split($0,a,":"); gsub(/[^0-9]/,"",a[2]); print a[2]}'
}

while true; do
  {
    ts=$(date '+%H:%M:%S')

    # ── CPU ──
    cpu_line=$(top -l 1 -n 0 2>/dev/null | grep "CPU usage" || echo "")
    cpu_user=$(echo "$cpu_line" | sed -n 's/.*: \([0-9.]*\)% user.*/\1/p')
    cpu_sys=$(echo "$cpu_line" | sed -n 's/.*user, \([0-9.]*\)% sys.*/\1/p')
    cpu_idle=$(echo "$cpu_line" | sed -n 's/.*sys, \([0-9.]*\)% idle.*/\1/p')
    cpu_used=$(echo "${cpu_user:-0} + ${cpu_sys:-0}" | bc 2>/dev/null || echo 0)
    cpu_pct=$(printf '%.0f' "$cpu_used" 2>/dev/null || echo 0)

    # ── Memory ──
    _vm=$(vm_stat 2>/dev/null || echo "")
    page_size=$(sysctl -n hw.pagesize 2>/dev/null || echo 16384)
    total_mem=$(sysctl -n hw.memsize 2>/dev/null || echo 1)
    pf=$(vmf 'Pages free'); pf=${pf:-0}
    pa=$(vmf 'Pages active'); pa=${pa:-0}
    pi=$(vmf 'Pages inactive'); pi=${pi:-0}
    pw=$(vmf 'Pages wired'); pw=${pw:-0}
    pc=$(vmf 'Pages stored in compressor'); pc=${pc:-0}

    used_bytes=$(( (pa + pw + pc) * page_size ))
    free_bytes=$(( pf * page_size ))
    inactive_bytes=$(( pi * page_size ))
    wired_bytes=$(( pw * page_size ))
    compressed_bytes=$(( pc * page_size ))
    active_bytes=$(( pa * page_size ))
    mem_pct=$(( used_bytes * 100 / total_mem ))

    # ── Disk ──
    disk_line=$(df -h / 2>/dev/null | tail -1)
    disk_size=$(echo "$disk_line" | awk '{print $2}')
    disk_used=$(echo "$disk_line" | awk '{print $3}')
    disk_avail=$(echo "$disk_line" | awk '{print $4}')
    disk_pct=$(echo "$disk_line" | awk '{gsub(/%/,"",$5); print $5}')
    disk_pct=${disk_pct:-0}

    # ── Load ──
    loadavg=$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}' | xargs || echo "n/a")

    # ── Swap ──
    swap_used=$(sysctl -n vm.swapusage 2>/dev/null | sed -n 's/.*used = \([0-9.]*\)M.*/\1/p' || echo "")
    swap_total=$(sysctl -n vm.swapusage 2>/dev/null | sed -n 's/.*total = \([0-9.]*\)M.*/\1/p' || echo "")

    # ── Network (first active interface with traffic) ──
    net_iface=""
    net_in=""
    net_out=""
    while read -r iface ib ob; do
      if [[ -n "$ib" && "$ib" != "0" ]]; then
        net_iface="$iface"
        net_in="$ib"
        net_out="$ob"
        break
      fi
    done < <(netstat -ib 2>/dev/null | awk '/Link/ && $7+0>0 {print $1, $7, $10}')

    # ── Render ──
    echo "# System Monitor"
    echo ""
    echo "**${ts}** | load: ${loadavg}"
    echo ""

    # CPU
    echo "## CPU  ${cpu_pct}%"
    echo ""
    echo "$(bar "$cpu_pct")  **${cpu_pct}%**"
    echo ""
    echo "| | |"
    echo "|---|---|"
    echo "| user | **${cpu_user:-0}%** |"
    echo "| system | **${cpu_sys:-0}%** |"
    echo "| idle | **${cpu_idle:-0}%** |"
    echo ""
    echo "### Top Processes (CPU)"
    echo ""
    echo "| %CPU | Process |"
    echo "|---:|---|"
    ps -Arco %cpu,comm 2>/dev/null | head -9 | tail -8 | while read -r pct name; do
      if [[ -n "$pct" && -n "$name" ]]; then
        echo "| ${pct} | ${name} |"
      fi
    done
    echo ""

    # Memory
    echo "## Memory  ${mem_pct}%"
    echo ""
    echo "$(bar "$mem_pct")  **$(hb "$used_bytes")** / $(hb "$total_mem")"
    echo ""
    echo "| | |"
    echo "|---|---|"
    echo "| App (active) | **$(hb "$active_bytes")** |"
    echo "| Wired | **$(hb "$wired_bytes")** |"
    echo "| Compressed | **$(hb "$compressed_bytes")** |"
    echo "| Free | $(hb "$free_bytes") |"
    echo "| Inactive | $(hb "$inactive_bytes") |"
    if [[ -n "$swap_used" ]]; then
      echo "| Swap used | ${swap_used}M / ${swap_total}M |"
    fi
    echo ""
    echo "### Top Processes (Memory)"
    echo ""
    echo "| RSS | Process |"
    echo "|---:|---|"
    ps -Amro rss,comm 2>/dev/null | head -9 | tail -8 | while read -r rss name; do
      if [[ -n "$rss" && -n "$name" ]]; then
        short=$(basename "$name" 2>/dev/null || echo "$name")
        echo "| $(hb $(( rss * 1024 )) ) | ${short} |"
      fi
    done
    echo ""

    # Disk
    echo "## Disk  ${disk_pct}%"
    echo ""
    echo "$(bar "$disk_pct")  **${disk_used:-?}** / ${disk_size:-?} (${disk_avail:-?} free)"
    echo ""
    echo "### Volumes"
    echo ""
    echo "| Used | Capacity | Mount |"
    echo "|---:|---:|---|"
    df -h 2>/dev/null | awk 'NR>1 && $1 !~ /^devfs|^map/ {print $3 "\t" $2 "\t" $9}' | head -6 | while IFS=$'\t' read -r used cap mount; do
      if [[ -n "$used" ]]; then
        echo "| ${used} | ${cap} | ${mount} |"
      fi
    done
    echo ""

    # Network
    if [[ -n "$net_in" && -n "$net_out" ]]; then
      echo "## Network (${net_iface:-?})"
      echo ""
      echo "| | |"
      echo "|---|---|"
      echo "| In (total) | **$(hb "${net_in}")** |"
      echo "| Out (total) | **$(hb "${net_out}")** |"
    fi

  } > "${OUT}.tmp" 2>/dev/null
  mv "${OUT}.tmp" "$OUT"

  sleep 5
done
