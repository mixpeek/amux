#!/usr/bin/env python3
"""A microphone plugged in or unplugged in the Mac app: a toast says whether
dictation uses it (owner, 2026-11-21).

    desktop/test/mic-change-e2e.py [path/to/Amux.app]

A quiet copy of the app on a private server serving this checkout's dashboard.
Microphones come and go through amux://test/mics, which feeds the same watcher
CoreAudio's device listener does, so no real device is touched. Checks each
case of the toast: the chosen mic connects, another mic connects, a new mic
becomes the default, the chosen mic disconnects, another connects while it is away, the default disconnects, and
an unused mic disconnects.
"""
import json, os, re, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-mic-change-e2e-")
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


WSCTL = os.path.join(TMP, "wsctl")
assert sh("xcrun", "swiftc", "-O", "-o", WSCTL, os.path.join(HERE, "wsctl.swift")).returncode == 0
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
SERVER = os.environ.get("AMUX_E2E_SERVER_BIN") or os.path.expanduser("~/.local/bin/amux-server-rs")
_s = socket.socket(); _s.bind(("127.0.0.1", 0)); PORT = _s.getsockname()[1]; _s.close()
url = f"https://localhost:{PORT}"
SHOME = os.path.join(TMP, "home"); os.makedirs(os.path.join(SHOME, "tmux"))
_env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}
_env.update(AMUX_HOME=SHOME, AMUX_RS_PORT=str(PORT), AMUX_ISOLATED="1", AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(SHOME, "tmux"),
            AMUX_DASHBOARD_DIR=os.path.join(REPO, "crates", "amux-dashboard", "static"))
SLOG = os.path.join(TMP, "server.log")
server = subprocess.Popen([SERVER], env=_env, stdout=open(SLOG, "w"), stderr=subprocess.STDOUT)
_ctx = ssl.create_default_context(); _ctx.check_hostname, _ctx.verify_mode = False, ssl.CERT_NONE


def api(path):
    return json.load(urllib.request.urlopen(url + path, context=_ctx, timeout=10))


for _ in range(120):
    try:
        if urllib.request.urlopen(url + "/health", context=_ctx, timeout=2).status == 200:
            break
    except OSError:
        pass
    time.sleep(0.5)

json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
           "windows": [{"url": url + "/#view=board", "frame": "{{80, 80}, {1200, 800}}"}]}, open(CFG, "w"))
p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST),
                     stdout=open(LOG, "a"), stderr=subprocess.STDOUT)


def hook(h, wait=0.6):
    sh(WSCTL, "send", str(p.pid), "amux://test/" + h)
    time.sleep(wait)


def js(code, timeout=15):
    out = ST + ".js"
    if os.path.exists(out):
        os.remove(out)
    hook("js-pane?i=0&code=" + urllib.parse.quote(code), 0)
    end = time.time() + timeout
    while time.time() < end and not os.path.exists(out):
        time.sleep(0.05)
    return open(out).read() if os.path.exists(out) else "no answer"



def toast():
    return js("const t = document.getElementById('toast'); return t.classList.contains('visible') ? t.textContent : ''")


def mics(q, want):
    js("document.getElementById('toast').classList.remove('visible'); return 'ok'")
    hook("mics?" + q, 0)
    end, t = time.time() + 5, ""
    while time.time() < end:
        t = toast()
        if t:
            break
        time.sleep(0.1)
    check(f"{q} -> {want!r}", t == want, repr(t))


def choose(name):
    js(f"localStorage.setItem('amux_dict_mic', {json.dumps(name)}); return 'ok'")


try:
    end = time.time() + 60
    while time.time() < end and js("return String(typeof window._dictMicsChanged)") != "function":
        time.sleep(1)
    check("the page has the handler", js("return String(typeof window._dictMicsChanged)") == "function")
    check("the app watches microphones from launch", "verdict=mic_watch_started" in log(), log()[-300:])
    q = urllib.parse.quote
    hook("mics?default=" + q("Desk Mic"), 1.5)   # the test default, before any change

    choose("")
    mics("add=" + q("Studio USB") + "&default=" + q("Studio USB"), "Studio USB connected. Dictation uses it.")
    mics("add=" + q("Webcam"), "Webcam connected. Dictation keeps using Studio USB.")
    mics("remove=" + q("Webcam"), "Webcam disconnected. Dictation did not use it.")
    mics("remove=" + q("Studio USB") + "&default=" + q("Desk Mic"), "Studio USB disconnected. Dictation now uses Desk Mic.")

    choose("Podcast Mic")
    mics("add=" + q("Podcast Mic"), "Podcast Mic connected. Dictation uses it.")
    mics("add=" + q("AirPods") + "&default=" + q("AirPods"), "AirPods connected. Dictation keeps using Podcast Mic.")
    mics("remove=" + q("Podcast Mic"), "Podcast Mic disconnected. Dictation uses AirPods until it is back.")
    mics("add=" + q("Webcam"), "Webcam connected. Dictation keeps using AirPods.")   # the chosen one is still away
    check("the app logs each change", log().count("verdict=mic_changed") >= 7, log().count("verdict=mic_changed"))
    sl = open(SLOG, errors="replace").read()
    check("and the page beacons it", "dict_mic_changed" in sl, sl[-300:])
finally:
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(10)
    except subprocess.TimeoutExpired:
        p.kill()
    server.terminate()
print("PASS" if fails == 0 else f"FAIL ({fails})")
sys.exit(1 if fails else 0)
