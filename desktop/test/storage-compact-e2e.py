#!/usr/bin/env python3
"""Saved page data can be compacted from Settings, and the app does it by itself
when it has grown too big (owner, 2026-11-26, after the app's localStorage log
for the server reached 23 GB: "we need to have ways to clean this in the app").

    desktop/test/storage-compact-e2e.py [path/to/Amux.app]

A quiet second copy of the app, with its own config and its own WebKit data
(Amux Dev), opens one window on a private server that serves this checkout's
dashboard. The page saves a marker and about 2 MB of data. Settings > This Mac
must show the size; Compact must delete the site's storage files and put every
saved item back; a second launch with a 1 MB threshold must compact by itself;
a copy left by a failed write-back must go back at the next launch.
"""
import json, os, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-storage-e2e-")
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


def js(code, timeout=20):
    out = ST + ".js"
    if os.path.exists(out):
        os.remove(out)
    send("amux://test/js-pane?i=0&code=%s" % urllib.parse.quote(code), 0)
    end = time.time() + timeout
    while time.time() < end and not os.path.exists(out):
        time.sleep(0.1)
    return open(out).read() if os.path.exists(out) else "no answer"


def wait_log(needle, mark, timeout=40):
    end = time.time() + timeout
    while time.time() < end and needle not in log()[mark:]:
        time.sleep(0.3)
    return needle in log()[mark:]


def launch(extra):
    global p
    json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
               "windows": [{"url": url + "/#view=board", "frame": "{{60, 60}, {1100, 780}}"}]}, open(CFG, "w"))
    mark = len(log())
    p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST, **extra),
                         stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
    wait_log("verdict=windows_restored", mark)
    time.sleep(4)


def read_after():
    """The pages reload after a compaction: ask again until one answers."""
    got = {}
    for _ in range(6):
        r = js(READ, 10)
        try:
            got = json.loads(r)
            break
        except ValueError:
            got = {"error": r}
            time.sleep(2)
    return got


def stop():
    if p and p.poll() is None:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            p.kill()


MARK = "e2e-%d" % os.getpid()
FILL = ("const big = 'x'.repeat(1 << 19); for (let i = 0; i < 4; i++) localStorage.setItem('e2e_fill_' + i, big + i);"
        " localStorage.setItem('e2e_marker', '%s'); return String(Object.keys(localStorage).length)" % MARK)
READ = "await new Promise(r => setTimeout(r, 1500)); return JSON.stringify({marker: localStorage.getItem('e2e_marker'), keys: Object.keys(localStorage).length, fill: (localStorage.getItem('e2e_fill_3') || '').length})"
try:
    # 1. Settings shows the size, and Compact keeps every saved item.
    launch({"AMUX_DESKTOP_STORAGE_AUTO_MB": "100000"})
    check("the app logs its saved page data at launch", "verdict=web_storage_size bytes=" in log(), log()[-400:])
    keys_before = int(js(FILL) or 0)
    time.sleep(2)
    st = state()["mac_settings"].get("storage") or {}
    check("Settings state measures saved page data", st.get("measured") is True and st.get("bytes", 0) > (1 << 20), st)
    check("and it is under the threshold, so not flagged", st.get("large") is False, st)

    send("amux://test/menu?title=" + urllib.parse.quote("Settings…"), 2.5)
    shown = js("await new Promise(r => setTimeout(r, 800)); const s = document.getElementById('mac-storage-size'), b = document.getElementById('mac-storage-compact');"
               " return JSON.stringify({text: s ? s.innerText : '', bytes: s ? +s.dataset.bytes : 0, button: !!b})")
    try:
        shown = json.loads(shown)
    except ValueError:
        shown = {"error": shown}
    check("This Mac shows 'Saved page data' with a size", "Saved page data:" in shown.get("text", "") and shown.get("bytes", 0) > 0, shown)
    check("and a Compact button", shown.get("button") is True, shown)

    # The app's limits are in the page too, and a number typed there reaches the config.
    lim = js("const i = document.getElementById('mac-limit-AMUX_DESKTOP_TAB_MB_LIMIT'); if (!i) return 'missing';"
             " i.value = '2048'; i.dispatchEvent(new Event('change')); return document.querySelectorAll('.mac-limit-input').length")
    time.sleep(1.5)
    check("This Mac lists the app's six limits", lim == "6", lim)
    cfg = json.load(open(CFG))
    check("a limit typed there is saved in the app's config", (cfg.get("limits") or {}).get("AMUX_DESKTOP_TAB_MB_LIMIT") == 2048, cfg.get("limits"))
    lims = {l["key"]: l for l in state()["mac_settings"].get("limits", [])}
    check("and the app now uses it", lims.get("AMUX_DESKTOP_TAB_MB_LIMIT", {}).get("value") == 2048, lims.get("AMUX_DESKTOP_TAB_MB_LIMIT"))
    check("a limit set by the environment says so and cannot be edited", lims.get("AMUX_DESKTOP_STORAGE_AUTO_MB", {}).get("fromEnv") is True, lims.get("AMUX_DESKTOP_STORAGE_AUTO_MB"))
    js("const i = document.getElementById('mac-limit-AMUX_DESKTOP_TAB_MB_LIMIT'); i.value = ''; i.dispatchEvent(new Event('change')); return 'ok'")
    time.sleep(1.5)
    check("emptying the field goes back to the default", "AMUX_DESKTOP_TAB_MB_LIMIT" not in (json.load(open(CFG)).get("limits") or {}), json.load(open(CFG)).get("limits"))

    mark = len(log())
    js("document.getElementById('mac-storage-compact').click(); return 'clicked'")
    check("Compact in the page runs a compaction", wait_log("verdict=web_storage_compacted why=settings", mark), log()[mark:][-600:])
    line = next((l for l in log()[mark:].splitlines() if "verdict=web_storage_compacted" in l), "")
    kept = int(line.split(" keys=")[1].split()[0]) if " keys=" in line else -1
    check("it put back every key the page had (at least the ones it saved; the page adds a few more)", kept >= keys_before, (kept, keys_before, line))
    after = read_after()
    check("after the reload the marker is still there", after.get("marker") == MARK, after)
    check("and the large items too", after.get("fill") == (1 << 19) + 1, after)
    check("the site's write-ahead log was measured before compaction", " before_log=" in line, line)
    check("no failure was logged", "web_storage_compact_failed" not in log()[mark:], log()[mark:][-400:])
    stop()

    # 2. Past the threshold, a launch compacts by itself.
    launch({"AMUX_DESKTOP_STORAGE_AUTO_MB": "1", "AMUX_DESKTOP_STORAGE_FIRST_S": "6"})
    mark = log().rfind("verdict=windows_restored")
    check("a launch with more than 1 MB saved compacts by itself", wait_log("verdict=web_storage_compacted why=launch", mark, 60), log()[mark:][-600:])
    after = read_after()
    check("and the marker survives that too", after.get("marker") == MARK, after)
    stop()

    # 3. A copy left by a compaction whose write-back failed goes back at the next launch.
    copy_file = os.path.expanduser("~/Library/Application Support/com.amux.desktop.dev/localstorage-copy.json")
    os.makedirs(os.path.dirname(copy_file), exist_ok=True)
    json.dump({"e2e_restored": MARK}, open(copy_file, "w"))
    launch({"AMUX_DESKTOP_STORAGE_AUTO_MB": "100000", "AMUX_DESKTOP_STORAGE_FIRST_S": "6"})
    mark = log().rfind("verdict=windows_restored")
    check("a kept copy is written back at launch", wait_log("verdict=web_storage_copy_restored keys=1", mark, 40), log()[mark:][-500:])
    got = js("await new Promise(r => setTimeout(r, 1500)); return localStorage.getItem('e2e_restored') || 'missing'")
    for _ in range(5):
        if got == MARK:
            break
        time.sleep(2)
        got = js("return localStorage.getItem('e2e_restored') || 'missing'")
    check("and the page has the item", got == MARK, got)
    check("and the copy file is gone once the page has it", not os.path.exists(copy_file), copy_file)
    stop()

    # 4. The hourly check puts a kept copy back too, and reloads the pages so
    #    they read it (review F2: it restored without reloading).
    json.dump({"e2e_restored_hourly": MARK}, open(copy_file, "w"))
    launch({"AMUX_DESKTOP_STORAGE_AUTO_MB": "100000", "AMUX_DESKTOP_STORAGE_FIRST_S": "900", "AMUX_DESKTOP_STORAGE_TICK_S": "12"})
    mark = log().rfind("verdict=windows_restored")
    js("window.__e2e_before_reload = 1; return 'set'")
    check("the hourly check writes a kept copy back", wait_log("verdict=web_storage_copy_restored keys=1 why=background", mark, 40), log()[mark:][-500:])
    time.sleep(3)
    got = js("return JSON.stringify({item: localStorage.getItem('e2e_restored_hourly'), reloaded: !window.__e2e_before_reload})")
    try:
        got = json.loads(got)
    except ValueError:
        got = {"error": got}
    check("and the page has the item", got.get("item") == MARK, got)
    check("and the page reloaded to read it", got.get("reloaded") is True, got)
    check("and the copy file is gone", not os.path.exists(copy_file), copy_file)
finally:
    stop()
    server.terminate()
print("PASS: storage-compact, all checks passed" if fails == 0 else f"storage-compact: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
