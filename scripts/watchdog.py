#!/usr/bin/env python3
"""
amux service watchdog — EXTERNAL liveness for the rust server (com.amux.server-rs).

# Why this exists at all, given launchd and the in-process invariant monitor

Three things watch the server, and they cover different failures. Keeping them
straight is the whole design:

  * **launchd `KeepAlive`** (com.amux.server-rs.plist) normally restarts a
    process that EXITS. It cannot see a process that is alive and wedged, and a
    newly replaced ad-hoc-signed binary can leave it stuck at EX_CONFIG until
    the agent registration is refreshed.
  * **The in-process invariant monitor** (`crates/amux-server/src/invariants/`)
    checks SEAMS on a 30s tick: route contract, config provenance, queue
    liveness. It is excellent at "two subsystems disagree" and structurally
    blind to "this process is dead", because if the process wedges the monitor
    wedges with it. A monitor cannot report its own death.
  * **This file** covers both gaps: a process is ALIVE but does not answer
    `/health`, or launchd has failed to restore a dead listener. Both failure
    modes have occurred on this host.

So the watchdog is retargeted, not retired (AMUX-2618). What it is NOT allowed
to do is anything the other two already do, or anything a human should decide.

# What changed on 2026-08-09 (the cutover fix)

The previous version supervised the RETIRED python service and had fired as
recently as 19:42 that day: it kickstarted `com.amux.server` (a label that no
longer resolves), decided that had failed, and then invoked
`claude --print --dangerously-skip-permissions` with a prompt instructing it to
edit `amux-server.py` (deleted), commit, and **`git push origin main`** — on a
SHARED checkout carrying other sessions' unpushed commits. It survived only
because that invocation happened to die on a signal. An unattended agent with a
push instruction is not a watchdog, it is a deploy with no operator, and it is
exactly the call CLAUDE.md's Deploy section reserves for a human. That path is
deleted, not fixed.

Deleted with it: the CPU and memory branches. `/health` on the rust server
serves no `cpu_percent`/`memory_mb`, so both read absent keys as 0 and could
never fire — and the CPU trigger had already been disabled once for tripping on
a baseline (ethos rule 7: a threshold below the baseline reports that the
machine is ON). What remains is only signals that are ABSENT in the healthy
state.

# The refused/hung split

`connection refused` and `connected but silent` are different faults with
different owners, and collapsing them is what made this thing noisy:

  * **refused** — nothing is listening. Short self-adoption gaps are normal, but
    two consecutive observations mean KeepAlive has demonstrably failed. The
    watchdog first kickstarts the exact service and, if macOS still reports a
    dead service, reloads that service's exact plist before escalating.
  * **hung / degraded** — the port answers but `/health` does not, or answers
    `store != ok`. Nobody else can see this. This is the one case we act on.

# Exercising the failure path

A watchdog nobody has watched fire is theatre. `WATCHDOG_DRY_RUN=1` logs the
action it WOULD take instead of taking it, and `WATCHDOG_HEALTH_URL` points it
at an arbitrary endpoint, so all three verdicts can be provoked against a fake
server without touching the live one. See the AMUX-2618 verification in the
card. Keep that affordance working.
"""

import json
import os
import socket
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime
from pathlib import Path
from urllib.parse import urlparse

IS_MACOS = sys.platform == "darwin"
IS_DOCKER = Path("/.dockerenv").exists()

# The rust server binds BOTH 8824 (AMUX_RS_PORT) and the retired 8822 from one
# process — verified live: pid 8831 held both. Probing 8824 is therefore
# sufficient, and it is the port the launchd agent actually configures and the
# only address anything new should use, so it is the honest thing to
# health-check. (8822's bind is a countdown for pre-cutover processes; see
# docs/rust-migration/server-boundary.md.)
AMUX_PORT = int(os.environ.get("AMUX_RS_PORT", os.environ.get("AMUX_PORT", 8824)))
_SCHEME = "https" if IS_MACOS else "http"
HEALTH_URL = os.environ.get("WATCHDOG_HEALTH_URL", f"{_SCHEME}://localhost:{AMUX_PORT}/health")
LAUNCHD_LABEL = os.environ.get("WATCHDOG_LAUNCHD_LABEL", "com.amux.server-rs")
LAUNCHD_PLIST = Path(
    os.environ.get(
        "WATCHDOG_LAUNCHD_PLIST",
        str(Path.home() / "Library" / "LaunchAgents" / f"{LAUNCHD_LABEL}.plist"),
    )
)
DRY_RUN = os.environ.get("WATCHDOG_DRY_RUN") == "1"

LOG_FILE = Path(os.environ.get("WATCHDOG_LOG", str(Path.home() / ".amux" / "logs" / "watchdog.log")))
SERVER_LOG = Path.home() / ".amux" / "logs" / "server-rs.log"
DIAG_DIR = Path.home() / ".amux" / "logs"
SERVER_BIN_NAME = "amux-server-rs"

CHECK_INTERVAL = int(os.environ.get("WATCHDOG_INTERVAL", 30))
# Consecutive HUNG observations before we kickstart. 3 x 30s = 90s of a process
# holding the port and not answering.
HUNG_THRESHOLD = int(os.environ.get("WATCHDOG_HUNG_THRESHOLD", 3))
# Consecutive DOWN (refused) observations before recovery. Two 30s observations
# ignore the ordinary sub-5s self-adoption gap but cap a client-visible outage at
# roughly one minute instead of the old ten-minute diagnostic-only path.
DOWN_ESCALATE = int(os.environ.get("WATCHDOG_DOWN_ESCALATE", 2))
# Don't kickstart more than once per this window — a crash-looping service is
# not fixed by restarting it faster, and the restart storm looks like the fault.
KICKSTART_COOLDOWN = int(os.environ.get("WATCHDOG_KICKSTART_COOLDOWN", 300))
DIAG_COOLDOWN = int(os.environ.get("WATCHDOG_DIAG_COOLDOWN", 900))
MAX_LOG_BYTES = int(os.environ.get("WATCHDOG_MAX_LOG_BYTES", 2_000_000))

CONNECT_TIMEOUT = 3
HEALTH_TIMEOUT = 10

_ssl_ctx = ssl.create_default_context()
_ssl_ctx.check_hostname = False
_ssl_ctx.verify_mode = ssl.CERT_NONE

_consecutive_hung = 0
_consecutive_down = 0
_last_kickstart = 0.0
_last_diag = 0.0
# Whether launchd has already redirected our stderr INTO the log file. If it
# has, writing the line to both doubles every entry — which is why the old log
# showed every message twice and read like two watchdogs were running. Resolved
# once at startup.
_stderr_is_logfile = False


def _resolve_log_duplication():
    """True when stderr and LOG_FILE are the same inode (launchd redirect)."""
    global _stderr_is_logfile
    try:
        LOG_FILE.parent.mkdir(parents=True, exist_ok=True)
        LOG_FILE.touch(exist_ok=True)
        st_err = os.fstat(sys.stderr.fileno())
        st_log = LOG_FILE.stat()
        _stderr_is_logfile = st_err.st_dev == st_log.st_dev and st_err.st_ino == st_log.st_ino
    except Exception:
        _stderr_is_logfile = False


def _rotate_if_large():
    """Keep the tail in place. The launchd redirect holds this file open in
    append mode, so truncating under it is safe; renaming is NOT (launchd would
    keep writing to the renamed inode and the visible log would stay empty)."""
    try:
        if LOG_FILE.stat().st_size <= MAX_LOG_BYTES:
            return
        keep = MAX_LOG_BYTES // 2
        with open(LOG_FILE, "r+b") as f:
            f.seek(-keep, os.SEEK_END)
            tail = f.read()
            f.seek(0)
            f.write(b"[watchdog] --- log truncated ---\n" + tail)
            f.truncate()
    except Exception:
        pass


def log(msg: str):
    line = f"{datetime.now().strftime('%Y-%m-%d %H:%M:%S')} [watchdog] {msg}\n"
    try:
        sys.stderr.write(line)
        sys.stderr.flush()
    except Exception:
        pass
    if _stderr_is_logfile:
        return  # launchd already put it there; writing again is the double-log bug
    try:
        LOG_FILE.parent.mkdir(parents=True, exist_ok=True)
        with open(LOG_FILE, "a") as f:
            f.write(line)
    except Exception:
        pass


def _endpoint() -> tuple[str, int]:
    u = urlparse(HEALTH_URL)
    return u.hostname or "localhost", u.port or (443 if u.scheme == "https" else 80)


def _port_is_open() -> bool:
    """TCP-connect only. This is the discriminator between 'nothing is running'
    (launchd's problem) and 'something is running and not answering' (ours).
    Tries every resolved address — localhost resolves to ::1 first on this box
    while the server binds IPv4, and a v6-only probe would report a healthy
    server as refused."""
    host, port = _endpoint()
    try:
        infos = socket.getaddrinfo(host, port, proto=socket.IPPROTO_TCP)
    except Exception:
        return False
    for family, socktype, proto, _canon, addr in infos:
        try:
            with socket.socket(family, socktype, proto) as s:
                s.settimeout(CONNECT_TIMEOUT)
                if s.connect_ex(addr) == 0:
                    return True
        except Exception:
            continue
    return False


def probe() -> tuple[str, object]:
    """Return (verdict, detail) where verdict is one of:

    ok       — 200 and store == "ok"
    degraded — answered, but the server says it is not well (store != ok / 503).
               This is the signal that is ABSENT in the healthy state, which is
               the only kind worth triggering on.
    hung     — the port accepts connections and /health does not come back
    down     — nothing is listening
    """
    if not _port_is_open():
        return "down", "no listener on %s:%d" % _endpoint()

    try:
        resp = urllib.request.urlopen(
            urllib.request.Request(HEALTH_URL), timeout=HEALTH_TIMEOUT, context=_ssl_ctx
        )
        body = resp.read()
    except urllib.error.HTTPError as e:
        # 503 with a body is the server telling us store == hung. That is an
        # ANSWER, not a failure to answer, and it is the highest-signal case.
        try:
            data = json.loads(e.read())
        except Exception:
            data = {}
        return "degraded", data or f"HTTP {e.code}"
    except Exception as e:
        return "hung", f"port open, /health did not answer: {e}"

    try:
        data = json.loads(body)
    except Exception as e:
        return "hung", f"/health returned unparseable body: {e}"

    if data.get("store") != "ok" or data.get("status") != "ok":
        return "degraded", data
    return "ok", data


def _find_server_pid() -> int | None:
    try:
        out = subprocess.check_output(
            ["pgrep", "-f", SERVER_BIN_NAME], timeout=5, stderr=subprocess.DEVNULL
        ).decode().strip()
        pids = [int(p) for p in out.splitlines() if p.strip() and int(p) != os.getpid()]
        return pids[0] if pids else None
    except Exception:
        return None


def _reload_launch_agent(target: str) -> bool:
    """Reload the one exact server plist after launchd itself is wedged.

    This is intentionally narrower than restarting arbitrary services: the
    label and plist are fixed by this watchdog's configuration, and it only runs
    after the canonical listener has already failed repeated probes.
    """
    plist = LAUNCHD_PLIST
    if not plist.is_file():
        log(f"launchd reload refused — plist does not exist: {plist}")
        return False
    domain = f"gui/{os.getuid()}"
    if DRY_RUN:
        log(f"DRY RUN — would reload {plist} in {domain} and kickstart {target}")
        return True
    try:
        # bootout returns non-zero when already unloaded; that is an acceptable
        # precondition for bootstrap and must not suppress the actual repair.
        subprocess.run(
            ["launchctl", "bootout", domain, str(plist)],
            capture_output=True,
            timeout=10,
        )
        boot = subprocess.run(
            ["launchctl", "bootstrap", domain, str(plist)],
            capture_output=True,
            timeout=10,
        )
        if boot.returncode != 0:
            err = (boot.stderr or b"").decode().strip()
            log(f"launchd bootstrap failed (rc={boot.returncode}) plist={plist}: {err}")
            return False
        subprocess.run(["launchctl", "enable", target], capture_output=True, timeout=5)
        start = subprocess.run(
            ["launchctl", "kickstart", target], capture_output=True, timeout=15
        )
        if start.returncode != 0:
            err = (start.stderr or b"").decode().strip()
            log(f"launchd kickstart after reload failed (rc={start.returncode}): {err}")
            return False
        log(f"launchd agent reloaded from {plist}")
        return True
    except Exception as e:
        log(f"launchd agent reload failed: {e}")
        return False


def restart_server(reload_if_needed: bool = False) -> bool:
    """Kickstart the rust service and wait for it to answer.

    For a persistent `down`, optionally re-register the exact launch agent when
    a normal kickstart cannot recover it."""
    global _last_kickstart
    now = time.time()
    if now - _last_kickstart < KICKSTART_COOLDOWN:
        log(f"kickstart suppressed — cooldown ({int(KICKSTART_COOLDOWN - (now - _last_kickstart))}s left)")
        return False
    _last_kickstart = now

    target = f"gui/{os.getuid()}/{LAUNCHD_LABEL}"
    if DRY_RUN:
        log(f"DRY RUN — would run: launchctl kickstart -k {target}")
        return True

    if IS_MACOS:
        log(f"restarting via launchctl kickstart -k {target}")
        kick_ok = True
        try:
            r = subprocess.run(
                ["launchctl", "kickstart", "-k", target], capture_output=True, timeout=15
            )
            if r.returncode != 0:
                # "Could not find service" is what the retired label produced for
                # months. Say the label out loud so the next reader can see the
                # mismatch instead of inferring it from a silent no-op.
                err = (r.stderr or b"").decode().strip()
                log(f"launchctl kickstart failed (rc={r.returncode}) label={LAUNCHD_LABEL}: {err}")
                kick_ok = False
        except Exception as e:
            log(f"launchctl restart failed: {e}")
            kick_ok = False
        if not kick_ok:
            if not reload_if_needed or not _reload_launch_agent(target):
                return False
    else:
        pid = _find_server_pid()
        if not pid:
            log(f"{SERVER_BIN_NAME} pid not found — cannot restart")
            return False
        log(f"killing {SERVER_BIN_NAME} pid {pid} (supervisor will respawn)")
        try:
            os.kill(pid, 15)
            time.sleep(2)
            try:
                os.kill(pid, 0)
                os.kill(pid, 9)
            except ProcessLookupError:
                pass
        except Exception as e:
            log(f"kill failed: {e}")
            return False

    for _ in range(6):
        time.sleep(5)
        verdict, _detail = probe()
        if verdict == "ok":
            log("server healthy again after restart")
            return True
    if IS_MACOS and reload_if_needed and kick_ok:
        log("server still down after kickstart — reloading its launch agent")
        if _reload_launch_agent(target):
            for _ in range(6):
                time.sleep(5)
                verdict, _detail = probe()
                if verdict == "ok":
                    log("server healthy again after launch-agent reload")
                    return True
    log("server still not healthy after restart")
    return False


def collect_diagnostics(issue: str) -> str:
    parts = [
        "# amux watchdog diagnostics",
        f"time={datetime.now().isoformat()}",
        f"issue={issue}",
        f"platform={sys.platform} docker={IS_DOCKER} url={HEALTH_URL} label={LAUNCHD_LABEL}",
        "",
    ]
    try:
        r = subprocess.run(
            ["launchctl", "print", f"gui/{os.getuid()}/{LAUNCHD_LABEL}"],
            capture_output=True, timeout=10,
        )
        head = (r.stdout or b"").decode().splitlines()[:40]
        parts.append("## launchctl print\n" + "\n".join(head) + "\n")
    except Exception as e:
        parts.append(f"## launchctl print: error — {e}\n")
    try:
        lines = SERVER_LOG.read_text(errors="replace").splitlines()[-150:]
        parts.append("## server-rs.log (last 150 lines)\n" + "\n".join(lines) + "\n")
    except Exception as e:
        parts.append(f"## server-rs.log: error reading — {e}\n")
    try:
        ps = subprocess.check_output(["ps", "-Ao", "pid,%cpu,%mem,etime,command"], timeout=5).decode()
        hits = [ln for ln in ps.splitlines() if SERVER_BIN_NAME in ln]
        parts.append("## processes\n" + "\n".join(hits) + "\n")
    except Exception:
        pass
    # Per-thread CPU ranks the threads; the aggregate only describes them
    # (ethos rule 7 — capture the measurement that discriminates).
    pid = _find_server_pid()
    if pid:
        try:
            parts.append(
                "## per-thread CPU (ps -M)\n"
                + subprocess.check_output(["ps", "-M", str(pid)], timeout=5).decode()
                + "\n"
            )
        except Exception:
            pass
    return "\n".join(parts)


def escalate(issue: str):
    """Write the evidence somewhere a human can find it, and say where.

    Deliberately NOT an autonomous repair. The predecessor spawned an agent with
    push rights here; the honest ceiling for an unattended process is to
    preserve evidence and name the file. Note we cannot use amux's own
    /api/alert/owner for this class of fault — that endpoint lives on the very
    server we are reporting as unreachable."""
    global _last_diag
    now = time.time()
    if now - _last_diag < DIAG_COOLDOWN:
        return
    _last_diag = now
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    path = DIAG_DIR / f"watchdog-diag-{stamp}.md"
    try:
        DIAG_DIR.mkdir(parents=True, exist_ok=True)
        path.write_text(collect_diagnostics(issue))
        log(f"ESCALATION: {issue} — diagnostics written to {path}")
    except Exception as e:
        log(f"ESCALATION: {issue} — could not write diagnostics: {e}")


def run():
    global _consecutive_hung, _consecutive_down
    log(
        f"watchdog starting — target={LAUNCHD_LABEL} url={HEALTH_URL} "
        f"interval={CHECK_INTERVAL}s hung_threshold={HUNG_THRESHOLD} "
        f"down_escalate={DOWN_ESCALATE}{' DRY_RUN' if DRY_RUN else ''}"
    )

    while True:
        _rotate_if_large()
        verdict, detail = probe()

        if verdict == "ok":
            if _consecutive_hung or _consecutive_down:
                log("recovered — healthy")
            _consecutive_hung = 0
            _consecutive_down = 0

        elif verdict == "down":
            _consecutive_down += 1
            _consecutive_hung = 0
            # Brief refusals are expected during self-adoption. Persistent
            # refusal is actionable because launchd can itself be stuck.
            log(f"down ({_consecutive_down}/{DOWN_ESCALATE}) — {detail}")
            if _consecutive_down >= DOWN_ESCALATE:
                log("down threshold reached — recovering the canonical listener")
                if not restart_server(reload_if_needed=True):
                    escalate(
                        f"server has been unreachable for ~{_consecutive_down * CHECK_INTERVAL}s "
                        f"— kickstart and launch-agent reload did not recover {LAUNCHD_LABEL}"
                    )
                _consecutive_down = 0

        else:  # hung | degraded
            _consecutive_hung += 1
            _consecutive_down = 0
            log(f"{verdict} ({_consecutive_hung}/{HUNG_THRESHOLD}) — {detail}")
            if _consecutive_hung >= HUNG_THRESHOLD:
                log(f"threshold reached ({verdict}) — this is the case launchd cannot see; restarting")
                if not restart_server():
                    escalate(f"{verdict} and restart did not recover it")
                _consecutive_hung = 0

        time.sleep(CHECK_INTERVAL)


if __name__ == "__main__":
    import signal

    _resolve_log_duplication()
    signal.signal(signal.SIGTERM, lambda s, f: (log("received SIGTERM, exiting"), sys.exit(0)))
    try:
        run()
    except KeyboardInterrupt:
        log("stopped")
