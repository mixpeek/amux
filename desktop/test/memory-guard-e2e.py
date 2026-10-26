#!/usr/bin/env python3
"""The memory guard reloads a hidden page over its limit and leaves the shown
page alone (desktop/amux-desktop.swift, guardTabMemory).

    desktop/test/memory-guard-e2e.py [path/to/Amux.app]

A second copy of the app on a virtual display, four tabs, with the limit set
to 40 MB, a 4s hidden grace and a 2s tick, so every hidden page qualifies
within seconds. Checks: each hidden page is reloaded (new web process, same
view), the shown page keeps its process, and every reload is logged.
"""
import json, os, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-memguard-")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
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


def rect(s):
    return tuple(float(v) for v in s.replace("{", "").replace("}", "").split(","))


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
displays = lambda: [json.loads(l) for l in sh(os.path.join(TMP, "wsctl"), "displays").stdout.splitlines() if l.strip()]
before = displays()
assert sh("clang", "-fobjc-arc", "-o", os.path.join(TMP, "vdisplay"), os.path.join(HERE, "vdisplay.m"),
          "-framework", "Foundation", "-framework", "CoreGraphics").returncode == 0
vdisp = subprocess.Popen([os.path.join(TMP, "vdisplay"), "amux-memguard", "4646", "1600", "1000"], stdout=subprocess.DEVNULL)
new, end = [], time.time() + 15
while not new and time.time() < end:
    time.sleep(0.5)
    new = [d for d in displays() if d["uuid"] not in {x["uuid"] for x in before}]
assert new, "the virtual display did not connect"
time.sleep(2)
vx, vy, vw, vh = rect(new[0]["visible"])
frame = "{{%d, %d}, {%d, %d}}" % (vx + 40, vy + 40, vw * 0.8, vh * 0.85)
VIEWS = ["#view=sessions", "#view=board", "#view=calendar", "#view=map"]
with open(CFG, "w") as f:
    json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
               "windows": [{"url": url + "/" + v, "frame": frame, "tabGroup": 1, "selected": i == 0} for i, v in enumerate(VIEWS)]}, f)
env = dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE,
           AMUX_DESKTOP_TAB_MB_LIMIT="40", AMUX_DESKTOP_TAB_HIDDEN_S="4", AMUX_DESKTOP_MEM_TICK_S="2")
p = subprocess.Popen([BIN], env=env, stdout=open(LOG, "a"), stderr=subprocess.STDOUT)


def state():
    b = os.path.getmtime(STATE) if os.path.exists(STATE) else 0
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/state")
    e = time.time() + 5
    while time.time() < e and (not os.path.exists(STATE) or os.path.getmtime(STATE) == b):
        time.sleep(0.05)
    return json.load(open(STATE)) if os.path.exists(STATE) else {}


try:
    end = time.time() + 60
    while "verdict=windows_restored" not in log() and time.time() < end:
        time.sleep(0.2)
    end = time.time() + 40
    while time.time() < end:
        st = state()
        if st.get("pages") and all(pg["pid"] > 0 and pg["view"] for pg in st["pages"]):
            break
        time.sleep(0.5)
    first = {pg["view"]: pg for pg in st["pages"]}
    shown = [v for v, pg in first.items() if pg["shown"]]
    print("pages:", {v: (pg["pid"], pg["mb"], pg["shown"]) for v, pg in first.items()})
    check("one page is shown", len(shown) == 1, str(shown))
    time.sleep(14)
    st = state()
    after = {pg["view"]: pg for pg in st["pages"]}
    reloads = log().count("verdict=tab_reloaded_for_memory")
    check("each hidden page over the limit was reloaded once (cooldown), and logged", reloads == len(VIEWS) - 1, f"{reloads} reloads")
    check("every page is still on its own view", sorted(after) == sorted(first), f"{sorted(after)} vs {sorted(first)}")
    for v in shown:
        check(f"the shown page ({v}) was never reloaded", f"view={v}" not in log() and after.get(v, {}).get("pid") == first[v]["pid"],
              f"pid {first[v]['pid']} -> {after.get(v, {}).get('pid')}")
    check("the reload names the page", all(f"view={v}" in log() for v in first if v not in shown), "")
finally:
    p.terminate()
    try:
        p.wait(15)
    except subprocess.TimeoutExpired:
        p.kill()
    vdisp.terminate()
print("PASS: memory-guard, all checks passed" if fails == 0 else f"memory-guard: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
