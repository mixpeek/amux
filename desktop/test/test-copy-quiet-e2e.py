#!/usr/bin/env python3
"""A test copy of the app stays out of the owner's way (owner, 2026-11-09:
"when u do work related to the amux mac app / ux you open up new amux apps and
it disrupts my experience ... maybe you create a new amux app for development").

    desktop/test/test-copy-quiet-e2e.py [path/to/Amux.app]

An agent's build is "Amux Dev" (com.amux.desktop.dev) with no amux:// handler,
so it shares nothing with the installed Amux. Run with AMUX_DESKTOP_TEST=1 it has
no Dock icon, never comes to the front (not even for amux://test/activate), and
every window it puts on screen is fully transparent and lets clicks through,
while its pages still load. The owner's own preferences file is not touched.
A second run with AMUX_DESKTOP_TEST_VISIBLE=1 is the positive control: the same
probe sees an opaque window, so "alpha 0" above is a measurement.
"""
import json, os, plistlib, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-quiet-e2e-")
CFG, LOG, ST = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
WSCTL = os.path.join(TMP, "wsctl")
OWNER_PREFS = os.path.expanduser("~/Library/Preferences/com.amux.desktop.plist")
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1
    return ok


def log():
    try:
        return open(LOG, errors="replace").read()
    except FileNotFoundError:
        return ""


def seen(pid):
    r = sh(WSCTL, "seen", str(pid))
    return json.loads(r.stdout) if r.returncode == 0 and r.stdout.strip() else {}


def state(pid):
    before = os.path.getmtime(ST) if os.path.exists(ST) else 0
    sh(WSCTL, "send", str(pid), "amux://test/state")
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(ST) or os.path.getmtime(ST) == before):
        time.sleep(0.05)
    return json.load(open(ST))


assert sh("xcrun", "swiftc", "-O", "-o", WSCTL, os.path.join(HERE, "wsctl.swift")).returncode == 0
info = plistlib.load(open(os.path.join(APP, "Contents", "Info.plist"), "rb"))
check("the build is Amux Dev, its own app", info.get("CFBundleIdentifier") == "com.amux.desktop.dev"
      and info.get("CFBundleName") == "Amux Dev", (info.get("CFBundleIdentifier"), info.get("CFBundleName")))
check("and claims no amux:// links", "CFBundleURLTypes" not in info, info.get("CFBundleURLTypes"))

url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": "{{80, 80}, {1000, 700}}", "tabGroup": 1, "selected": True,
                        "grid": {"tree": "r0.500(0,1)", "panes": [{"url": url + "/#view=calendar", "title": "Calendar"}]}},
                       {"url": url + "/#view=messages", "frame": "{{80, 80}, {1000, 700}}", "tabGroup": 1}]}, open(CFG, "w"))


def run(visible):
    """Launch a copy, let it restore and ask to come forward, then read what is on screen."""
    prefs_before = os.path.getmtime(OWNER_PREFS) if os.path.exists(OWNER_PREFS) else None
    front_before = None
    n = log().count("verdict=windows_restored")
    env = dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST)
    if visible:
        # The control is the one run that shows a window: a small one, briefly, in a corner.
        env["AMUX_DESKTOP_TEST_VISIBLE"] = "1"
        c = json.load(open(CFG))
        for w in c["windows"]:
            w["frame"] = "{{200, 200}, {360, 260}}"
        json.dump(c, open(CFG, "w"))
    p = subprocess.Popen([BIN], env=env, stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
    try:
        front_before = seen(p.pid).get("front_pid")
        end = time.time() + 30
        while time.time() < end and log().count("verdict=windows_restored") <= n:
            time.sleep(0.3)
        # Its pages load: the tabs name themselves from them.
        titles, end = [], time.time() + 30
        while time.time() < end:
            titles = [w["title"] for w in state(p.pid)["windows"]]
            if titles and all(t not in ("", "amux") for t in titles):
                break
            time.sleep(0.5)
        if not visible:   # the control needs only to be seen, not brought forward
            sh(WSCTL, "send", str(p.pid), "amux://test/activate")
        time.sleep(1.5)
        s = seen(p.pid)
        prefs_after = os.path.getmtime(OWNER_PREFS) if os.path.exists(OWNER_PREFS) else None
        return s, titles, front_before, prefs_before, prefs_after
    finally:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(15)
        except subprocess.TimeoutExpired:
            p.kill()


s, titles, front_before, pb, pa = run(visible=False)
print(f"    quiet copy: {json.dumps(s)[:300]}")
check("its pages load (the tabs are named by them)", len(titles) >= 2 and all(t not in ("", "amux") for t in titles), titles)
check("no Dock icon or menu bar (an accessory app)", s.get("policy") == "accessory", s.get("policy"))
# Only whether IT came forward: the owner may switch apps while this runs.
check("it did not come to the front, even when asked to", not s.get("front"), (s.get("front_pid"), front_before))
wins = s.get("windows", [])
big = [w for w in wins if w["w"] >= 300]
check("it has windows on screen to look at", len(big) >= 1, wins)
check("every one of them fully transparent", all(w["alpha"] == 0 for w in wins), wins)
check("it says so in its log", "verdict=test_copy_quiet" in log())
check("it asked for no notification permission", "notifications granted=" not in log()
      and "notifications_unavailable reason=quiet_test_copy" in log())
check("and did not make itself a login item", "verdict=login_item_skipped reason=test_copy" in log() and "login_item want=" not in log())
check("the owner's preferences file was not touched", pb == pa, (pb, pa))

s, titles, _, _, _ = run(visible=True)
print(f"    visible copy: {json.dumps(s)[:300]}")
check("with AMUX_DESKTOP_TEST_VISIBLE=1 the same probe sees an opaque window",
      any(w["alpha"] > 0.9 and w["w"] >= 300 for w in s.get("windows", [])), s.get("windows"))
check("the control window was the small one", all(w["w"] <= 400 for w in s.get("windows", []) if w["alpha"] > 0.9), s.get("windows"))
check("and a regular app", s.get("policy") == "regular", s.get("policy"))

print(f"{'PASS' if fails == 0 else 'FAIL'}: {fails} failing check(s) (log {LOG})")
sys.exit(1 if fails else 0)
