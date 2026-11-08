#!/usr/bin/env python3
"""Network watch: who can reach this amux host, who did, and what it reaches out to.

Runs as a `kind: shell` schedule, so a quiet run costs no model call. Part of
docs/security/RED-TEAM-LOOP.md (see docs/security/NETWORK-WATCH.md for what each
check means and what it cannot see).

Each run takes one snapshot and compares it with a baseline the first run records:
  listeners  TCP sockets listening on a non-loopback address (LAN and tailnet
             can reach them). A new one is an anomaly.
  tailnet    peers, their owner, sharing, and serve/funnel exposure. A new peer,
             a peer owned by another user, a shared-in node, or any serve or
             funnel config is an anomaly.
  requests   the amux request log since the last run (read-only): a client that
             is neither loopback nor a known tailnet peer, and per-client bursts
             of 401/403.
  firewall   the macOS application firewall state. A change is an anomaly.
  egress     processes holding connections to public addresses. A process name
             not seen before is an anomaly. This is a point-in-time sample: it
             misses short-lived connections and sees no DNS. A shell or raw
             socket tool (bash, nc, socat, ...) is never accepted by name: each
             new destination is its own anomaly, since that is what a reverse
             shell looks like. GitHub's range is exempt for them because git's
             helpers hand their socket to the calling shell.

Auto-fix (--fix, which the schedule passes) is limited to one class that is
always wrong on this host and never load-bearing: an ad-hoc static file server
(`python -m http.server` and friends) listening on a non-loopback address. The
owner's rules forbid starting one. It is stopped with SIGTERM and the stop is
reported. Everything else is reported, never changed: blocking an address,
changing the firewall or tailnet ACLs needs root or the owner.

Fail closed. Exit codes:
  0  the checks ran; anything new was delivered
  2  a probe could not run (lsof, tailscale, the request log). Nothing is
     reported as clean and state is not advanced.
  3  something new was found but the message was not delivered. State is not
     advanced, so the next run tries again.

Request-log fields (paths, user agents) are attacker-controlled and this message
lands in an agent's turn, so only addresses, counts, ports and process names are
sent, each cleaned to a short safe charset.

--dry-run prints what it would send and records nothing. --rebaseline accepts the
current snapshot as normal.
"""

from __future__ import annotations

import json
import os
import re
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

WORKER = os.environ.get("NETWORK_WATCH_WORKER", "amux-security")
STATE = Path(
    os.environ.get(
        "NETWORK_WATCH_STATE", "~/.amux/tmp/amux-security/network-watch"
    )
).expanduser()
DB = Path(os.environ.get("NETWORK_WATCH_DB", "~/.amux/amux.db")).expanduser()
TAILSCALE = os.environ.get("NETWORK_WATCH_TAILSCALE", "tailscale")
AMUX = os.environ.get("NETWORK_WATCH_AMUX", "amux")
# Per client per run window. A dashboard with a stale token polls a few 401s an
# hour (measured 2026-11-04: 154 in 7 days from the owner's laptop), a guesser
# makes thousands.
AUTH_FAIL_BURST = int(os.environ.get("NETWORK_WATCH_AUTH_BURST", "60"))
# Listener command names whose ports are chosen at random per run (e2e servers).
# Matched as prefixes: lsof prints the full name here (amux-server,
# ControlCenter) and the old exact match on 9-character names never fired.
DYNAMIC_PORT_COMMANDS = ("amux-serv", "rapportd", "Spotify", "ControlCe", "identitys")
FILE_SERVER = re.compile(
    r"(-m\s+(http\.server|SimpleHTTPServer)\b|\bhttp-server\b|\bnpx\s+serve\b|\bserve\s+-[ls]\b)"
)


class CouldNotRun(Exception):
    pass


def clean(text: object, n: int = 60) -> str:
    return re.sub(r"[^A-Za-z0-9._:\[\]/@-]", "_", str(text))[:n]


def run(cmd: list[str], timeout: int = 30) -> str:
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError) as exc:
        raise CouldNotRun(f"{cmd[0]}: {exc}") from exc
    # lsof exits 1 when a filter matches nothing; that is an empty answer.
    if p.returncode not in (0, 1) or (p.returncode == 1 and p.stderr.strip()):
        raise CouldNotRun(f"{cmd[0]} exit {p.returncode}: {p.stderr.strip()[:200]}")
    return p.stdout


def is_loopback(addr: str) -> bool:
    return addr.startswith("127.") or addr in ("::1", "[::1]", "localhost")


def is_private(addr: str) -> bool:
    a = addr.strip("[]")
    if is_loopback(a) or a.startswith(("10.", "192.168.", "169.254.", "fe80:", "fd", "fc")):
        return True
    m = re.match(r"(\d+)\.(\d+)\.", a)
    if m:
        x, y = int(m.group(1)), int(m.group(2))
        return (x == 172 and 16 <= y <= 31) or (x == 100 and 64 <= y <= 127)
    return False


# A reverse shell holds its socket in one of these. They are judged per
# destination and never accepted by name.
SHELLS = {"bash", "sh", "zsh", "dash", "ksh", "nc", "ncat", "netcat", "socat",
          "telnet", "perl", "ruby", "php",
          # Launchers: `env X=1 git ...` and friends hold the socket the same way.
          "env", "xargs", "nohup", "timeout", "sudo"}


def is_github(addr: str) -> bool:
    # 140.82.112.0/20, GitHub's git and API front ends (api.github.com/meta).
    m = re.match(r"140\.82\.(\d+)\.\d+$", addr.strip("[]"))
    return bool(m) and 112 <= int(m.group(1)) <= 127


def split_hostport(s: str) -> tuple[str, str]:
    host, _, port = s.rpartition(":")
    return host, port


def norm_command(name: str) -> str:
    # Claude Code names its process after its version (2.1.318).
    return "claude" if re.fullmatch(r"\d+\.\d+\.\d+", name) else name


def parse_lsof(text: str) -> list[dict]:
    """lsof -F pcn output: p<pid>, c<command>, n<name> lines."""
    rows, pid, cmd = [], None, None
    for ln in text.splitlines():
        if not ln:
            continue
        tag, val = ln[0], ln[1:]
        if tag == "p":
            pid, cmd = int(val), None
        elif tag == "c":
            cmd = val
        elif tag == "n" and pid is not None:
            rows.append({"pid": pid, "command": norm_command(cmd or "?"), "name": val})
    return rows


def cmdline(pid: int) -> str:
    try:
        return run(["ps", "-o", "command=", "-p", str(pid)], timeout=10).strip()
    except CouldNotRun:
        return ""


def cwd_of(pid: int) -> str:
    # The working directory names the lane that started a process (its
    # ~/.amux/tmp/<worker> scratch or its worktree), which is gone once the
    # process is stopped.
    try:
        out = run(["lsof", "-a", "-p", str(pid), "-d", "cwd", "-Fn"], timeout=10)
    except CouldNotRun:
        return ""
    return next((x[1:] for x in out.splitlines() if x.startswith("n")), "")


APP_PREFIXES = ("/Applications/", "/System/", "/usr/libexec/", "/usr/sbin/")


def exe_path(pid: int) -> str:
    try:
        return run(["ps", "-o", "comm=", "-p", str(pid)], timeout=10).strip()
    except CouldNotRun:
        return ""


def is_app_bundle(path: str) -> bool:
    return path.startswith(APP_PREFIXES) and (".app/" in path or not path.startswith("/Applications/"))


def collect() -> dict:
    listen = parse_lsof(run(["lsof", "-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcn"]))
    if not listen:
        # amux itself listens, so an empty answer means lsof saw nothing.
        raise CouldNotRun("lsof returned no listeners")
    wide = []
    for r in listen:
        host, port = split_hostport(r["name"])
        if not is_loopback(host.strip("[]")):
            wide.append({**r, "host": host, "port": port})
    est = parse_lsof(run(["lsof", "-nP", "-iTCP", "-sTCP:ESTABLISHED", "-F", "pcn"]))
    egress: dict[str, set[str]] = {}
    paths: dict[int, str] = {}
    for r in est:
        _, _, remote = r["name"].partition("->")
        host, _ = split_hostport(remote)
        if remote and not is_private(host):
            if r["pid"] not in paths:
                paths[r["pid"]] = exe_path(r["pid"])
            # Installed app bundles and the OS reach the internet by design and
            # alerted on first sight with nothing to act on (Obsidian, Spotify).
            # A hijacked agent runs command-line tools, which still alert.
            if is_app_bundle(paths[r["pid"]]):
                continue
            egress.setdefault(r["command"], set()).add(host)
    try:
        ts = json.loads(run([TAILSCALE, "status", "--json"]))
        serve = run([TAILSCALE, "serve", "status", "--json"]).strip()
    except ValueError as exc:
        raise CouldNotRun(f"tailscale status: {exc}") from exc
    me = ts.get("Self") or {}
    peers = {}
    for p in [me, *(ts.get("Peer") or {}).values()]:
        ips = p.get("TailscaleIPs") or []
        peers[p.get("ID") or p.get("PublicKey") or p.get("HostName")] = {
            "host": p.get("DNSName") or p.get("HostName") or "?",
            "ip": ips[0] if ips else "",
            "user": p.get("UserID"),
            "shared": bool(p.get("ShareeNode")),
        }
    serving = serve not in ("", "{}", "null")
    fw = run(["/usr/libexec/ApplicationFirewall/socketfilterfw", "--getglobalstate"]).strip()
    return {
        "wide": wide,
        "egress": egress,
        "peers": peers,
        "self_user": me.get("UserID"),
        "serving": serving,
        "firewall": "on" if "enabled" in fw.lower() else "off" if "disabled" in fw.lower() else fw[:40],
    }


def requests_since(since: float, peer_ips: set[str]) -> dict:
    if not DB.exists():
        raise CouldNotRun(f"request log not found: {DB}")
    try:
        con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True, timeout=10)
        rows = con.execute(
            "select coalesce(client_ip,''), count(*), sum(status in (401,403)), max(ts) "
            "from _amux_request_log where ts > ? group by 1",
            (since,),
        ).fetchall()
        newest = con.execute("select max(ts) from _amux_request_log").fetchone()[0]
        con.close()
    except sqlite3.Error as exc:
        raise CouldNotRun(f"request log: {exc}") from exc
    return {"clients": rows, "newest": newest or since, "peer_ips": peer_ips}


def evaluate(snap: dict, base: dict, reqs: dict) -> list[dict]:
    out = []
    allowed = {(c, p) for c, p in base.get("listeners", [])}
    allowed_cmds = {c for c, _ in base.get("listeners", [])}
    for w in snap["wide"]:
        port = w["port"]
        known = (w["command"], port) in allowed or (
            w["command"].startswith(DYNAMIC_PORT_COMMANDS) and w["command"] in allowed_cmds
        )
        if FILE_SERVER.search(w.get("cmdline", "")):
            out.append({"key": f"fileserver:{w['pid']}:{port}", "kind": "FILE-SERVER", "w": w})
        elif not known:
            out.append({"key": f"listener:{w['command']}:{port}", "kind": "LISTENER", "w": w})
    known_peers = set(base.get("peers", []))
    for pid, p in snap["peers"].items():
        if pid not in known_peers:
            out.append({"key": f"peer:{pid}", "kind": "NEW-PEER", "p": p})
        if p["shared"]:
            out.append({"key": f"shared:{pid}", "kind": "SHARED-PEER", "p": p})
        if snap.get("self_user") and p["user"] and p["user"] != snap["self_user"]:
            out.append({"key": f"foreign:{pid}", "kind": "FOREIGN-PEER", "p": p})
    if snap["serving"]:
        out.append({"key": "serve:on", "kind": "SERVE-ON"})
    if base.get("firewall") and snap["firewall"] != base["firewall"]:
        out.append({"key": f"firewall:{snap['firewall']}", "kind": "FIREWALL", "state": snap["firewall"]})
    known_egress = set(base.get("egress", []))
    for cmd in sorted(snap["egress"]):
        if cmd in SHELLS:
            for addr in sorted(snap["egress"][cmd]):
                if not is_github(addr):
                    out.append({"key": f"egress:{cmd}:{addr}", "kind": "NEW-EGRESS",
                                "cmd": cmd, "n": 1, "to": [addr]})
            continue
        if cmd not in known_egress:
            out.append({"key": f"egress:{cmd}", "kind": "NEW-EGRESS", "cmd": cmd,
                        "n": len(snap["egress"][cmd]), "to": sorted(snap["egress"][cmd])[:3]})
    for ip, n, fails, _ in reqs["clients"]:
        if not is_loopback(ip) and ip not in reqs["peer_ips"]:
            out.append({"key": f"client:{ip}", "kind": "UNKNOWN-CLIENT", "ip": ip, "n": n, "fails": fails})
        if (fails or 0) >= AUTH_FAIL_BURST:
            # Keyed by hour so a continuing burst reports again, at most hourly.
            hour = int(time.time() // 3600)
            out.append({"key": f"burst:{ip}:{hour}", "kind": "AUTH-BURST", "ip": ip, "n": n, "fails": fails})
    return out


def line(r: dict) -> str:
    k = r["kind"]
    if "w" in r:
        w = r["w"]
        s = f"{k} {clean(w['command'])} pid {w['pid']} on {clean(w['host'])}:{clean(w['port'])}"
        s += " (stopped)" if r.get("stopped") else ""
        return s + (f" started in {clean(r['cwd'], 120)}" if r.get("cwd") else "")
    if "p" in r:
        return f"{k} {clean(r['p']['host'])} {clean(r['p']['ip'])}"
    if k == "NEW-EGRESS":
        to = " ".join(clean(a, 45) for a in r.get("to", []))
        return f"{k} {clean(r['cmd'])} to {r['n']} public address(es): {to}"
    if k in ("UNKNOWN-CLIENT", "AUTH-BURST"):
        return f"{k} {clean(r['ip'])}: {r['n']} requests, {r['fails']} refused (401/403)"
    if k == "FIREWALL":
        return f"{k} macOS application firewall is now {clean(r['state'])}"
    return f"{k} tailscale serve or funnel has a config"


def fix(rows: list[dict]) -> None:
    for r in rows:
        if r["kind"] != "FILE-SERVER":
            continue
        r["cwd"] = cwd_of(r["w"]["pid"])
        try:
            os.kill(r["w"]["pid"], signal.SIGTERM)
            r["stopped"] = True
        except OSError:
            r["stopped"] = False


def save(name: str, obj) -> None:
    STATE.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=STATE, prefix=f".{name}.")
    with os.fdopen(fd, "w") as fh:
        json.dump(obj, fh, indent=1, sort_keys=True)
    os.replace(tmp, STATE / name)


def load(name: str, default):
    p = STATE / name
    return json.loads(p.read_text()) if p.exists() else default


def baseline_of(snap: dict) -> dict:
    return {
        "listeners": sorted({(w["command"], w["port"]) for w in snap["wide"]
                             if not FILE_SERVER.search(w.get("cmdline", ""))}),
        "peers": sorted(snap["peers"]),
        "egress": sorted(snap["egress"]),
        "firewall": snap["firewall"],
    }


# An alerted key stays quiet this long after it was last present, so a dev
# server restarted every few minutes on the same wide port alerts once a day
# instead of on every return. Keys that carry a pid (file servers) still alert
# for each new process.
SEEN_TTL = int(os.environ.get("NETWORK_WATCH_SEEN_TTL", str(24 * 3600)))


def remember(seen, keys, now: float) -> dict:
    """Seen keys still inside the TTL, with the present ones stamped now."""
    if isinstance(seen, list):  # older state: a bare list of keys
        seen = {k: now for k in seen}
    kept = {k: t for k, t in seen.items() if now - t < SEEN_TTL}
    kept.update({k: now for k in keys})
    return kept


def main() -> int:
    dry, do_fix = "--dry-run" in sys.argv, "--fix" in sys.argv
    try:
        base = load("baseline.json", None)
        seen = remember(load("seen.json", {}), [], time.time())
        cursor = load("cursor.json", {}).get("ts", time.time() - 6 * 3600)
        snap = collect()
        for w in snap["wide"]:
            w["cmdline"] = cmdline(w["pid"])
        peer_ips = {p["ip"] for p in snap["peers"].values() if p["ip"]}
        reqs = requests_since(cursor, peer_ips)
    except (CouldNotRun, ValueError, OSError) as exc:
        print(f"network watch could not run: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 2
    first = base is None or "--rebaseline" in sys.argv
    if first:
        base = baseline_of(snap)
    rows = evaluate(snap, base, reqs)
    fresh = [r for r in rows if r["key"] not in seen]
    if do_fix and not dry:
        fix(fresh)
    print(
        f"{len(snap['wide'])} wide listeners, {len(snap['peers'])} tailnet nodes, "
        f"{len(snap['egress'])} processes with public egress, firewall {snap['firewall']}, "
        f"{len(reqs['clients'])} clients in the request window, "
        f"{len(rows)} anomalies, {len(fresh)} unseen" + (" (baseline recorded)" if first else "")
    )
    for r in fresh:
        print("  " + line(r))
    if fresh:
        body = "Network watch: new anomalies on this amux host.\n" + "\n".join(
            line(r) for r in fresh[:30]
        )
        if len(fresh) > 30:
            body += f"\n... and {len(fresh) - 30} more"
        body += "\nTriage per docs/security/NETWORK-WATCH.md; --rebaseline once one is accepted as normal."
        if dry:
            print(body)
            return 0
        try:
            p = subprocess.run([AMUX, "send", WORKER, "--no-board", "--stdin"], input=body,
                               text=True, capture_output=True, timeout=60)
            sent, why = p.returncode == 0, p.stderr.strip()[:200]
        except (OSError, subprocess.SubprocessError) as exc:
            sent, why = False, str(exc)
        if not sent:
            print(f"network watch found {len(fresh)} but could not deliver: {why}", file=sys.stderr)
            return 3
    if not dry:
        if first:
            save("baseline.json", base)
        save("seen.json", remember(seen, [r["key"] for r in rows], time.time()))
        save("cursor.json", {"ts": reqs["newest"]})
    return 0


if __name__ == "__main__":
    sys.exit(main())
