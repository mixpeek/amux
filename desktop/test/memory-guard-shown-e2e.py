#!/usr/bin/env python3
"""Does the memory guard bound a page that is ON SCREEN? (Memory RCA
2026-11-15: two worker terminals shown side by side in a pane grid held 13.4
and 6.9 GB. The guard only ever reloaded hidden pages, so a shown page had no
bound at all.)

    desktop/test/memory-guard-shown-e2e.py [path/to/Amux.app]

A quiet test copy (AMUX_DESKTOP_TEST=1, invisible, no dock icon) with a worker
tab that stays selected. The tab is made ~600 MB heavier in script; the shown
limit (AMUX_DESKTOP_SHOWN_TAB_MB_LIMIT=600) must end its web process and load
its view in a new one while it stays shown. Pass: the page shown before and
after, a new pid, the page under the limit, the allocation gone, and the log
saying shown_tab_reloaded_for_memory then tab_reload_freed. Read only: it
opens pages and messages no worker."""
import json, os, subprocess, sys, tempfile, time, urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-guard-", dir=os.environ.get("TMPDIR"))
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
url = subprocess.run(["amux", "url"], capture_output=True, text=True).stdout.strip()
frame = "{{100, 100}, {1400, 900}}"
B = "peek=amux&tab=terminal"
json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
           "windows": [{"url": url + "/#" + B, "frame": frame, "tabGroup": 0, "selected": True},
                       {"url": url + "/#board", "frame": frame, "tabGroup": 0, "selected": False}]}, open(CFG, "w"))
WSCTL = os.path.join(TMP, "wsctl")
assert subprocess.run(["xcrun", "swiftc", "-O", "-o", WSCTL, os.path.join(HERE, "wsctl.swift")]).returncode == 0
env = dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE,
           AMUX_DESKTOP_TAB_MB_LIMIT="100000", AMUX_DESKTOP_SHOWN_TAB_MB_LIMIT="600", AMUX_DESKTOP_TAB_HIDDEN_S="5", AMUX_DESKTOP_MEM_TICK_S="10",
           AMUX_DESKTOP_TAB_RELOAD_COOLDOWN_S="3600")
p = subprocess.Popen([BIN], env=env, stdout=open(LOG, "a"), stderr=subprocess.STDOUT)


def hook(path):
    subprocess.run([WSCTL, "send", str(p.pid), "amux://test/" + path])


def pages():
    try: os.remove(STATE)
    except OSError: pass
    hook("state")
    for _ in range(40):
        time.sleep(0.25)
        try: return json.load(open(STATE)).get("pages", [])
        except (OSError, ValueError): pass
    return []


def mb_of(view):
    return [(pg["mb"], pg["shown"]) for pg in pages() if pg["view"].startswith("#" + B.split("&")[0])]


try:
    end = time.time() + 180
    while time.time() < end and not (len(pages()) >= 2 and all(pg["pid"] > 0 for pg in pages())):
        time.sleep(2)
    time.sleep(10)
    hook("select-view?view=" + urllib.parse.quote(B))
    time.sleep(3)
    hook("js?code=" + urllib.parse.quote("window._ballast=[];for(let i=0;i<60;i++){window._ballast.push(new Uint8Array(1e7).fill(i+1))};0"))
    ps = pages()
    if not ps:
        sys.exit("FAIL: the test copy never reported its pages (log %s)" % LOG)
    idx = max(range(len(ps)), key=lambda i: ps[i]["mb"])
    heavy = [(ps[idx]["mb"], ps[idx]["shown"])]
    old_pid = ps[idx]["pid"]
    log = ""
    for _ in range(12):
        time.sleep(10)
        ps = pages()
        if idx < len(ps): print("t", ps[idx]["pid"], ps[idx]["mb"], ps[idx]["shown"], flush=True)
        log = open(LOG).read()
        if "tab_reload_freed" in log or "tab_reload_did_not_free" in log: break
    out = STATE + ".js"
    try: os.remove(out)
    except OSError: pass
    hook("js-pane?i=0&code=" + urllib.parse.quote("return String(!!window._ballast)"))
    time.sleep(3)
    answer = open(out).read() if os.path.exists(out) else "no answer"
    print("allocation still there?", answer)
    ps = pages()
    after = [(ps[idx]["mb"], ps[idx]["shown"])] if idx < len(ps) else []
    new_pid = ps[idx]["pid"] if idx < len(ps) else 0
    lines = [l[l.find("verdict="):][:160] for l in log.splitlines() if "verdict=" in l and ("tab_reload" in l or "web_process" in l or "shown_tab" in l)]
    print("heavy", heavy, "pid", old_pid, "after", after, "pid", new_pid)
    for l in lines: print(l)
    ok = (heavy and after and heavy[0][0] > 600 and heavy[0][1] and after[0][1] and after[0][0] < 600
          and new_pid not in (0, old_pid) and answer.startswith("false")
          and any("shown_tab_reloaded_for_memory" in l for l in lines) and any("tab_reload_freed" in l for l in lines))
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)
finally:
    p.terminate()
