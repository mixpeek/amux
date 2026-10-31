#!/usr/bin/env bash
# Lock down a dedicated amux host. Run as root by bootstrap.sh and by update.sh on every update, so
# it is idempotent and a re-run refreshes Cloudflare's address list. Ethan, 2026-10-31: "they should
# all be behind cloudflare ... and have standard security stuff so nobody can get in".
#
# Always:
#   - SSH: no root password, 3 tries per connection, no X11; fail2ban bans guessers. Password
#     sign-in is bootstrap.sh's call (00-amux-no-password.conf, read first, so it wins).
#   - unattended security upgrades.
#   - Caddy hands the door the visitor's real address as X-Real-IP ({client_ip}), overwriting
#     anything the client sent, so the door's wrong-password throttle counts the right address.
# When the host is behind Cloudflare: /etc/amux/edge says "cloudflare" (bootstrap.sh writes it for
# AMUX_BEHIND_CLOUDFLARE=1), or, with no such file, the site's name resolves only to Cloudflare
# addresses. So proxying a host's DNS record is enough: its next update locks the origin, and an
# un-proxied record opens it again (2026-10-31: TubeScience and every new amux Cloud server).
#   - ufw admits 80, 443 and 8443 only from Cloudflare's published ranges. Nobody reaches the web
#     server except through Cloudflare. SSH (22) and tailscale0 are unchanged.
#   - Caddy trusts Cloudflare's addresses for CF-Connecting-IP, and only theirs.
# A failed download of the ranges keeps the last good list; with no list at all the web ports are
# left as they are rather than closed. Every Caddy change is validated and rolled back if rejected.
# One summary line: verdict=edge_hardened ... (journalctl -t amux-edge).
set -uo pipefail
say() { logger -t amux-edge -- "$*"; echo "$*"; }
CF=/etc/caddy/Caddyfile
LIST=/etc/amux/cloudflare-ips
notes=()

# ── SSH ──
SSHD=/etc/ssh/sshd_config.d/01-amux-harden.conf
want_sshd=$'# amux cloud/desktop/edge-harden.sh\nPermitRootLogin prohibit-password\nMaxAuthTries 3\nLoginGraceTime 30\nX11Forwarding no\n'
if [[ "$(cat "$SSHD" 2>/dev/null)"$'\n' != "$want_sshd" ]]; then
  install -d /etc/ssh/sshd_config.d
  printf '%s' "$want_sshd" > "$SSHD.new"
  if mv "$SSHD.new" "$SSHD" && sshd -t 2>/dev/null; then
    systemctl reload ssh 2>/dev/null || systemctl reload sshd 2>/dev/null || true; notes+=("sshd hardened")
  else
    rm -f "$SSHD"; say "WARN verdict=edge_sshd_rejected sshd -t failed; drop-in removed"
  fi
fi

# ── fail2ban + unattended upgrades ──
pkgs=()
command -v fail2ban-client >/dev/null || pkgs+=(fail2ban)
dpkg -s unattended-upgrades >/dev/null 2>&1 || pkgs+=(unattended-upgrades)
if [[ ${#pkgs[@]} -gt 0 ]]; then
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${pkgs[@]}" >/dev/null 2>&1 \
    && notes+=("installed ${pkgs[*]}") || say "WARN verdict=edge_install_failed packages=${pkgs[*]}"
fi
if command -v fail2ban-client >/dev/null; then
  J=/etc/fail2ban/jail.d/amux-sshd.local
  want_jail=$'[sshd]\nenabled = true\nmaxretry = 5\nfindtime = 10m\nbantime = 1h\nbantime.increment = true\nignoreip = 127.0.0.1/8 ::1 100.64.0.0/10\n'
  if [[ "$(cat "$J" 2>/dev/null)"$'\n' != "$want_jail" ]]; then
    printf '%s' "$want_jail" > "$J"; systemctl restart fail2ban >/dev/null 2>&1; notes+=("fail2ban sshd jail")
  fi
  systemctl is-active -q fail2ban || systemctl enable --now fail2ban >/dev/null 2>&1
fi
U=/etc/apt/apt.conf.d/20auto-upgrades
want_u=$'APT::Periodic::Update-Package-Lists "1";\nAPT::Periodic::Unattended-Upgrade "1";\n'
[[ "$(cat "$U" 2>/dev/null)"$'\n' == "$want_u" ]] || { printf '%s' "$want_u" > "$U"; notes+=("unattended upgrades on"); }

# ── Cloudflare ranges (always fetched: the detection below needs them) ──
ranges=()
install -d /etc/amux; tmp=$(mktemp)
if curl -fsS --max-time 20 https://www.cloudflare.com/ips-v4 > "$tmp" && echo >> "$tmp" \
   && curl -fsS --max-time 20 https://www.cloudflare.com/ips-v6 >> "$tmp" \
   && [[ $(grep -cE '^[0-9a-f:.]+/[0-9]+$' "$tmp") -ge 10 ]]; then
  grep -E '^[0-9a-f:.]+/[0-9]+$' "$tmp" | sort -u > "$LIST.new" && install -d /etc/amux && mv "$LIST.new" "$LIST"
else
  say "WARN verdict=edge_cloudflare_ranges_unavailable kept=$( [[ -s $LIST ]] && wc -l < $LIST || echo 0 )"
fi
rm -f "$tmp"

# ── Behind Cloudflare? ──
DETECTED=/etc/amux/edge-detected
if [[ -s /etc/amux/edge ]]; then
  EDGE=$(cat /etc/amux/edge); how=configured
else
  site=$(grep -m1 -oE '^[A-Za-z0-9.-]+\.[A-Za-z]{2,} *\{' "$CF" 2>/dev/null | tr -d ' {')
  EDGE=$(python3 - "$site" "$LIST" <<'EOF2'
import ipaddress, socket, sys
site, listf = sys.argv[1], sys.argv[2]
try:
    nets = [ipaddress.ip_network(l.strip()) for l in open(listf) if l.strip()]
    addrs = {a[4][0] for a in socket.getaddrinfo(site, 443, proto=socket.IPPROTO_TCP)} if site else set()
except (OSError, ValueError):
    print("unknown"); sys.exit()
if not addrs or not nets:
    print("unknown")
else:
    print("cloudflare" if all(any(ipaddress.ip_address(a) in n for n in nets) for a in addrs) else "direct")
EOF2
)
  how=detected
  if [[ "$EDGE" == unknown ]]; then EDGE=$(cat "$DETECTED" 2>/dev/null || echo direct); how=last_detected
  else install -d /etc/amux; echo "$EDGE" > "$DETECTED"; fi
fi
[[ "$EDGE" == cloudflare && -s "$LIST" ]] && mapfile -t ranges < "$LIST"

# ── ufw: web ports from Cloudflare only ──
if [[ ${#ranges[@]} -gt 0 ]] && command -v ufw >/dev/null; then
  have=$(ufw status 2>/dev/null)
  for r in "${ranges[@]}"; do
    grep -qE "^80,443,8443/tcp +ALLOW +$r( |$)" <<< "$have" \
      || ufw allow proto tcp from "$r" to any port 80,443,8443 comment amux-cloudflare >/dev/null
  done
  # Only once every Cloudflare rule is in: close the open-to-anyone web rules, and drop ranges
  # Cloudflare no longer lists.
  for p in 80/tcp 443/tcp 8443/tcp; do
    while ufw status 2>/dev/null | grep -qE "^$p( \(v6\))? +ALLOW +Anywhere"; do ufw --force delete allow "$p" >/dev/null || break; done
  done
  ufw status 2>/dev/null | grep -E '^80,443,8443/tcp +ALLOW +[0-9a-f:.]+/[0-9]+ +# amux-cloudflare' | awk '{print $3}' | while read -r r; do
    grep -qxF "$r" "$LIST" || ufw --force delete allow proto tcp from "$r" to any port 80,443,8443 >/dev/null
  done
  notes+=("web from cloudflare only (${#ranges[@]} ranges)")
fi

# Not behind Cloudflare (any more): web ports open to anyone again, Cloudflare-only rules removed.
if [[ "$EDGE" != cloudflare ]] && command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q amux-cloudflare; then
  ufw allow 80/tcp >/dev/null; ufw allow 443/tcp >/dev/null; ufw allow 8443/tcp >/dev/null
  ufw status 2>/dev/null | grep -E '^80,443,8443/tcp +ALLOW +[0-9a-f:.]+/[0-9]+ +# amux-cloudflare' | awk '{print $3}' | while read -r r; do
    ufw --force delete allow proto tcp from "$r" to any port 80,443,8443 >/dev/null
  done
  notes+=("web open to anyone again (not behind cloudflare)")
fi

# ── Caddy: the visitor's real address ──
if [[ -f $CF ]]; then
  cp -p $CF $CF.edge-prev
  python3 - "$CF" "$LIST" "$EDGE" <<'EOF'
import re, sys
path, listf, edge = sys.argv[1:4]
s = open(path).read()
# Global options: trust Cloudflare for CF-Connecting-IP. Kept between markers so a re-run replaces it.
s = re.sub(r"# amux-edge begin\n.*?# amux-edge end\n", "", s, flags=re.S)
try:
    ranges = open(listf).read().split() if edge == "cloudflare" else []
except OSError:   # no list yet: still hand the door the real address below
    ranges = []
if ranges:
    if re.match(r"\s*(#.*\n\s*)*\{", s):
        print("WARN: Caddyfile already has a global options block; Cloudflare trust not added", file=sys.stderr)
    else:
        s = ("# amux-edge begin\n{\n\tservers {\n\t\ttrusted_proxies static " + " ".join(ranges) +
             "\n\t\tclient_ip_headers CF-Connecting-IP\n\t}\n}\n# amux-edge end\n") + s
# The door gets the visitor's address as X-Real-IP, overwriting a client's own.
s = s.replace("    reverse_proxy 127.0.0.1:8890\n", "    reverse_proxy 127.0.0.1:8890 {\n      header_up X-Real-IP {client_ip}\n    }\n")
s = re.sub(r"(forward_auth 127\.0\.0\.1:8890 \{\n)(?!\s*header_up X-Real-IP)", r"\1      header_up X-Real-IP {client_ip}\n", s)
open(path, "w").write(s)
EOF
  if cmp -s $CF $CF.edge-prev; then
    rm -f $CF.edge-prev
  elif runuser -u caddy -- caddy validate --config $CF --adapter caddyfile >/dev/null 2>&1 && systemctl reload caddy; then
    rm -f $CF.edge-prev; notes+=("caddy client address")
  else
    mv $CF.edge-prev $CF; systemctl reload caddy; say "WARN verdict=edge_caddy_rejected previous Caddyfile kept"
  fi
fi

web_open=$(ufw status 2>/dev/null | grep -cE '^(80|443|8443)/tcp( \(v6\))? +ALLOW +Anywhere' || true)
say "verdict=edge_hardened edge=$EDGE edge_from=$how cloudflare_ranges=${#ranges[@]} web_rules_open_to_anyone=$web_open changes=\"${notes[*]:-none}\""
