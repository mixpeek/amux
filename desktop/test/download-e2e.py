#!/usr/bin/env python3
"""A file a page saves lands in Downloads (Ethan, 2026-10-26: the file viewer's
Download asked macOS for an app to open "blob:https://sandbox.amux.io/...").

    desktop/test/download-e2e.py [path/to/Amux.app]

Runs a second copy of the app (AMUX_DESKTOP_TEST=1, its own config, its own
downloads folder via AMUX_DESKTOP_DOWNLOADS_DIR) against the local amux server
with one tab, then makes the page save two files the two ways a page does: an
<a download> on a blob: URL (the file viewer's Download, the theme export) and
window.open on a blob: URL. Each must arrive in the downloads folder with the
bytes the page made, and the app log must say download_finished, never hand a
blob: URL to another app.
"""
import json, os, signal, subprocess, sys, tempfile, time, urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-download-e2e-")
CFG, LOG, STATE, DL = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json", "downloads"))
os.makedirs(DL)
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


def log():
    try:
        return open(LOG, errors="replace").read()
    except FileNotFoundError:
        return ""


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
with open(CFG, "w") as f:
    json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
               "windows": [{"url": url + "/#view=sessions", "frame": "{{80, 80}, {900, 700}}", "selected": True}]}, f)
p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE,
                                     AMUX_DESKTOP_DOWNLOADS_DIR=DL), stdout=open(LOG, "a"), stderr=subprocess.STDOUT)


def js(code):
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/js?code=" + urllib.parse.quote(code, safe=""))


def wait_file(name, timeout=20):
    end = time.time() + timeout
    while time.time() < end:
        path = os.path.join(DL, name)
        if os.path.exists(path) and os.path.getsize(path) > 0:
            time.sleep(0.5)
            return open(path, "rb").read()
        time.sleep(0.3)
    return None


try:
    end = time.time() + 40
    while "verdict=windows_restored" not in log() and time.time() < end:
        time.sleep(0.3)
    time.sleep(6)   # the page boots
    print("1. <a download> on a blob: URL (the file viewer's Download)")
    js("(() => { const b = new Blob(['col1,col2\\n1,2\\n'], {type: 'text/csv'}); const a = document.createElement('a');"
       " a.href = URL.createObjectURL(b); a.download = 'amux-dl-test.csv'; document.body.appendChild(a); a.click(); a.remove(); return 1; })()")
    got = wait_file("amux-dl-test.csv")
    check("the file is in the downloads folder with the page's bytes", got == b"col1,col2\n1,2\n", repr(got))
    print("2. window.open on a blob: URL")
    js("(() => { const b = new Blob(['hello'], {type: 'application/octet-stream'}); window.open(URL.createObjectURL(b), '_blank'); return 1; })()")
    end = time.time() + 20
    files = []
    while time.time() < end:
        files = [f for f in os.listdir(DL) if f != "amux-dl-test.csv"]
        if files:
            break
        time.sleep(0.3)
    time.sleep(0.5)
    check("it is saved too, not handed to another app", bool(files) and open(os.path.join(DL, files[0]), "rb").read() == b"hello", str(files))
    check("the log records finished downloads", log().count("verdict=download_finished") >= 2, "")
    check("nothing failed", "verdict=download_failed" not in log(), "")
finally:
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(15)
    except subprocess.TimeoutExpired:
        p.kill()

print("PASS: download, all checks passed" if fails == 0 else f"download: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
