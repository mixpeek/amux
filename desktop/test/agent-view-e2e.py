#!/usr/bin/env python3
"""What the owner sees and uses while workers use this Mac, end to end
(owner, 2026-11-22: "Do them all", "Make sure u test them all e2e"):

- the live preview ("<worker> is using <App>", its screenshot, Stop, Watch)
  and the outline over the exact window being driven, from the server's
  activity feed, gone 8 seconds after the last step; refusals never show
- both are left out of screen capture
- the menu bar icon shows a dot and says who is using what
- Quick Ask: opens, lists the server's workers, sends a message with the
  selected text and the window you were in attached, and the server has it
- Settings > Permissions lists Accessibility, Screen Recording, Microphone
  and Notifications with their real state

    desktop/test/agent-view-e2e.py [path/to/Amux.app] [amux-server binary]

A quiet test copy (invisible, never in front) talks to a private server. The
window it outlines and attaches is its own; the selection is a stand-in.
"""
import json, os, shutil, signal, socket, ssl, struct, subprocess, sys, tempfile, time, urllib.parse, urllib.request, zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "Amux.app")
SERVER = sys.argv[2] if len(sys.argv) > 2 else os.path.expanduser("~/.local/bin/amux-server-rs")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-agent-view-e2e-")
HOME = os.path.join(TMP, "home")
for d in ("tmux", "sessions", "computer", "computer-screenshots"):
    os.makedirs(os.path.join(HOME, d))
CFG, LOG, ST = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
WSCTL = os.path.join(TMP, "wsctl")
CTX = ssl.create_default_context(); CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE
OWNER_FILE = "auth" + "_token"
W = "view-e2e-worker"
fails = 0
SKIPPED = []


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


def png(path, w=64, h=40):
    raw = b"".join(b"\x00" + bytes([30, 160, 200]) * w for _ in range(h))
    chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    open(path, "wb").write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
                           + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


port = free_port()
URL = f"https://localhost:{port}"


def owner(method, path, body=None):
    h = {"Content-Type": "application/json", "Authorization": "Bearer " + read(os.path.join(HOME, OWNER_FILE)).strip()}
    req = urllib.request.Request(URL + path, method=method, headers=h, data=json.dumps(body).encode() if body is not None else None)
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=30) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def send(pid, url):
    subprocess.run([WSCTL, "send", str(pid), url], capture_output=True)


def state(pid):
    before = os.path.getmtime(ST) if os.path.exists(ST) else 0
    send(pid, "amux://test/state")
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(ST) or os.path.getmtime(ST) == before):
        time.sleep(0.05)
    return json.load(open(ST))


def activity(row):
    with open(os.path.join(HOME, "computer", "mac-activity.jsonl"), "a") as f:
        f.write(json.dumps(row) + "\n")


def rect(s):  # "{{x, y}, {w, h}}"
    n = [float(x) for x in s.replace("{", "").replace("}", "").split(",")]
    return n


assert subprocess.run(["xcrun", "swiftc", "-O", "-o", WSCTL, os.path.join(HERE, "wsctl.swift")]).returncode == 0
srv = os.path.join(TMP, "amux-server")
shutil.copy(SERVER, srv)
senv = {k: v for k, v in os.environ.items() if not k.startswith(("TMUX", "AMUX_SESSION", "AMUX_URL", "AMUX_MAC_"))}
senv.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(port), AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(HOME, "tmux"))
slog = open(os.path.join(TMP, "server.log"), "w")
sp = subprocess.Popen([srv], env=senv, stdout=slog, stderr=subprocess.STDOUT)
json.dump({"connections": [{"name": "e2e", "url": URL}], "lastUrl": URL, "cleanExit": True, "launch": "last",
           "computerUse": True, "menuBar": True,
           "windows": [{"url": URL + "/#view=sessions", "frame": "{{120, 140}, {820, 560}}", "tabGroup": 1, "selected": True}]},
          open(CFG, "w"))
aenv = dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST, AMUX_HOME=HOME)
# ScreenCaptureKit captures no invisible window, so the window-attach check
# needs the copy's window shown: a small one, in a corner, for this run only
# (AMUX_AGENT_VIEW_VISIBLE=1). Without it that check is skipped and says so.
VISIBLE = os.environ.get("AMUX_AGENT_VIEW_VISIBLE") == "1"
if VISIBLE:
    aenv["AMUX_DESKTOP_TEST_VISIBLE"] = "1"
    c = json.load(open(CFG)); c["windows"][0]["frame"] = "{{40, 40}, {420, 300}}"; json.dump(c, open(CFG, "w"))
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
    check("the private server, the app and its driver are up", sp.poll() is None and wait(lambda: os.path.exists(EP), 30)
          and wait(lambda: "verdict=windows_restored" in read(LOG), 30), read(LOG)[-400:])
    code, r = owner("POST", "/api/sessions", {"name": W, "dir": "/tmp", "start": False})
    check("a worker exists on the server", code in (200, 201), (code, r))

    # ── Live preview and outline ──
    s = state(ap.pid)
    win = next(w for w in s["windows"] if w.get("selected"))
    shot = os.path.join(HOME, "computer-screenshots", f"mac-{W}-1.png")
    png(shot)
    activity({"ts": int(time.time() * 1000) - 60_000, "worker": W, "tool": "launch_app", "apps": "Terminal",
              "verdict": "refused", "code": "app_blocked"})
    time.sleep(2.5)
    check("an old refusal shows nothing", not state(ap.pid)["agent_view"]["preview"], state(ap.pid)["agent_view"])
    activity({"ts": int(time.time() * 1000), "worker": W, "tool": "click", "apps": "Amux Dev (com.amux.desktop.dev)",
              "pid": ap.pid, "window_id": win["number"], "verdict": "called", "screenshot": shot})
    av = wait(lambda: (lambda v: v if v["preview"] else None)(state(ap.pid)["agent_view"]), 6) or state(ap.pid)["agent_view"]
    check("a worker's step shows the live preview", av["preview"] and av["headline"] == f"{W} is using Amux Dev", av)
    check("with its screenshot", av["has_image"], av)
    check("and the outline over the exact window it is driving", av["outline"], av)
    if av["outline"]:
        print("    window frame:", win["frame"], " window server:", win.get("cg_frame"), " outline:", av["outline_frame"])
        # Against the window as the window server draws it: an invisible test
        # window that was never composited can disagree with its own NSWindow frame.
        o, f = rect(av["outline_frame"]), rect(win["cg_frame"])
        check("  (the outline's frame is the window's, 3pt outside it)",
              all(abs(a - b) <= 4 for a, b in zip(o, [f[0] - 3, f[1] - 3, f[2] + 6, f[3] + 6])), (o, f))
    check("neither can be captured into a screenshot or a screen share", not av["preview_capturable"] and not av["outline_capturable"], av)
    sm = state(ap.pid)["status_menu"]
    check("the menu bar shows a dot and says who is using what", "●" in sm["title"] and sm["agent"] == f"{W} is using Amux Dev"
          and f"{W} is using Amux Dev" in sm["items"], sm)
    check("the app logs it", f"verdict=agent_view_shown worker={W}" in read(LOG))
    gone = wait(lambda: not state(ap.pid)["agent_view"]["preview"], 14)
    check("8 seconds after the last step it goes away", gone and not state(ap.pid)["agent_view"]["outline"]
          and "●" not in state(ap.pid)["status_menu"]["title"], state(ap.pid)["agent_view"])

    # ── Quick Ask ──
    send(ap.pid, "amux://test/quickask?show=1")
    qa = wait(lambda: (lambda q: q if W in q["workers"] else None)(state(ap.pid)["quick_ask"]), 10) or state(ap.pid)["quick_ask"]
    check("Quick Ask opens and lists the server's workers", qa["visible"] and W in qa["workers"], qa)
    check("and cannot be captured either", not qa["capturable"], qa)
    msg = f"quick ask check {int(time.time())}"
    send(ap.pid, "amux://test/quickask?" + urllib.parse.urlencode({"text": msg, "worker": W, "selection": "the selected words",
                                                                    "attach_selection": "1", "window_self": "1", "attach_window": "1", "send": "1"},
                                                                   quote_via=urllib.parse.quote))
    ls = wait(lambda: state(ap.pid)["quick_ask"]["last_sent"] or None, 20) or {}
    check("Send delivers it", ls.get("ok") is True and ls.get("worker") == W, ls)
    check("with the selected text attached", "selection" in (ls.get("attached") or []), ls)
    front = subprocess.run(["lsappinfo", "info", "-only", "bundleid", subprocess.run(["lsappinfo", "front"], capture_output=True, text=True).stdout.strip()],
                           capture_output=True, text=True).stdout
    if "com.apple.loginwindow" in front:
        SKIPPED.append("window attach: this Mac's screen is locked, and macOS captures no window while it is")
    elif not VISIBLE:
        SKIPPED.append("window attach: a quiet copy's window is invisible and macOS captures none; run with AMUX_AGENT_VIEW_VISIBLE=1")
    else:
        check("and the window you were in", "window" in (ls.get("attached") or []), ls)
        img = next((l.split(": ", 1)[1] for l in (ls.get("text") or "").splitlines() if l.startswith("The window I was in: ")), "")
        check("  (the window is a PNG on disk)", img.endswith(".png") and os.path.getsize(img) > 100 if img else False, img)
    code, h = owner("GET", f"/api/history?session={W}&limit=5")
    rows = h if isinstance(h, list) else h.get("items") or h.get("rows") or h.get("history") or []
    check("the server has the message, attachments included", any(msg in (x.get("text") or "") and "the selected words" in (x.get("text") or "") for x in rows), (code, str(h)[:500]))
    check("and the panel closed", not state(ap.pid)["quick_ask"]["visible"])

    # ── Permissions ──
    s = state(ap.pid)
    perms = {p["id"]: p["state"] for p in s["permissions"]}
    check("Permissions lists the four, each with a state", set(perms) == {"accessibility", "screen", "microphone", "notifications"}
          and all(v in ("on", "off", "not set up", "unknown") for v in perms.values()), perms)
    check("and Settings shows a row for each", s["settings_permission_rows"] == 4, s["settings_permission_rows"])
    print("    this copy's permissions:", perms)
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
for s_ in SKIPPED:
    print("  SKIP " + s_)
print(f"{'PASS' if fails == 0 else 'FAIL'}: {fails} failing check(s), {len(SKIPPED)} skipped (logs {TMP})")
sys.exit(1 if fails else 0)
