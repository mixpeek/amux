#!/usr/bin/env python3
"""A tab nobody has looked at for a while is unloaded when the tabs together
are over the memory budget, and loads again, on its view, when it is shown
(owner, 2026-11-25: "make it faster to open, load, refresh": 57 live tabs held
27 GB and 13 GB of swap).

    desktop/test/tab-discard-e2e.py [path/to/Amux.app]

A quiet copy of the app opens three tabs of one window on a private server:
Board shown, Calendar and Scheduler behind it. With a 1 MB budget and a 1 s
hidden limit both hidden tabs are discarded (their pages read about:blank,
their saved views stay), the shown one never is, and the background queue
does not reload them. Showing Calendar loads it again on #view=calendar.
"""
import json, os, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-discard-e2e-")
CFG, LOG, ST = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
fails = 0
p = None


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
SERVER = os.environ.get("AMUX_E2E_SERVER_BIN") or os.path.expanduser("~/.local/bin/amux-server-rs")
_s = socket.socket(); _s.bind(("127.0.0.1", 0)); PORT = _s.getsockname()[1]; _s.close()
url = f"https://localhost:{PORT}"
SHOME = os.path.join(TMP, "home"); os.makedirs(os.path.join(SHOME, "tmux"))
_env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}
_env.update(AMUX_HOME=SHOME, AMUX_RS_PORT=str(PORT), AMUX_ISOLATED="1", AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(SHOME, "tmux"),
            AMUX_DASHBOARD_DIR=os.path.join(REPO, "crates", "amux-dashboard", "static"))
server = subprocess.Popen([SERVER], env=_env, stdout=open(os.path.join(TMP, "server.log"), "w"), stderr=subprocess.STDOUT)
_ctx = ssl.create_default_context(); _ctx.check_hostname, _ctx.verify_mode = False, ssl.CERT_NONE
for _ in range(120):
    try:
        if urllib.request.urlopen(url + "/health", context=_ctx, timeout=2).status == 200:
            break
    except OSError:
        pass
    time.sleep(0.5)


def log():
    return open(LOG, errors="replace").read() if os.path.exists(LOG) else ""


def send(u, settle_s=1.5):
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), u)
    time.sleep(settle_s)


def pages():
    before = os.path.getmtime(ST) if os.path.exists(ST) else 0
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/state")
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(ST) or os.path.getmtime(ST) == before):
        time.sleep(0.05)
    out = {}
    for pg in json.load(open(ST)).get("pages", []):
        name = (pg.get("saved") or "").rsplit("#view=", 1)[-1]
        out[name] = pg
    return out


frame = "{{60, 60}, {1100, 760}}"
json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": frame, "tabGroup": 1, "selected": True},
                       {"url": url + "/#view=calendar", "frame": frame, "tabGroup": 1, "selected": False},
                       {"url": url + "/#view=scheduler", "frame": frame, "tabGroup": 1, "selected": False}]}, open(CFG, "w"))
try:
    env = dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST, AMUX_HOME=SHOME,
               AMUX_DESKTOP_DEFER_TABS="0", AMUX_DESKTOP_MEM_TICK_S="2", AMUX_DESKTOP_DISCARD_HIDDEN_S="1",
               AMUX_DESKTOP_WEB_BUDGET_MB=os.environ.get("E2E_BUDGET_MB", "1"))
    p = subprocess.Popen([BIN], env=env, stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
    end = time.time() + 40
    while time.time() < end and "verdict=windows_restored" not in log():
        time.sleep(0.3)
    end = time.time() + 30
    while time.time() < end and log().count("verdict=tab_discarded") < 2:
        time.sleep(0.5)
    pg = pages()
    check("three pages, one per tab", set(pg) >= {"board", "calendar", "scheduler"}, list(pg))
    for v in ("calendar", "scheduler"):
        check(f"hidden {v} is discarded", pg.get(v, {}).get("discarded") is True and pg.get(v, {}).get("url") == "about:blank", pg.get(v))
        check(f"and keeps its view to come back to", pg.get(v, {}).get("saved", "").endswith("#view=" + v), pg.get(v))
    check("the shown tab is never discarded", pg.get("board", {}).get("discarded") is False
          and pg.get("board", {}).get("url", "").startswith(url), pg.get("board"))
    check("each discard is logged with its size and the budget", "verdict=tab_discarded mb=" in log() and "budget_mb=1 " in log(), log()[-600:])
    time.sleep(8)
    check("the background queue does not load them again", "verdict=discarded_tab_restored" not in log()
          and pages().get("scheduler", {}).get("discarded") is True, log()[-400:])

    send("amux://test/select-view?view=view%3Dcalendar", 4)
    pg = pages()
    check("showing Calendar loads it again", "verdict=discarded_tab_restored why=" in log() and pg.get("calendar", {}).get("discarded") is False, log()[-400:])
    check("on its own view", pg.get("calendar", {}).get("url", "").endswith("#view=calendar"), pg.get("calendar"))
    check("Scheduler, still behind, stays unloaded", pg.get("scheduler", {}).get("discarded") is True, pg.get("scheduler"))
finally:
    if p and p.poll() is None:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            p.kill()
    server.terminate()
print("PASS: tab-discard, all checks passed" if fails == 0 else f"tab-discard: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
