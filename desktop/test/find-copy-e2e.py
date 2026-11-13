#!/usr/bin/env python3
"""⌘F, ⌃F and ⌘C in the Mac app (owner, 2026-11-13: "Make sure in the Mac OS
app I can use ctrl F and cmd c and cmd f etc. currently it just makes the
disabled beep sound").

    desktop/test/find-copy-e2e.py [path/to/Amux.app]

A quiet copy of the app on a private server serving this checkout's
dashboard. Keys go through the app's menu and window as AppKit routes them
(the amux://test/key hook). Checks: ⌘F opens a find bar, typing finds and
selects a word on the page, Escape closes it and gives the keyboard back to
the page; ⌃F outside a text box opens it, inside one it does not; ⌘C copies
the page's selection (Edit > Copy, chosen through the menu). The test copy's clipboard and find text are private
pasteboards, never the owner's.
"""
import json, os, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-find-e2e-")
CFG, LOG, ST = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
WORD = "zebraquux"
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
server = subprocess.Popen([SERVER], env=_env, stdout=open(os.path.join(TMP, "server.log"), "w"), stderr=subprocess.STDOUT)
_ctx = ssl.create_default_context(); _ctx.check_hostname, _ctx.verify_mode = False, ssl.CERT_NONE
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


def key(k, mods="", wait=0.6):
    hook(f"key?k={k}&mods={urllib.parse.quote(mods)}", wait)


def js(code, timeout=15):
    out = ST + ".js"
    if os.path.exists(out):
        os.remove(out)
    hook("js-pane?i=0&code=" + urllib.parse.quote(code), 0)
    end = time.time() + timeout
    while time.time() < end and not os.path.exists(out):
        time.sleep(0.1)
    return open(out).read() if os.path.exists(out) else "no answer"


def page():
    before = os.path.getmtime(ST) if os.path.exists(ST) else 0
    hook("state", 0)
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(ST) or os.path.getmtime(ST) == before):
        time.sleep(0.05)
    return json.load(open(ST))["windows"][0]


try:
    end = time.time() + 60
    while time.time() < end and js("return String(typeof _hkFindKey)") != "function":
        time.sleep(1)
    js(f"const d = document.createElement('div'); d.id = 'find-e2e'; d.textContent = 'before {WORD} after';"
       " d.style.cssText = 'position:fixed;top:200px;left:200px;z-index:99999'; document.body.appendChild(d);"
       " document.activeElement && document.activeElement.blur(); return 'ok'")

    key("f", "cmd")
    st = page()
    check("⌘F opens the find bar and puts the keyboard in it", st.get("find") is not None and st["responder"] != "WKWebView", st)
    for ch in WORD:
        key(ch, "", 0.05)
    time.sleep(1.5)
    sel = js("return String(getSelection())")
    st = page()
    check("typing finds the word and selects it on the page", sel == WORD and st.get("find") == WORD and st.get("find_status") == "", f"selection={sel!r} {st}")
    check("and the find is logged", "verdict=find_in_page found=1" in log(), log()[-300:])
    key("esc")
    st = page()
    check("Escape closes the bar and the page has the keyboard again", st.get("find") is None and st["responder"] == "WKWebView", st)

    # Edit > Copy, which ⌘C chooses once the page leaves the key alone (a test
    # copy is never the active app, so WebKit cannot hand the key back).
    hook("menu?title=Copy", 1.2)
    check("⌘C copies the page's selection", page().get("clipboard") == WORD and "verdict=copy_done source=selection" in log(), page().get("clipboard"))
    js("getSelection().removeAllRanges(); return 'ok'")
    hook("menu?title=Copy", 1.2)
    check("⌘C with nothing selected is logged, not an error", "verdict=copy_nothing_selected" in log(), log()[-300:])

    js("document.activeElement && document.activeElement.blur(); return 'ok'")
    key("f", "ctrl", 1)
    check("⌃F outside a text box opens the find bar", page().get("find") is not None, page())
    key("esc")
    js("const t = document.createElement('textarea'); t.id = 'find-e2e-box'; document.body.appendChild(t); t.focus(); return document.activeElement.id")
    key("f", "ctrl", 1)
    check("⌃F in a text box leaves it to the text box", page().get("find") is None, page())
finally:
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(10)
    except subprocess.TimeoutExpired:
        p.kill()
    server.terminate()
print("PASS: find-copy, all checks passed" if fails == 0 else f"find-copy: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
