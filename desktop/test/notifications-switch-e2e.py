#!/usr/bin/env python3
"""The dashboard's device switch is the Mac app's own (owner, 2026-11-13: "Make
sure I can disable/enable notifications on the amux ui and the device/client
respects it").

    desktop/test/notifications-switch-e2e.py [path/to/Amux.app]

A quiet copy of the app on a private server serving this checkout's
dashboard, one window split in two pages. Turning notifications off in one
page must set the app's switch (its config), reach the other page, and stop a
Messages notification in the app; turning it on in the other page brings both
back.
"""
import json, os, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-notif-e2e-")
CFG, LOG, ST = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


def log():
    return open(LOG, errors="replace").read() if os.path.exists(LOG) else ""


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

json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": "{{60, 60}, {1300, 820}}",
                        "grid": {"tree": "r0.500(0,1)", "panes": [{"url": url + "/#view=calendar", "title": "Calendar"}]}}]}, open(CFG, "w"))
p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST),
                     stdout=open(LOG, "a"), stderr=subprocess.STDOUT)


def js(i, code, timeout=15):
    out = ST + ".js"
    if os.path.exists(out):
        os.remove(out)
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/js-pane?i=%d&code=%s" % (i, urllib.parse.quote(code)))
    end = time.time() + timeout
    while time.time() < end and not os.path.exists(out):
        time.sleep(0.1)
    return open(out).read() if os.path.exists(out) else "no answer"


def native(i):
    return js(i, "return String(typeof _notifsNative === 'undefined' ? 'none' : _notifsNative)")


def config_on():
    return json.load(open(CFG)).get("notifications", True)


try:
    end = time.time() + 60
    while time.time() < end and not all(native(i) in ("true", "false") for i in (0, 1)):
        time.sleep(1)
    check("both pages see the app's switch, on by default",
          native(0) == "true" and native(1) == "true" and js(0, "return String(!!_notifDesktopPrefs())") == "true",
          f"{native(0)} {native(1)}")
    js(0, "await _notifToggleNative(); return 'ok'")
    time.sleep(1.5)
    check("off in one page sets the app's own switch", config_on() is False, json.load(open(CFG)).get("notifications"))
    check("and reaches the other page", native(1) == "false", native(1))
    check("and is logged with its source", "verdict=notifications_set on=0 source=page pages=2" in log(), log()[-400:])
    js(0, "window.webkit.messageHandlers.amuxNotify.postMessage({title: 'x', body: 'y', url: '#view=messages', tag: 'conv-t'}); return 'ok'")
    time.sleep(1)
    check("a Messages notification is not posted while off", "verdict=notification_suppressed what=message" in log(), log()[-400:])
    js(1, "await _notifToggleNative(); return 'ok'")
    time.sleep(1.5)
    check("on again from the other page", config_on() is True and native(0) == "true", f"{config_on()} {native(0)}")
finally:
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(10)
    except subprocess.TimeoutExpired:
        p.kill()
    server.terminate()
print("PASS: notifications-switch, all checks passed" if fails == 0 else f"notifications-switch: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
