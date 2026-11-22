#!/usr/bin/env python3
"""Computer use on this Mac's own apps, end to end: the Mac app starts the
bundled cua-driver, a private amux server finds it, and a worker's calls go
through the per-app gate to the driver (owner, 2026-11-22: build Athena's
computer use into amux).

    desktop/test/computer-mac-e2e.py [path/to/Amux.app] [amux-server binary]

A quiet test copy of the app (its own config and AMUX_HOME) talks to a private
server on its own port. The test only READS from the owner's Mac (list_apps,
and get_window_state on Finder): it never clicks, types or launches in a real
app. Checks the driver starts and publishes its endpoint, the tool list is
classified, a worker with no apps listed is refused with a next step, a
listed app reaches the driver, a blocked app stays refused even under *, the
CLI verb works, and Stop Computer Use ends the driver.
"""
import json, os, shutil, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "Amux.app")
SERVER = sys.argv[2] if len(sys.argv) > 2 else os.path.expanduser("~/.local/bin/amux-server-rs")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-computer-mac-e2e-")
HOME = os.path.join(TMP, "home")
os.makedirs(os.path.join(HOME, "tmux"))
os.makedirs(os.path.join(HOME, "sessions"))
CFG, LOG, ST = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
WSCTL = os.path.join(TMP, "wsctl")
CTX = ssl.create_default_context(); CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE
WORKER = "mac-e2e-worker"
fails = 0


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:600]}"))
    fails += 0 if ok else 1
    return ok


def read(p):
    try:
        return open(p, errors="replace").read()
    except FileNotFoundError:
        return ""


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait(pred, secs=15):
    end = time.time() + secs
    while time.time() < end:
        v = pred()
        if v:
            return v
        time.sleep(0.3)
    return pred()


port = free_port()
URL = f"https://localhost:{port}"


def api(method, path, body=None, worker=WORKER):
    h = {"Content-Type": "application/json", "X-Amux-Session": worker}
    req = urllib.request.Request(URL + path, method=method, headers=h, data=json.dumps(body).encode() if body is not None else None)
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=70) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def mac(tool, args):
    return api("POST", "/api/computer/mac/call", {"tool": tool, "args": args})


assert subprocess.run(["xcrun", "swiftc", "-O", "-o", WSCTL, os.path.join(HERE, "wsctl.swift")]).returncode == 0
check("the build carries the driver", os.access(os.path.join(APP, "Contents", "Helpers", "cua-driver"), os.X_OK))
# A copy of the server: the builder may replace the original mid-run.
srv = os.path.join(TMP, "amux-server")
shutil.copy(SERVER, srv)
senv = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE", "AMUX_SESSION", "AMUX_URL", "AMUX_MAC_APPS", "AMUX_MAC_ANY_APP", "AMUX_MAC_FOREGROUND")}
senv.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(port), AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(HOME, "tmux"))
slog = open(os.path.join(TMP, "server.log"), "w")
sp = subprocess.Popen([srv], env=senv, stdout=slog, stderr=subprocess.STDOUT)
json.dump({"connections": [{"name": "e2e", "url": URL}], "lastUrl": URL, "cleanExit": True, "launch": "last",
           "computerUse": True, "menuBar": True,
           "windows": [{"url": URL + "/#view=sessions", "frame": "{{80, 80}, {900, 640}}", "tabGroup": 1, "selected": True}]},
          open(CFG, "w"))
aenv = dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST, AMUX_HOME=HOME)
ap = subprocess.Popen([BIN], env=aenv, stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
EP = os.path.join(HOME, "cua", "endpoint.json")
try:
    def healthy():
        if sp.poll() is not None:
            return True
        try:
            return urllib.request.urlopen(URL + "/health", context=CTX, timeout=2).status == 200
        except OSError:
            return False
    wait(healthy, 90)
    check("the private server is up", sp.poll() is None, read(os.path.join(TMP, "server.log"))[-500:])
    check("the app starts the driver", wait(lambda: "verdict=computer_use_started" in read(LOG), 20), read(LOG)[-800:])
    ep = json.loads(read(EP) or "{}")
    check("and publishes its endpoint", ep.get("socket") and ep.get("pid"), ep)
    print("    app log:", [l.split("amux-desktop: ")[-1] for l in read(LOG).splitlines() if "computer_use" in l])

    code, st = wait(lambda: (lambda r: r if r[1].get("up") else None)(api("GET", "/api/computer/mac/status")), 20) or api("GET", "/api/computer/mac/status")
    check("the server finds the driver", code == 200 and st.get("up") is True, st)
    perms = st.get("permissions") or {}
    print("    permissions:", json.dumps(perms.get("structuredContent", perms))[:300])

    code, tl = api("GET", "/api/computer/mac/tools")
    by = {t["name"]: t["class"] for t in tl.get("tools", [])}
    check("the tool list is measured and classified", tl.get("measured") and by.get("get_window_state") == "app"
          and by.get("list_apps") == "free" and isinstance(by.get("kill_app"), dict), {k: by.get(k) for k in ("get_window_state", "list_apps", "kill_app")})

    code, la = mac("list_apps", {})
    txt = json.dumps(la)
    check("a free tool reaches the driver", code == 200 and la.get("ok") and "Finder" in txt, (code, txt[:400]))
    finder = int(subprocess.run(["pgrep", "-x", "Finder"], capture_output=True, text=True).stdout.split()[0])

    code, r = mac("get_window_state", {"pid": finder, "include_screenshot": False})
    check("an app the worker was not given is refused", code == 403 and r.get("code") == "app_not_allowed" and r.get("sent") is False, (code, r))
    check("with what to do next", "com.apple.finder" in r.get("next", "") and "AMUX_MAC_APPS" in r.get("next", ""), r.get("next"))

    code, r = mac("kill_app", {"pid": finder})
    check("a refused tool is refused", code == 403 and r.get("code") == "tool_refused", (code, r))

    # The owner lists Finder for this worker (its Scope file), and * for everything else.
    open(os.path.join(HOME, "sessions", WORKER + ".env"), "w").write("AMUX_MAC_APPS=com.apple.finder,*\n")
    code, lw = mac("list_windows", {"pid": finder})
    wins = (lw.get("result") or {}).get("structuredContent", {}).get("windows") or []
    wid = next((w.get("window_id") for w in wins if w.get("window_id")), None)
    check("a listed app reaches the driver", code == 200 and lw.get("ok"), (code, json.dumps(lw)[:500]))
    if wid:
        # A 64-pixel thumbnail: proves the screenshot path without keeping a readable copy of the owner's window.
        code, r = mac("get_window_state", {"pid": finder, "window_id": wid, "max_elements": 10, "max_dimension": 64})
        shot = r.get("screenshot")
        print(f"    get_window_state on Finder window {wid}: http {code}", json.dumps(r.get("result") or r.get("error"))[:200])
        if r.get("ok") and shot:
            check("its screenshot is saved for the worker to read", os.path.isfile(shot) and "mac-" + WORKER in shot, shot)
            os.remove(shot)
        else:
            check("the read reaches the driver (it may lack a permission)", code in (200, 502) and r.get("code") in (None, "driver_error"), (code, r))
    else:
        print("    Finder has no window open: skipped the window read")

    code, r = mac("launch_app", {"name": "Terminal"})
    check("a blocked app stays refused, even under *", code == 403 and r.get("code") == "app_blocked", (code, r))
    code, r = mac("click", {"pid": finder, "x": 5, "y": 5, "delivery_mode": "foreground"})
    check("foreground delivery needs its own switch", code == 403 and r.get("code") == "foreground_not_allowed", (code, r))

    cli = subprocess.run([os.path.join(ROOT, "amux"), "computer", "mac", "list_apps"], capture_output=True, text=True, timeout=90,
                         env=dict(os.environ, AMUX_API=URL, CC_HOME=HOME, AMUX_SESSION=WORKER))
    check("the CLI verb works", cli.returncode == 0 and "Finder" in cli.stdout, (cli.returncode, cli.stdout[-300:], cli.stderr[-300:]))
    cli = subprocess.run([os.path.join(ROOT, "amux"), "computer", "mac", "kill_app", '{"pid":1}'], capture_output=True, text=True, timeout=60,
                         env=dict(os.environ, AMUX_API=URL, CC_HOME=HOME, AMUX_SESSION=WORKER))
    check("and prints a refusal with its next step", cli.returncode == 1 and "tool_refused" in cli.stderr and "next:" in cli.stderr, cli.stderr[-300:])

    sl = read(os.path.join(TMP, "server.log"))
    check("every call logs a verdict", "verdict=refused code=app_not_allowed" in sl and "verdict=called" in sl, sl[-600:])

    # Stop Computer Use, as the menu (or ⌘⇧Esc) does it.
    subprocess.run([WSCTL, "send", str(ap.pid), "amux://test/status-menu?title=Stop Computer Use"], capture_output=True)
    check("Stop Computer Use removes the endpoint", wait(lambda: not os.path.exists(EP), 10))
    gone = wait(lambda: subprocess.run(["kill", "-0", str(ep.get("pid", 0))], capture_output=True).returncode != 0, 10)
    check("and ends the driver", gone)
    code, r = mac("list_apps", {})
    check("after which a call says the driver is down", code == 503 and r.get("code") == "driver_down" and r.get("next"), (code, r))
    check("and it stays stopped (no restart)", "verdict=computer_use_stopped reason=owner_stop" in read(LOG)
          and read(LOG).count("verdict=computer_use_started") == 1, read(LOG)[-500:])
finally:
    ap.send_signal(signal.SIGTERM)
    try:
        ap.wait(15)
    except subprocess.TimeoutExpired:
        ap.kill()
    sp.terminate()
    slog.close()
    try:
        pid = json.loads(read(EP) or "{}").get("pid")
        if pid:
            os.kill(pid, signal.SIGTERM)
    except (OSError, ValueError):
        pass
print(f"{'PASS' if fails == 0 else 'FAIL'}: {fails} failing check(s) (logs {TMP})")
sys.exit(1 if fails else 0)
