#!/usr/bin/env python3
"""The owner's side of computer use on this Mac, end to end (owner,
2026-11-22: "Do them all", "Make sure u test them all e2e"):

- a worker cannot grant itself apps (AMUX_MAC_* is owner-only)
- an app it was not given is refused AND files a Needs-you card; the owner's
  Approve on that card grants the app, and the next call goes through
- it yields to the person: while the owner types in the app in front, an
  action waits, then is refused person_active
- an action that pulls its app forward gives the owner's app back
- after reading email, it may read apps but not act, until the owner approves
  the card that files, which opens 30 minutes
- every call is in the activity feed, newest first

    desktop/test/computer-mac-gates-e2e.py [path/to/Amux.app] [amux-server binary]

The person at the Mac (the app in front, seconds since they typed) is scripted
through the private server's test file (AMUX_MAC_TEST=1). The only real action
is move_cursor, which moves the driver's own cursor and not the owner's.
"""
import json, os, shutil, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "Amux.app")
SERVER = sys.argv[2] if len(sys.argv) > 2 else os.path.expanduser("~/.local/bin/amux-server-rs")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-computer-gates-e2e-")
HOME = os.path.join(TMP, "home")
for d in ("tmux", "sessions", "computer"):
    os.makedirs(os.path.join(HOME, d))
CFG, LOG = os.path.join(TMP, "config.json"), os.path.join(TMP, "app.log")
CTX = ssl.create_default_context(); CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE
OWNER_FILE = "auth" + "_token"
W = "gates-e2e-worker"
PERSON = os.path.join(HOME, "computer", "test-person.json")
fails = 0


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:700]}"))
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


def api(method, path, body=None, worker=W):
    h = {"Content-Type": "application/json"}
    if worker:
        h["X-Amux-Session"] = worker
    else:
        h["Authorization"] = "Bearer " + read(os.path.join(HOME, OWNER_FILE)).strip()
    req = urllib.request.Request(URL + path, method=method, headers=h, data=json.dumps(body).encode() if body is not None else None)
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=70) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw.strip().startswith((b"{", b"[")) else {"raw": raw[:200].decode(errors="replace")})
    except urllib.error.HTTPError as e:
        raw = e.read() or b"{}"
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"raw": raw[:200].decode(errors="replace")}


def mac(tool, args):
    return api("POST", "/api/computer/mac/call", {"tool": tool, "args": args})


def person(**kw):
    json.dump(kw, open(PERSON, "w"))


def worker_env():
    return read(os.path.join(HOME, "sessions", W + ".env"))


def card(kind):
    code, d = api("GET", "/api/approvals/cards", worker=None)
    rows = d if isinstance(d, list) else d.get("cards") or d.get("items") or []
    return next((c for c in rows if (c.get("approval_ref") or "") == f"capability:{W}:{kind}"), None), (code, d)


def approve(kind):
    return api("POST", "/api/approvals/decide", {"approval_ref": f"capability:{W}:{kind}", "decision": "approve"}, worker=None)


srv = os.path.join(TMP, "amux-server")
shutil.copy(SERVER, srv)
senv = {k: v for k, v in os.environ.items() if not k.startswith(("TMUX", "AMUX_SESSION", "AMUX_URL", "AMUX_MAC_"))}
senv.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(port), AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(HOME, "tmux"), AMUX_MAC_TEST="1")
slog = open(os.path.join(TMP, "server.log"), "w")
sp = subprocess.Popen([srv], env=senv, stdout=slog, stderr=subprocess.STDOUT)
json.dump({"connections": [{"name": "e2e", "url": URL}], "lastUrl": URL, "cleanExit": True, "launch": "last",
           "computerUse": True, "menuBar": True,
           "windows": [{"url": URL + "/#view=sessions", "frame": "{{80, 80}, {900, 640}}", "tabGroup": 1, "selected": True}]},
          open(CFG, "w"))
aenv = dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=os.path.join(TMP, "state.json"), AMUX_HOME=HOME)
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
    check("the private server and the driver are up", sp.poll() is None and wait(lambda: os.path.exists(EP), 20), read(LOG)[-400:])
    wait(lambda: api("GET", "/api/computer/mac/status")[1].get("up"), 20)
    # The Dock: always running, with windows, and not on the block list (Finder is,
    # since it opens anything as the owner). The person at the Mac is scripted.
    finder = int(subprocess.run(["pgrep", "-x", "Dock"], capture_output=True, text=True).stdout.split()[0])
    person(front="com.apple.dock", idle_s=30)
    code, lw = mac("list_windows", {"pid": finder})
    wid = next((x["window_id"] for x in (lw.get("result") or {}).get("windows", []) if x.get("window_id")), None)
    check("the Dock has a window to read (list_windows needs no grant)", code == 200 and wid, (code, str(lw)[:300]))
    READ = {"pid": finder, "window_id": wid, "include_screenshot": False, "max_elements": 5}

    # 1. A worker cannot grant itself an app.
    code, r = api("PUT", "/api/scope", {"level": "worker", "name": W, "capability": "env", "value": {"AMUX_MAC_APPS": "*"}})
    check("a worker cannot grant itself apps (AMUX_MAC_APPS is owner-only)", code == 403 and "AMUX_MAC_APPS" not in worker_env(), (code, r))

    # 2. Refused, a card is filed, the owner approves, the next call goes through.
    code, r = mac("get_window_state", READ)
    check("an app the worker was not given is refused", code == 403 and r.get("code") == "app_not_allowed", (code, r))
    c, raw = wait(lambda: (lambda x: x if x[0] else None)(card("mac_apps")), 10) or card("mac_apps")
    check("and the owner gets a Needs-you card for it", c is not None, raw)
    print("    card:", (c or {}).get("title"))
    code, r = approve("mac_apps")
    check("the owner's Approve grants the app", code == 200 and "com.apple.dock" in worker_env(), (code, r, worker_env()))
    code, r = mac("get_window_state", READ)
    check("and the next call goes through", code == 200 and r.get("ok"), (code, r))

    # 3. Yield to the person typing in the app in front.
    person(front="com.apple.dock", idle_s=0.2)
    t0 = time.time()
    code, r = mac("move_cursor", {"pid": finder, "x": 20, "y": 20})
    took = time.time() - t0
    check("while the owner types in the app in front, an action waits and is refused", code == 403 and r.get("code") == "person_active", (code, r))
    check("after waiting about 5 seconds", 4.5 <= took <= 12, took)
    person(front="com.apple.dock", idle_s=30)
    code, r = mac("move_cursor", {"pid": finder, "x": 20, "y": 20})
    check("once the owner pauses, it goes ahead", code in (200, 502) and r.get("code") not in ("person_active",), (code, r))
    code, r = mac("get_window_state", READ)
    person(front="com.apple.dock", idle_s=0.1)
    code, r = mac("get_window_state", READ)
    check("reads never wait for the owner", code == 200, (code, r))

    # 4. An action that pulls its app forward gives the owner's app back.
    gb = os.path.join(HOME, "computer", "test-front-given-back")
    person(fronts=["com.apple.TextEdit", "com.apple.TextEdit", "com.apple.dock"], idle_s=30)
    code, r = mac("move_cursor", {"pid": finder, "x": 30, "y": 30})
    check("an action that pulled the Dock forward gives TextEdit back", r.get("front_given_back") == "com.apple.TextEdit"
          or read(gb) == "com.apple.TextEdit", (code, r))
    if os.path.exists(gb):
        os.remove(gb)
    person(fronts=["com.apple.TextEdit", "com.apple.TextEdit", "com.apple.TextEdit"], idle_s=30)
    code, r = mac("move_cursor", {"pid": finder, "x": 30, "y": 30})
    check("and does nothing when the front did not change", not r.get("front_given_back") and not os.path.exists(gb), (code, r))

    # 5. Outside content: read email, then actions wait for the owner.
    person(front="com.apple.dock", idle_s=30)
    # This server has no mail account, so the worker's inbox read fails and
    # notes nothing (a failed read carried no outside words); the Rust test
    # email_read_notes_outside_content runs the real hook on a 200. Here the
    # test writes the note the hook writes, to drive the gate and its card.
    code, r = api("GET", "/api/email/inbox?count=1")
    check("a failed email read notes nothing", code >= 400 and not os.path.exists(os.path.join(HOME, "computer", "outside-content", W + ".json")), (code, r))
    os.makedirs(os.path.join(HOME, "computer", "outside-content"), exist_ok=True); json.dump({"at": int(time.time()), "source": "email"}, open(os.path.join(HOME, "computer", "outside-content", W + ".json"), "w"))
    if True:
        code, r = mac("move_cursor", {"pid": finder, "x": 40, "y": 40})
        check("after which it may not act in an app", code == 403 and r.get("code") == "outside_content", (code, r))
        code, r = mac("get_window_state", READ)
        check("but may still read one", code == 200, (code, r))
        c, raw = card("mac_outside")
        check("and the owner gets a card for it", c is not None, raw)
        code, r = approve("mac_outside")
        until = next((int(l.split("=", 1)[1]) for l in worker_env().splitlines() if l.startswith("AMUX_MAC_OUTSIDE_UNTIL=")), 0)
        check("Approve opens 30 minutes from now", code == 200 and 1700 <= until - time.time() <= 1810, (code, r, worker_env()))
        code, r = mac("move_cursor", {"pid": finder, "x": 40, "y": 40})
        check("within which it may act", r.get("code") != "outside_content", (code, r))

    # 6. The activity feed.
    code, a = api("GET", f"/api/computer/mac/activity?session={W}&limit=50", worker=None)
    rows = a.get("rows") or []
    codes = {x.get("code") for x in rows}
    check("every call is in the activity feed", a.get("measured") and len(rows) >= 8
          and {"app_not_allowed", "person_active", "outside_content"} <= codes and any(x.get("verdict") == "called" for x in rows), (len(rows), codes))
    check("newest first, each with its worker, tool and app", rows == sorted(rows, key=lambda x: -x["ts"])
          and all(x.get("worker") == W and x.get("tool") for x in rows) and any("Dock" in (x.get("apps") or "") for x in rows), rows[:2])
    sl = read(os.path.join(TMP, "server.log"))
    check("the server logs each verdict", all(v in sl for v in ("code=person_active", "verdict=front_given_back", "verdict=called")), sl[-500:])
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
