#!/usr/bin/env python3
"""Every setting in one place, in its own window: amux > Settings… opens a
Settings window apart from the amux windows (owner, 2026-11-27: "it should open
up a modal that's outside of the amux app"). It shows the server's Settings
alone, on its This Mac tab, with the server's tabs beside it (owner, 2026-11-25:
"consolidate all our settings to the amux Mac app settings").

    desktop/test/mac-settings-e2e.py [path/to/Amux.app]

A second copy of the app, with its own config, opens one window on a private
server that serves this checkout's dashboard. Settings… must open the Settings
window on the server's This Mac tab, and leave the amux window as it was. A switch flipped there must reach the
app's config, and the page must redraw from the app's answer. A workspace saved
there must be in the config and listed. The amux, View and Dock menus no longer
carry their own copies of these switches.
"""
import json, os, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-macset-e2e-")
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


def state():
    before = os.path.getmtime(ST) if os.path.exists(ST) else 0
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/state")
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(ST) or os.path.getmtime(ST) == before):
        time.sleep(0.05)
    return json.load(open(ST))


def js(code, timeout=15, where="settings"):
    """Run code in the Settings window's page, or (where="pane") the amux window's."""
    out = ST + ".js"
    if os.path.exists(out):
        os.remove(out)
    send("amux://test/js-pane?i=0&in=%s&code=%s" % (where, urllib.parse.quote(code)), 0)
    end = time.time() + timeout
    while time.time() < end and not os.path.exists(out):
        time.sleep(0.1)
    return open(out).read() if os.path.exists(out) else "no answer"


def page(where="settings"):
    r = js(where=where, code="await new Promise(r => setTimeout(r, 800));"
           " const m = document.getElementById('settings-menu'), t = document.getElementById('settings-tab-mac'),"
           " h = document.getElementById('mac-settings'), b = document.getElementById('mac-set-dockBadge');"
           " return JSON.stringify({open: !!(m && m.classList.contains('open')), tab_shown: !!(t && !t.hidden),"
           " active: (document.querySelector('.settings-tab-btn.active') || {}).dataset?.stab || '',"
           " text: h ? h.innerText : '', dock: b ? b.checked : null,"
           " perms: h ? h.querySelectorAll('[data-permission]').length : 0})")
    try:
        return json.loads(r)
    except ValueError:
        return {"error": r}


def config():
    return json.load(open(CFG))


json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "dockBadge": True,
           "windows": [{"url": url + "/#view=board", "frame": "{{60, 60}, {1200, 820}}"}]}, open(CFG, "w"))
try:
    p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST),
                         stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
    end = time.time() + 40
    while time.time() < end and "verdict=windows_restored" not in log():
        time.sleep(0.3)
    time.sleep(4)

    windows_before = len(state()["windows"])
    send("amux://test/menu?title=" + urllib.parse.quote("Settings…"), 2.5)
    end = time.time() + 15
    while time.time() < end and "verdict=settings_window_shown" not in log():
        time.sleep(0.3)
    check("amux > Settings… is a menu item", "verdict=test_menu_chosen title=Settings… found=1" in log(), log()[-400:])
    check("it opens the Settings window on the server's Settings", "verdict=settings_window_open server=1" in log()
          and "verdict=settings_window_shown source=server" in log(), log()[-600:])
    st = state()
    sw = st.get("settings_window") or {}
    check("the Settings window is its own window, titled Settings", sw.get("visible") and sw.get("title") == "Settings", sw)
    check("it loads the server's Settings-only page", "settings_window=1" in sw.get("url", "") and sw.get("url", "").startswith(url), sw)
    check("no amux window was opened for it", len(st["windows"]) == windows_before, (windows_before, len(st["windows"])))
    check("and the amux window's own Settings stays closed", page("pane").get("open") is False, page("pane"))
    shown = json.loads(js("return JSON.stringify({mode: document.documentElement.classList.contains('settings-window'),"
                          " server_tabs: [...document.querySelectorAll('.settings-tab-btn[data-group=server]')].filter(b => !b.hidden).map(b => b.dataset.stab)})") or "{}")
    check("the window shows Settings alone", shown.get("mode") is True, shown)
    check("with the server's tabs beside This Mac", shown.get("server_tabs") == ["account", "workers", "notifications", "integrations"], shown)
    pg = page()
    check("Settings is open on the This Mac tab", pg.get("open") and pg.get("tab_shown") and pg.get("active") == "mac", pg)
    for want in ("Workspaces", "Computer use", "Permissions", "Quick Ask", "When amux opens", "Show in menu bar",
                 "Notifications", "Show count on Dock icon", "Zoom all windows together", "Snap windows to a grid", "Updates", "Server"):
        check(f"This Mac shows {want}", want in pg.get("text", ""), pg.get("text", "")[:300])
    check("one row per macOS permission, as the app reports them", pg.get("perms") == len(st.get("permissions", [])) > 0, (pg.get("perms"), len(st.get("permissions", []))))
    check("the Dock count switch shows the app's value (on)", pg.get("dock") is True, pg)

    js("document.getElementById('mac-set-dockBadge').click(); return 'clicked'")
    time.sleep(1.5)
    check("switching it off there reaches the app's config", config().get("dockBadge") is False, config().get("dockBadge"))
    check("the app logs the change", "verdict=settings_action action=toggle name=dockBadge" in log(), log()[-400:])
    check("and its state says off", state()["mac_settings"]["toggles"]["dockBadge"] is False, state()["mac_settings"]["toggles"])
    pg = page()
    check("the page redraws from the app's answer (still off)", pg.get("dock") is False, pg)

    # update-check sends no redraw of its own, so only the Updater's hook can.
    draws = lambda: int(js("return document.getElementById('mac-settings').dataset.draws || '0'") or 0)
    before = draws()
    js("document.getElementById('mac-upd-check').click(); return 'ok'")
    end, after = time.time() + 30, before
    while time.time() < end and after == before:
        time.sleep(0.5)
        after = draws()
    check("Check for Updates redraws the tab's Updates row", after > before, (before, after))

    js("document.getElementById('mac-ws-name').value = 'E2E desk'; document.getElementById('mac-ws-save').click(); return 'ok'")
    time.sleep(3)
    check("a workspace saved there is in the config", any(l.get("name") == "E2E desk" for l in config().get("layouts", [])), config().get("layouts"))
    check("and listed on the tab", "E2E desk" in page().get("text", ""), page().get("text", "")[:300])

    # Settings stays Settings: a page change inside it goes to the amux window.
    mark = len(log())
    js("location.href = '/#view=board'; return 'ok'")
    time.sleep(2)
    sw = state().get("settings_window") or {}
    check("a link out of Settings opens in the amux window", "verdict=settings_window_link to=server" in log()[mark:], log()[mark:][-400:])
    check("and the Settings window still shows Settings", "settings_window=1" in sw.get("url", ""), sw)

    # Opening it again is the same window, not a reload or a second one.
    mark = len(log())
    send("amux://test/menu?title=" + urllib.parse.quote("Settings…"), 2)
    check("Settings… again does not reload the page", "verdict=settings_window_shown" not in log()[mark:], log()[mark:][-400:])
    check("and Settings is still open in it", page().get("open") is True)

    for gone in ("Show in Menu Bar", "Notify When amux Needs You", "Show Count on Dock Icon", "Zoom All Windows"):
        send("amux://test/menu?title=" + urllib.parse.quote(gone), 0.8)
        check(f"the menus no longer carry {gone!r}", f"verdict=test_menu_chosen title={gone} found=0" in log(), log()[-300:])
finally:
    if p and p.poll() is None:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            p.kill()
    server.terminate()
print("PASS: mac-settings, all checks passed" if fails == 0 else f"mac-settings: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
