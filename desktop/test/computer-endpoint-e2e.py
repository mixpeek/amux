#!/usr/bin/env python3
"""A copy of the app that quits leaves another copy's computer-use endpoint alone.

    desktop/test/computer-endpoint-e2e.py [path/to/Amux.app]

Every copy of the app reads <AMUX_HOME>/cua/endpoint.json, and a quiet test
copy shares the owner's ~/.amux. On quit, a copy used to delete that file
whether or not it had started a driver, so the server read the owner's
computer use as off while their driver kept running (2026-11-25, twice in one
morning, each time a desktop test ended). Here the file names another
process; a test copy with computer use off starts and quits, and the file must
still be there, byte for byte, with the decision logged.

This copy runs with its own AMUX_HOME, so the owner's file is never touched.
"""
import json, os, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-cuep-e2e-")
CFG, LOG = os.path.join(TMP, "config.json"), os.path.join(TMP, "app.log")
HOME = os.path.join(TMP, "home")
EP = os.path.join(HOME, "cua", "endpoint.json")
fails = 0


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


def log():
    return open(LOG, errors="replace").read() if os.path.exists(LOG) else ""


os.makedirs(os.path.dirname(EP))
# Another copy's driver: this process's parent, so the pid is real and not ours.
other = {"app": "com.amux.desktop", "binary": "/Applications/Amux.app/Contents/Helpers/cua-driver",
         "pid": os.getppid(), "started_at": int(time.time()), "transport": "broker"}
raw = json.dumps(other, sort_keys=True)
open(EP, "w").write(raw)
json.dump({"connections": [], "cleanExit": True, "launch": "new", "computerUse": False}, open(CFG, "w"))

p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_HOME=HOME, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1"),
                     stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
try:
    time.sleep(6)
    check("the copy started", p.poll() is None, log()[-400:])
    p.send_signal(signal.SIGTERM)
    p.wait(20)
    check("it quit", p.returncode is not None)
    check("the other copy's endpoint.json is still there", os.path.exists(EP))
    check("unchanged", os.path.exists(EP) and open(EP).read() == raw)
    check("and the copy says why it kept it", "verdict=computer_use_endpoint_kept why=quit" in log(), log()[-600:])
finally:
    if p.poll() is None:
        p.kill()
print("PASS: computer-endpoint, all checks passed" if fails == 0 else f"computer-endpoint: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
