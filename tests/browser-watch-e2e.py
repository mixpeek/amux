#!/usr/bin/env python3
"""Learn a web task by watching it done once (owner, 2026-11-22: "Do them
all", from research/athena-mac-app-teardown.md: Athena records the person's
clicks with secret fields redacted).

    python3 tests/browser-watch-e2e.py [amux-server binary]

A private amux server launches its own headless Chrome on a fresh profile and
opens a form served by this test. The test starts a watch, then does the task
through the browser's real input (clicks and typing, as a person's hands
would): a name, a password, a card number, a checkbox, Enter, then a link to
a page on ANOTHER origin and a button there. Checks the steps come back in
order with what was clicked and filled, the password and the card number are
"[redacted]" and appear nowhere in the steps or the server log, the watch
survives the cross-origin navigation, stop ends it and leaves the page
unwatched, and the CLI prints the steps.
"""
import http.server, json, os, shutil, socket, socketserver, ssl, subprocess, sys, tempfile, threading, time, urllib.request

ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
SERVER = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/.local/bin/amux-server-rs")
TMP = tempfile.mkdtemp(prefix="amux-watch-e2e-")
HOME = os.path.join(TMP, "amux")
os.makedirs(os.path.join(HOME, "tmux"))
os.makedirs(os.path.join(HOME, "playwright-auth", "profiles", "watch-e2e"))
CTX = ssl.create_default_context(); CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE
OWNER_FILE = "auth" + "_token"
S = "watch-e2e"
SECRET_PW, SECRET_CARD = "hunter2-Sup3rSecret", "4242424242424242"
fails = 0


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:800]}"))
    fails += 0 if ok else 1


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


# Two origins: the form, and the page its link goes to.
site_a, site_b = os.path.join(TMP, "a"), os.path.join(TMP, "b")
os.makedirs(site_a); os.makedirs(site_b)
pa, pb = free_port(), free_port()
open(os.path.join(site_b, "next.html"), "w").write(
    "<!doctype html><title>Step two</title><h1>Step two</h1>"
    "<label>Your name <input id='name' name='name'></label>"
    "<label>Card number <input id='card' name='cc-number' autocomplete='cc-number'></label>"
    "<label>Password <input id='pw' type='password' name='password'></label>"
    "<button id='finish'>Finish booking</button>")
open(os.path.join(site_a, "form.html"), "w").write(f"""<!doctype html><title>Book a table</title>
<form onsubmit="event.preventDefault()">
  <label><input id="agree" type="checkbox" name="agree"> I agree</label>
  <button id="go" type="submit">Continue</button>
</form>
<a id="next" href="http://localhost:{pb}/next.html">Next step</a>""")


def serve(d, p):
    class H(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **k): super().__init__(*a, directory=d, **k)
        def log_message(self, *a): pass
    srv = socketserver.TCPServer(("127.0.0.1", p), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


web = [serve(site_a, pa), serve(site_b, pb)]
port = free_port()
URL = f"https://localhost:{port}"
srv = os.path.join(TMP, "amux-server")
shutil.copy(SERVER, srv)
env = {k: v for k, v in os.environ.items() if not k.startswith(("TMUX", "AMUX_SESSION", "AMUX_URL"))}
env.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(port), AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(HOME, "tmux"))
slog_path = os.path.join(TMP, "server.log")
slog = open(slog_path, "w")
sp = subprocess.Popen([srv], env=env, stdout=slog, stderr=subprocess.STDOUT)


def call(method, path, body=None, timeout=60):
    h = {"Content-Type": "application/json", "X-Amux-Session": S,
         "Authorization": "Bearer " + open(os.path.join(HOME, OWNER_FILE)).read().strip()}
    req = urllib.request.Request(URL + path, method=method, headers=h, data=json.dumps(body).encode() if body is not None else None)
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=timeout) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def act(**kw):
    code, r = call("POST", "/api/browser/action", dict(kw, session=S))
    if code != 200:
        print("    action", kw, "->", code, str(r)[:200])
    time.sleep(0.35)
    return code


def click(sel):
    """A real pointer click at the element, as a hand would (focus moves with it)."""
    code, r = call("POST", "/api/browser/action", {"action": "eval", "session": S, "script":
        f"(function(){{var r=document.querySelector({json.dumps(sel)}).getBoundingClientRect();return {{x:r.left+r.width/2,y:r.top+r.height/2}};}})()"})
    pt = r.get("result") if isinstance(r.get("result"), dict) else (r.get("value") or {})
    if not isinstance(pt, dict) or "x" not in pt:
        print("    could not place", sel, str(r)[:200]); return act(action="click", selector=sel)
    return act(action="click", x=pt["x"], y=pt["y"])


try:
    for _ in range(180):
        try:
            if urllib.request.urlopen(URL + "/health", context=CTX, timeout=2).status == 200 and os.path.exists(os.path.join(HOME, OWNER_FILE)):
                break
        except OSError:
            pass
        time.sleep(0.5)
    code, r = call("POST", "/api/browser/start", {"profile": "watch-e2e", "url": f"http://127.0.0.1:{pa}/form.html", "session": S}, 120)
    check("the server's own headless Chrome opens the form", code == 200, (code, r))
    time.sleep(1.5)

    code, r = call("POST", "/api/browser/watch/start", {"session": S})
    check("a watch starts on that tab", code == 200 and r.get("watch", {}).get("watching"), (code, r))

    # After a cross-origin navigation the headless tab can stop answering the
    # driver for 30-40s, with or without a watch (a browser-driver issue of its
    # own, measured 2026-11-22). The test waits for it to answer again; the
    # watch rides it out by reconnecting (verdict=watch_reconnect).
    click("#agree")
    click("#go")
    click("#next")
    t0 = time.time()
    while time.time() - t0 < 90:
        code, r = call("POST", "/api/browser/action", {"action": "eval", "session": S, "script": "location.href"}, 40)
        if code == 200 and "next.html" in json.dumps(r):
            break
        time.sleep(2)
    print(f"    the tab answered again after {time.time() - t0:.0f}s")
    click("#name"); act(action="type", text="Ada Lovelace")
    click("#card"); act(action="type", text=SECRET_CARD)
    click("#pw"); act(action="type", text=SECRET_PW)
    click("#finish")
    time.sleep(1.0)

    code, live = call("GET", f"/api/browser/watch?session={S}")
    check("the steps can be read while watching", code == 200 and live.get("watching") and live.get("n_considered", 0) >= 6, (code, live))

    code, r = call("POST", "/api/browser/watch/stop", {"session": S})
    w = r.get("watch", {})
    steps = w.get("steps") or []
    for s in steps:
        t = s.get("target") or {}
        print(f"    {s.get('kind'):6} {t.get('name') or t.get('selector') or ''!s:28} {s.get('value', '')!s:20} {s.get('url', '')}")
    check("stop ends it", code == 200 and w.get("watching") is False, w)
    fills = {(s.get("target") or {}).get("id"): s for s in steps if s.get("kind") == "fill"}
    check("the name is filled as typed", fills.get("name", {}).get("value") == "Ada Lovelace", fills.get("name"))
    check("the password is redacted", fills.get("pw", {}).get("value") == "[redacted]" and fills["pw"].get("redacted") is True, fills.get("pw"))
    check("the card number is redacted (by its autocomplete name)", fills.get("card", {}).get("value") == "[redacted]", fills.get("card"))
    check("the checkbox is recorded checked", fills.get("agree", {}).get("value") == "true", fills.get("agree"))
    blob = json.dumps(steps)
    check("neither secret appears anywhere in the steps", SECRET_PW not in blob and SECRET_CARD not in blob)
    clicks = [(s.get("target") or {}).get("name") for s in steps if s.get("kind") == "click"]
    check("the clicks name what was clicked", "Continue" in clicks and "Next step" in clicks, clicks)
    pages = [s.get("url") for s in steps if s.get("kind") == "page"]
    check("the watch follows the link to another origin", any(f":{pb}/next.html" in (u or "") for u in pages), pages)
    check("and records the click there", "Finish booking" in clicks and steps[-1].get("url", "").endswith("/next.html"), clicks)
    order = [s.get("kind") for s in steps]
    check("in the order it was done", order.index("page") == 0 and clicks.index("Continue") < clicks.index("Next step") < clicks.index("Your name") < clicks.index("Finish booking"), order)

    n = len(steps)
    click("#finish")
    time.sleep(1.0)
    code, after = call("GET", f"/api/browser/watch?session={S}")
    check("after stop, nothing more is recorded", len(after.get("steps") or []) == n and not after.get("watching"), after)
    code, r = call("POST", "/api/browser/action", {"action": "eval", "session": S, "script": "String(window.__amuxWatchOff === true)"})
    check("and the page is told to stop listening", "true" in json.dumps(r), r)

    cli = subprocess.run([os.path.join(ROOT, "amux"), "browser", "watch", "show"], capture_output=True, text=True, timeout=60,
                         env=dict(os.environ, AMUX_API=URL, CC_HOME=HOME, AMUX_SESSION=S))
    check("the CLI prints the steps", cli.returncode == 0 and "Finish booking" in cli.stdout and "[redacted]" in cli.stdout, (cli.returncode, cli.stdout[-400:], cli.stderr[-300:]))
    sl = open(slog_path, errors="replace").read()
    check("the server log has the verdicts and no secret", "verdict=watch_started" in sl and "verdict=watch_stopped" in sl
          and SECRET_PW not in sl and SECRET_CARD not in sl)
finally:
    try:
        call("POST", "/api/browser/stop", {"session": S}, 30)
    except Exception:
        pass
    sp.terminate()
    slog.close()
    for w_ in web:
        w_.shutdown()
print(f"{'PASS' if fails == 0 else 'FAIL'}: {fails} failing check(s) (logs {TMP})")
sys.exit(1 if fails else 0)
