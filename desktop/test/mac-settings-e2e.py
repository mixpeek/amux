#!/usr/bin/env python3
"""Every setting in one place: amux > Settings… opens the dashboard's Settings
on its This Mac tab, which carries the app's own settings (owner, 2026-11-25:
"consolidate all our settings to the amux Mac app settings").

    desktop/test/mac-settings-e2e.py [path/to/Amux.app]

A second copy of the app, with its own config, opens one window on a private
server that serves this checkout's dashboard. Settings… must open the page's
This Mac tab (not the app's own window). A switch flipped there must reach the
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


def js(code, timeout=15):
    out = ST + ".js"
    if os.path.exists(out):
        os.remove(out)
    send("amux://test/js-pane?i=0&code=%s" % urllib.parse.quote(code), 0)
    end = time.time() + timeout
    while time.time() < end and not os.path.exists(out):
        time.sleep(0.1)
    return open(out).read() if os.path.exists(out) else "no answer"


def page():
    r = js("await new Promise(r => setTimeout(r, 800));"
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

    send("amux://test/menu?title=" + urllib.parse.quote("Settings…"), 2.5)
    check("amux > Settings… is a menu item", "verdict=test_menu_chosen title=Settings… found=1" in log(), log()[-400:])
    check("it opens the dashboard's Settings, not the app's own window", "verdict=settings_in_page" in log() and "verdict=settings_native" not in log(), log()[-600:])
    pg = page()
    check("Settings is open on the This Mac tab", pg.get("open") and pg.get("tab_shown") and pg.get("active") == "mac", pg)
    for want in ("Workspaces", "Computer use", "Permissions", "Quick Ask", "When amux opens", "Show in menu bar",
                 "Notifications", "Show count on Dock icon", "Zoom all windows together", "Snap windows to a grid", "Updates", "Server"):
        check(f"This Mac shows {want}", want in pg.get("text", ""), pg.get("text", "")[:300])
    st = state()
    check("one row per macOS permission, as the app reports them", pg.get("perms") == len(st.get("permissions", [])) > 0, (pg.get("perms"), len(st.get("permissions", []))))
    check("the Dock count switch shows the app's value (on)", pg.get("dock") is True, pg)

    js("document.getElementById('mac-set-dockBadge').click(); return 'clicked'")
    time.sleep(1.5)
    check("switching it off there reaches the app's config", config().get("dockBadge") is False, config().get("dockBadge"))
    check("the app logs the change", "verdict=settings_action action=toggle name=dockBadge" in log(), log()[-400:])
    check("and its state says off", state()["mac_settings"]["toggles"]["dockBadge"] is False, state()["mac_settings"]["toggles"])
    pg = page()
    check("the page redraws from the app's answer (still off)", pg.get("dock") is False, pg)

    # An update check started from the tab redraws its Updates row with no
    # other Settings window ever opened (review F1: the redraw hook was only
    # set when the app's own window was made).
    # update-check sends no redraw of its own, so only the Updater's hook can.
    draws = lambda: int(js("return document.getElementById('mac-settings').dataset.draws || '0'") or 0)
    before = draws()
    js("document.getElementById('mac-upd-check').click(); return 'ok'")
    end, after = time.time() + 30, before
    while time.time() < end and after == before:
        time.sleep(0.5)
        after = draws()
    check("Check for Updates redraws the tab's Updates row", after > before, (before, after))
    check("and the native window was never made for it", "verdict=settings_native" not in log(), log()[-300:])

    js("document.getElementById('mac-ws-name').value = 'E2E desk'; document.getElementById('mac-ws-save').click(); return 'ok'")
    time.sleep(3)
    check("a workspace saved there is in the config", any(l.get("name") == "E2E desk" for l in config().get("layouts", [])), config().get("layouts"))
    check("and listed on the tab", "E2E desk" in page().get("text", ""), page().get("text", "")[:300])

    # Settings opens inside the window wherever the gear is (2026-11-25: the
    # Needs-you approvals above the header pushed the gear to 777px of an 832px
    # window and the menu hung off the bottom, so Settings "could not open").
    def placed():
        r = js("await new Promise(r => setTimeout(r, 400)); const m = document.getElementById('settings-menu'), r = m.getBoundingClientRect();"
               " const t = document.elementFromPoint(r.left + r.width / 2, r.top + Math.min(r.height / 2, 200));"
               " const g = document.getElementById('settings-btn').getBoundingClientRect();"
               " return JSON.stringify({open: m.classList.contains('open'), top: r.top, bottom: r.bottom, left: r.left, right: r.right, vh: innerHeight, vw: innerWidth,"
               " hit: !!(t && m.contains(t)), gear_top: g.top, gear_hit: m.contains(document.elementFromPoint(g.left + g.width / 2, g.top + g.height / 2))})")
        try:
            return json.loads(r)
        except ValueError:
            return {"error": r}
    # A new server starts in the simple layout, which has no header; the
    # owner's has one. "Everything else" is how a person gets it.
    js("closeSettings(); _homeShowEverything(); return 'ok'")
    time.sleep(1.5)
    js("closeSettings(); const d = document.createElement('div'); d.id = 'e2e-push'; d.style.height = (innerHeight - 70) + 'px';"
       " const h = document.querySelector('.header-row'); h.parentNode.insertBefore(d, h); scrollTo(0, 0); return 'pushed'")
    send("amux://test/menu?title=" + urllib.parse.quote("Settings…"), 2.5)
    pl = placed()
    check("the gear really is near the bottom of the window", pl.get("gear_top", 0) > pl.get("vh", 1) - 140, pl)
    check("Settings still opens fully inside the window", pl.get("open") and pl.get("top", -1) >= 0 and pl.get("bottom", 1e9) <= pl.get("vh", 0) + 1
          and pl.get("left", -1) >= 0 and pl.get("right", 1e9) <= pl.get("vw", 0) + 1, pl)
    check("and it is what you see there, not covered", pl.get("hit"), pl)
    check("the gear stays uncovered, so it can close Settings", pl.get("gear_hit") is False, pl)
    js("closeSettings(); document.getElementById('e2e-push').remove(); return 'ok'")
    send("amux://test/menu?title=" + urllib.parse.quote("Settings…"), 2.5)
    pl = placed()
    check("with the gear at the top it opens under the gear", pl.get("open") and pl.get("top", 0) >= pl.get("gear_top", 0) and pl.get("bottom", 1e9) <= pl.get("vh", 0) + 1, pl)
    js("closeSettings(); return 'ok'")

    # A page that does not answer must not leave ⌘, doing nothing.
    mark = len(log())
    js("setTimeout(() => { const t = Date.now() + 5000; while (Date.now() < t) {} }, 50); return 'busy'")
    send("amux://test/menu?title=" + urllib.parse.quote("Settings…"), 3.5)
    check("a page busy for 5 s gets the app's own Settings window after 2 s", "verdict=settings_native reason=page_did_not_answer" in log()[mark:], log()[mark:][-500:])
    time.sleep(3)
    check("and the late answer does not open a second one", log()[mark:].count("verdict=settings_") == 1, log()[mark:][-500:])

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
