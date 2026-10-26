#!/usr/bin/env python3
"""Does one page of the Mac app keep growing? (Ethan, 2026-10-25: "fix the
single tab growth too".)

    desktop/test/tab-growth.py [path/to/Amux.app] [--minutes 15] [--view '#peek=mixpeek-general&tab=terminal']

The desktop-resources reports showed worker terminal pages at 1.0-1.7 GB
while other pages sat at 150-600 MB. This runs a second copy of the app (its
own config, AMUX_DESKTOP_TEST=1) on a virtual display against the local
server with ONE shown tab on --view, samples that page's phys_footprint every
30s, and reports the growth rate after the first two minutes (load and first
renders). A busy worker is the honest specimen: it is the page that grew.

Read only: it opens a page and messages no worker. Exit 1 when the page grew
faster than --max-mb-per-min (default 4).
"""
import json, os, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
args = sys.argv[1:]
APP = args[0] if args and not args[0].startswith("--") else os.path.join(HERE, "..", "..", "Amux.app")
opt = lambda k, d: args[args.index(k) + 1] if k in args else d
MINUTES = float(opt("--minutes", 15))
VIEW = opt("--view", "#peek=mixpeek-general&tab=terminal")
LIMIT = float(opt("--max-mb-per-min", 4))
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-growth-")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def rect(s):
    return tuple(float(v) for v in s.replace("{", "").replace("}", "").split(","))


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
displays = lambda: [json.loads(l) for l in sh(os.path.join(TMP, "wsctl"), "displays").stdout.splitlines() if l.strip()]
before = displays()
assert sh("clang", "-fobjc-arc", "-o", os.path.join(TMP, "vdisplay"), os.path.join(HERE, "vdisplay.m"),
          "-framework", "Foundation", "-framework", "CoreGraphics").returncode == 0
vdisp = subprocess.Popen([os.path.join(TMP, "vdisplay"), "amux-growth", "4546", "1920", "1080"], stdout=subprocess.DEVNULL)
new, end = [], time.time() + 15
while not new and time.time() < end:
    time.sleep(0.5)
    new = [d for d in displays() if d["uuid"] not in {x["uuid"] for x in before}]
assert new, "the virtual display did not connect"
time.sleep(2)
vx, vy, vw, vh = rect(new[0]["visible"])
frame = "{{%d, %d}, {%d, %d}}" % (vx + 40, vy + 40, vw * 0.8, vh * 0.85)
with open(CFG, "w") as f:
    json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
               "windows": [{"url": url + "/" + VIEW, "frame": frame, "selected": True}]}, f)
p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE),
                     stdout=open(LOG, "a"), stderr=subprocess.STDOUT)


def state():
    before = os.path.getmtime(STATE) if os.path.exists(STATE) else 0
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/state")
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(STATE) or os.path.getmtime(STATE) == before):
        time.sleep(0.05)
    return json.load(open(STATE))


samples = []
try:
    t0 = time.time()
    while time.time() - t0 < MINUTES * 60:
        time.sleep(30)
        try:
            pg = state()["pages"][0]
        except (OSError, ValueError, KeyError, IndexError):
            continue
        samples.append((round((time.time() - t0) / 60, 2), pg["mb"]))
        print(f"  {samples[-1][0]:>5} min  {pg['mb']:>5} MB  {pg['view']}", flush=True)
finally:
    p.terminate()
    try:
        p.wait(15)
    except subprocess.TimeoutExpired:
        p.kill()
    vdisp.terminate()

steady = [s for s in samples if s[0] >= 2]
if len(steady) < 3:
    print("growth: not measured (fewer than 3 samples after the first 2 minutes)")
    sys.exit(2)
# Least-squares slope over the steady samples, MB per minute.
n = len(steady)
mx, my = sum(t for t, _ in steady) / n, sum(m for _, m in steady) / n
slope = sum((t - mx) * (m - my) for t, m in steady) / sum((t - mx) ** 2 for t, _ in steady)
print(f"growth: {slope:+.1f} MB/min over {steady[-1][0] - steady[0][0]:.1f} min ({n} samples), "
      f"{steady[0][1]} -> {steady[-1][1]} MB, peak {max(m for _, m in samples)} MB, view {VIEW}")
sys.exit(1 if slope > LIMIT else 0)
