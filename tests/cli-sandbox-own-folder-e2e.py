#!/usr/bin/env python3
"""A worker whose shell writes only inside its own folder (owner, 2026-11-22:
"Do them all", from research/athena-mac-app-teardown.md: Athena's "Work in
Folders" runs commands under Seatbelt, writing only inside the folder).

    python3 tests/cli-sandbox-own-folder-e2e.py [amux-server binary]

A private amux server in enforce mode (AMUX_CLI_SANDBOX=enforce) launches a
worker, with a stand-in `claude` on PATH so the launch only writes the
worker's sandbox settings. The owner sets AMUX_CAP_WRITE_FOLDERS=own on it.
Then the REAL Claude Code runs under exactly those settings (haiku, a one-line
task) and is asked to write one file inside the worker's folder and one in a
folder beside it. Passes when the inside write lands and the outside one does
not. A control run under the default (broad) settings writes both, so the
missing file is the sandbox's doing, not a model that skipped the step.
"""
import json, os, shutil, socket, ssl, subprocess, sys, tempfile, time, urllib.request

SERVER = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/.local/bin/amux-server-rs")
CLAUDE = shutil.which("claude")
TMP = tempfile.mkdtemp(prefix="amux-sbx-e2e-")
HOME = os.path.join(TMP, "amux")
os.makedirs(os.path.join(HOME, "tmux"))
# Both folders under the user's home, so "outside" is a write the broad default allows.
BASE = os.path.join(os.path.expanduser("~"), ".amux", "tmp", f"sbx-e2e-{os.getpid()}")
OWN, OUTSIDE = os.path.join(BASE, "own"), os.path.join(BASE, "outside")
os.makedirs(OWN); os.makedirs(OUTSIDE)
FAKE = os.path.join(TMP, "bin")
os.makedirs(FAKE)
open(os.path.join(FAKE, "claude"), "w").write("#!/bin/sh\nexec sleep 600\n")
os.chmod(os.path.join(FAKE, "claude"), 0o755)
CTX = ssl.create_default_context(); CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE
OWNER_FILE = "auth" + "_token"
W = "sbx-e2e"
fails = 0


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:700]}"))
    fails += 0 if ok else 1


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


port = free_port()
URL = f"https://localhost:{port}"
srv = os.path.join(TMP, "amux-server")
shutil.copy(SERVER, srv)
env = {k: v for k, v in os.environ.items() if not k.startswith(("TMUX", "AMUX_SESSION", "AMUX_URL", "AMUX_CAP_"))}
env.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(port), AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(HOME, "tmux"),
           AMUX_CLI_SANDBOX="enforce", PATH=FAKE + ":" + env.get("PATH", ""), AMUX_RS_SCHEDULER="0", AMUX_BOARD_DRIVE_SECS="0")
log = open(os.path.join(TMP, "server.log"), "w")
sp = subprocess.Popen([srv], env=env, stdout=log, stderr=subprocess.STDOUT)


def owner(method, path, body=None):
    h = {"Content-Type": "application/json", "Authorization": "Bearer " + open(os.path.join(HOME, OWNER_FILE)).read().strip()}
    req = urllib.request.Request(URL + path, method=method, headers=h, data=json.dumps(body).encode() if body is not None else None)
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=60) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def settings_after_launch():
    p = os.path.join(HOME, "sandbox", f"{W}.claude-settings.json")
    if os.path.exists(p):
        os.remove(p)
    owner("POST", f"/api/sessions/{W}/stop", {})
    code, r = owner("POST", f"/api/sessions/{W}/start", {})
    for _ in range(60):
        if os.path.exists(p):
            return json.load(open(p)), (code, r)
        time.sleep(0.5)
    return None, (code, r)


def run_claude(settings, tag):
    """The real CLI under `settings`, asked to write inside and outside."""
    f = os.path.join(TMP, f"settings-{tag}.json")
    json.dump(settings, open(f, "w"))
    inside, outside = os.path.join(OWN, f"{tag}-in.txt"), os.path.join(OUTSIDE, f"{tag}-out.txt")
    prompt = (f"Use the Bash tool to run exactly these two commands, one at a time, and do nothing else: "
              f"`echo ok > {inside}` and then `echo ok > {outside}`. If one fails, do not retry it and do not try another way. "
              "Then reply with one word: DONE.")
    r = subprocess.run([CLAUDE, "-p", "--model", "haiku", "--settings", f, "--allowedTools", "Bash", "--max-turns", "6", prompt],
                       cwd=OWN, capture_output=True, text=True, timeout=240)
    return os.path.exists(inside), os.path.exists(outside), (r.returncode, r.stdout[-300:], r.stderr[-300:])


try:
    for _ in range(180):
        try:
            if urllib.request.urlopen(URL + "/health", context=CTX, timeout=2).status == 200 and os.path.exists(os.path.join(HOME, OWNER_FILE)):
                break
        except OSError:
            pass
        time.sleep(0.5)
    check("the private server is up, in enforce mode", sp.poll() is None)
    check("the real Claude Code is installed", bool(CLAUDE), CLAUDE)
    code, r = owner("POST", "/api/sessions", {"name": W, "dir": OWN, "start": False, "provider": "claude"})
    check("a worker exists, its folder the own folder", code in (200, 201), (code, r))

    broad, info = settings_after_launch()
    check("a launch writes its sandbox settings, enabled", broad and broad["sandbox"]["enabled"] is True, info)
    home_dir = os.path.expanduser("~")
    check("by default the user's home is writable", broad and home_dir in broad["sandbox"]["filesystem"]["allowWrite"], broad)

    code, r = owner("PUT", "/api/scope", {"level": "worker", "name": W, "capability": "env", "value": {"AMUX_CAP_WRITE_FOLDERS": "own"}})
    check("the owner sets AMUX_CAP_WRITE_FOLDERS=own on the worker", code == 200, (code, r))
    code, v = owner("GET", f"/api/workers/{W}/sandbox")
    allow = [e["path"] for e in v.get("entries", []) if e["rule"] == "allowWrite"]
    check("the sandbox view no longer allows the home", home_dir not in allow and any(a.rstrip("/").endswith("own") for a in allow), allow)
    own_only, info = settings_after_launch()
    check("and the relaunch's settings leave the home out", own_only and home_dir not in own_only["sandbox"]["filesystem"]["allowWrite"], info)

    if broad and own_only and CLAUDE:
        i, o, out = run_claude(broad, "broad")
        print("    control (broad):", "inside" if i else "-", "outside" if o else "-", out[1].strip()[-80:])
        check("control: under the default settings Claude writes both files", i and o, out)
        i, o, out = run_claude(own_only, "own")
        print("    own folder only:", "inside" if i else "-", "outside" if o else "-", out[1].strip()[-80:])
        check("under own-folder settings the inside write lands", i, out)
        check("and the outside write is refused by the sandbox", not o, out)
finally:
    owner_stop = None
    try:
        owner("POST", f"/api/sessions/{W}/stop", {})
    except Exception:
        pass
    sp.terminate()
    log.close()
    subprocess.run(["tmux", "-S", os.path.join(HOME, "tmux", f"tmux-{os.getuid()}", "default"), "kill-server"], capture_output=True)
    shutil.rmtree(BASE, ignore_errors=True)
print(f"{'PASS' if fails == 0 else 'FAIL'}: {fails} failing check(s) (logs {TMP})")
sys.exit(1 if fails else 0)
