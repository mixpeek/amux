#!/usr/bin/env python3
"""What the Mac app costs, measured on the built app (Ethan, 2026-10-25:
"improve speed, reduce footprint, make it all faster, with zero degradation").

    desktop/test/perf-bench.py [path/to/Amux.app] [--idle 60] [--json out.json]

Runs a second copy of the app (its own config, AMUX_DESKTOP_TEST=1) on a
virtual display against the local amux server, with one window of the tabs
the owner keeps open: the views below plus two worker pages. Only one tab is
shown, as in real use. Reports:

- launch: seconds from start to the windows restored, and to every page
  having a web process;
- switch: seconds from selecting another tab to that page being shown;
- idle: per-page memory (phys_footprint, MB) and CPU (% of one core) over
  --idle seconds, split into the shown page and the hidden ones, plus the app.

Read only: it opens pages and selects tabs, and messages no worker.
"""
import json, os, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
args = sys.argv[1:]
APP = args[0] if args and not args[0].startswith("--") else os.path.join(HERE, "..", "..", "Amux.app")
IDLE = int(args[args.index("--idle") + 1]) if "--idle" in args else 60
OUT = args[args.index("--json") + 1] if "--json" in args else None
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-perf-")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def log():
    try:
        return open(LOG, errors="replace").read()
    except FileNotFoundError:
        return ""


def rect(s):
    return tuple(float(v) for v in s.replace("{", "").replace("}", "").split(","))


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
# The same two worker pages every run (PERF_WORKERS=a,b), so runs compare.
workers = [w for w in os.environ.get("PERF_WORKERS", "amux,mixpeek-general").split(",") if w]


def displays():
    return [json.loads(l) for l in sh(os.path.join(TMP, "wsctl"), "displays").stdout.splitlines() if l.strip()]


before = displays()
assert sh("clang", "-fobjc-arc", "-o", os.path.join(TMP, "vdisplay"), os.path.join(HERE, "vdisplay.m"),
          "-framework", "Foundation", "-framework", "CoreGraphics").returncode == 0
vdisp = subprocess.Popen([os.path.join(TMP, "vdisplay"), "amux-perf", "4545", "1920", "1080"], stdout=subprocess.DEVNULL)
new, end = [], time.time() + 15
while not new and time.time() < end:
    time.sleep(0.5)
    new = [d for d in displays() if d["uuid"] not in {x["uuid"] for x in before}]
assert new, "the virtual display did not connect"
time.sleep(2)
vx, vy, vw, vh = rect(new[0]["visible"])
frame = "{{%d, %d}, {%d, %d}}" % (vx + 40, vy + 40, vw * 0.8, vh * 0.85)
VIEWS = ["#view=sessions", "#view=board", "#view=messages", "#view=calendar", "#view=files", "#view=map"] + ["#peek=" + w for w in workers]
with open(CFG, "w") as f:
    json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
               "windows": [{"url": url + "/" + v, "frame": frame, "tabGroup": 1, "selected": i == 0} for i, v in enumerate(VIEWS)]}, f)

t0 = time.time()
p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE),
                     stdout=open(LOG, "a"), stderr=subprocess.STDOUT)


def send(u, settle=0.2):
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/" + u)
    time.sleep(settle)


def state():
    before = os.path.getmtime(STATE) if os.path.exists(STATE) else 0
    send("state", 0)
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(STATE) or os.path.getmtime(STATE) == before):
        time.sleep(0.05)
    return json.load(open(STATE))


result = {"views": VIEWS, "workers": workers}
try:
    while "verdict=windows_restored" not in log() and time.time() - t0 < 60:
        time.sleep(0.05)
    result["launch_restored_s"] = round(time.time() - t0, 2)
    # The tab you look at: its page has a process and has posted its name.
    while time.time() - t0 < 60:
        st = state()
        if any(pg["shown"] and pg["pid"] > 0 and pg["view"] for pg in st.get("pages", [])):
            break
        time.sleep(0.05)
    result["launch_shown_s"] = round(time.time() - t0, 2)
    while time.time() - t0 < 90:
        st = state()
        if st.get("pages") and all(pg["pid"] > 0 and pg["view"] for pg in st["pages"]):
            break
        time.sleep(0.2)
    result["launch_pages_s"] = round(time.time() - t0, 2)
    time.sleep(15)   # first fetches and renders settle

    s1, t1 = state(), time.time()
    time.sleep(IDLE)
    s2, t2 = state(), time.time()
    dt = t2 - t1
    pages = []
    for a, b in zip(s1["pages"], s2["pages"]):
        pages.append({"view": b["view"], "shown": b["shown"], "mb": b["mb"],
                      "cpu_pct": round(100 * (b["cpu_s"] - a["cpu_s"]) / dt, 2) if a["cpu_s"] >= 0 and b["cpu_s"] >= 0 else None})
    shown = [x for x in pages if x["shown"]]
    hidden = [x for x in pages if not x["shown"]]
    result["idle"] = {
        "seconds": round(dt),
        "pages": pages,
        "web_mb_total": sum(x["mb"] for x in pages if x["mb"] > 0),
        "hidden_mb_total": sum(x["mb"] for x in hidden if x["mb"] > 0),
        "shown_cpu_pct": round(sum(x["cpu_pct"] or 0 for x in shown), 2),
        "hidden_cpu_pct": round(sum(x["cpu_pct"] or 0 for x in hidden), 2),
        "app_mb": s2["app_mb"],
        "app_cpu_pct": round(100 * (s2["app_cpu_s"] - s1["app_cpu_s"]) / dt, 2),
    }

    # Switching tabs: select each hidden tab and time until its page is shown.
    switches = []
    for v in VIEWS[1:4]:
        title_before = state()
        t = time.time()
        send("select-view?view=" + v.lstrip("#"), 0)
        while time.time() - t < 10:
            st = state()
            if any(pg["view"] == v and pg["shown"] for pg in st["pages"]):
                break
            time.sleep(0.02)
        switches.append(round(time.time() - t, 3))
    result["switch_s"] = switches
finally:
    p.terminate()
    try:
        p.wait(15)
    except subprocess.TimeoutExpired:
        p.kill()
    vdisp.terminate()

i = result.get("idle", {})
print(f"launch: windows {result.get('launch_restored_s')}s, shown tab {result.get('launch_shown_s')}s, every page {result.get('launch_pages_s')}s")
print(f"idle {i.get('seconds')}s: web {i.get('web_mb_total')} MB ({i.get('hidden_mb_total')} MB hidden), "
      f"cpu shown {i.get('shown_cpu_pct')}% hidden {i.get('hidden_cpu_pct')}%, app {i.get('app_mb')} MB {i.get('app_cpu_pct')}%")
for pg in i.get("pages", []):
    print(f"  {'shown ' if pg['shown'] else 'hidden'} {pg['mb']:>5} MB {pg['cpu_pct']:>6}%  {pg['view']}")
print(f"switch: {result.get('switch_s')}")
if OUT:
    json.dump(result, open(OUT, "w"), indent=1)
