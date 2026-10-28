#!/usr/bin/env python3
"""View > Zoom In applies to every window with Zoom All Windows on (Ethan,
2026-10-28: "apply a zoom in/zoom out to all windows").

    desktop/test/zoom-all-e2e.py [path/to/Amux.app]

Runs a copy of the app (its own config, AMUX_DESKTOP_TEST=1) with two windows
on this Mac's amux server, zooms through the menu actions (amux://test/zoom),
and reads every page's dashboard zoom level back. The copy shares the app's
web storage, so the level each page started at is put back at the end.
"""
import json, os, signal, subprocess, sys, tempfile, time
from urllib.parse import quote

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-zoomall-e2e-")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
with open(CFG, "w") as f:
    json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
               "windows": [{"url": f"{url}/#view=sessions", "frame": "{{100, 200}, {800, 600}}"},
                           {"url": f"{url}/#view=board", "frame": "{{950, 200}, {800, 600}}"}]}, f)
p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE),
                     stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
end = time.time() + 20
while time.time() < end and "verdict=windows_restored" not in open(LOG, errors="replace").read():
    time.sleep(0.3)


def send(u, settle=0.5):
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/" + u)
    time.sleep(settle)


def levels():
    f = STATE + ".zoom"
    before = os.path.getmtime(f) if os.path.exists(f) else 0
    send("zoom-levels", 0)
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(f) or os.path.getmtime(f) == before):
        time.sleep(0.05)
    return json.load(open(f))


def until(cond, t=20):
    end = time.time() + t
    while time.time() < end:
        z = levels()
        if cond(z):
            return z
        time.sleep(0.4)
    return levels()


start = None
try:
    z = until(lambda z: len(z["levels"]) == 2 and all(n > 0 for n in z["levels"]), 30)
    check("two dashboard pages are up", len(z["levels"]) == 2 and all(n > 0 for n in z["levels"]), str(z))
    start = z["levels"][0]
    check("Zoom All Windows is on by default", z["all"] is True, str(z))
    send("zoom?op=reset")
    z = until(lambda z: z["levels"] == [100, 100])
    check("Actual Size puts both windows at 100%", z["levels"] == [100, 100], str(z))

    print("zoom in and out reach every window")
    send("zoom?op=in")
    z = until(lambda z: z["levels"] == [110, 110])
    check("Zoom In: both windows at 110%", z["levels"] == [110, 110], str(z))
    send("zoom?op=out"); send("zoom?op=out")
    z = until(lambda z: z["levels"] == [95, 95])
    check("Zoom Out twice: both windows at 95%", z["levels"] == [95, 95], str(z))
    check("the app logged each broadcast", "verdict=zoom_all level=95 pages=1 dashboard=1" in open(LOG, errors="replace").read())

    print("a zoom from the page's own controls reaches the other window")
    # What app.js _zoomShare sends after the Settings buttons or the keyboard.
    send("js?code=" + quote("_zoomLevel=130;_applyZoom();window.webkit.messageHandlers.amuxZoom.postMessage(130)"))
    z = until(lambda z: z["levels"] == [130, 130])
    check("a page's amuxZoom puts both windows at 130%", z["levels"] == [130, 130], str(z))
    send("zoom?op=reset"); send("zoom?op=out")
    z = until(lambda z: z["levels"] == [95, 95])

    print("with the option off, a zoom stays in its own window")
    send("zoom?op=all")
    send("zoom?op=in", 1.5)
    z = levels()
    check("the option reads off", z["all"] is False, str(z))
    check("only one window changed", sorted(z["levels"]) == [95, 100], str(z))
    send("zoom?op=all")
    z = until(lambda z: z["all"] and len(set(z["levels"])) == 1)
    check("turning it back on brings the other window to the same level", z["all"] and len(set(z["levels"])) == 1, str(z))
finally:
    if start:
        send("js?code=" + quote(f"_zoomLevel={start};_applyZoom()"))
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(10)
    except subprocess.TimeoutExpired:
        p.kill()

print(f"\n{'PASS' if not fails else 'FAIL'}: zoom-all-e2e ({fails} failed) log={LOG}")
sys.exit(1 if fails else 0)
