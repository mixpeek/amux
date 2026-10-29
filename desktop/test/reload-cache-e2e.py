#!/usr/bin/env python3
"""⌘⇧R reloads the active pane past every cache (owner, 2026-10-29: "make it so
cmd shift r works to clear cache of a particular pane thats active").

    desktop/test/reload-cache-e2e.py [path/to/Amux.app]

A second copy of the app with its own config opens one window on this Mac's
amux. A Cache Storage entry is put in the page and the page is marked; View >
Reload Pane Ignoring Cache (amux://test/reload-ignoring-cache, as the key
reaches it) must clear that entry, reload the page (the mark is gone) and log
verdict=pane_reload_ignoring_cache.
"""
import json, os, re, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-reload-e2e-")
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
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"


def log():
    return open(LOG, errors="replace").read() if os.path.exists(LOG) else ""


def start():
    global p
    n = log().count("verdict=windows_restored")
    p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST),
                         stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
    end = time.time() + 30
    while time.time() < end and log().count("verdict=windows_restored") <= n:
        time.sleep(0.3)
    settle()


def settle(secs=30):
    end = time.time() + secs
    while time.time() < end:
        try:
            w = win()
            if w and "amux" not in [t for tabs in w.get("pane_tabs", []) for t in tabs]:
                return
        except (OSError, ValueError, KeyError):
            pass
        time.sleep(0.5)


def stop():
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(10)
    except subprocess.TimeoutExpired:
        p.kill()


def send(u, settle_s=1.5):
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), u)
    time.sleep(settle_s)


def state():
    before = os.path.getmtime(ST) if os.path.exists(ST) else 0
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/state")
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(ST) or os.path.getmtime(ST) == before):
        time.sleep(0.05)
    return json.load(open(ST))


def win():
    return next((w for w in state()["windows"] if w.get("grid")), None)



def js(code, timeout=15):
    out = ST + ".js"
    if os.path.exists(out):
        os.remove(out)
    import urllib.parse
    send("amux://test/js-pane?i=0&code=" + urllib.parse.quote(code), 0)
    end = time.time() + timeout
    while time.time() < end and not os.path.exists(out):
        time.sleep(0.1)
    return open(out).read() if os.path.exists(out) else "no answer"


json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": "{{80, 80}, {1100, 760}}", "selected": True}]}, open(CFG, "w"))
try:
    start()
    time.sleep(3)
    seeded = js("const c = await caches.open('amux-e2e-reload-probe'); await c.put('/amux-e2e-reload-probe', new Response('x'));"
                " window.__amuxReloadMark = 1; return String(await caches.has('amux-e2e-reload-probe'))")
    check("a cache entry is seeded in the page", seeded == "true", seeded)
    before = log().count("verdict=pane_reload_ignoring_cache")
    send("amux://test/reload-ignoring-cache", 1)
    end = time.time() + 15
    while time.time() < end and log().count("verdict=pane_reload_ignoring_cache") <= before:
        time.sleep(0.3)
    check("the reload is logged", log().count("verdict=pane_reload_ignoring_cache") == before + 1, log()[-400:])
    time.sleep(4)
    after = js("return JSON.stringify({has: await caches.has('amux-e2e-reload-probe'), mark: typeof window.__amuxReloadMark})")
    try:
        a = json.loads(after)
    except ValueError:
        a = {}
    check("the page's caches are cleared", a.get("has") is False, after)
    check("and the page reloaded", a.get("mark") == "undefined", after)
finally:
    if p and p.poll() is None:
        stop()
print("PASS: reload-cache, all checks passed" if fails == 0 else f"reload-cache: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
