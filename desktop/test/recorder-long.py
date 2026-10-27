#!/usr/bin/env python3
"""A long recording, in real time, on the real app and a private server
(docs/design/recorder.md acceptance: 2+ hours without dropped audio).

    AMUX_E2E_SERVER_BIN=<server> desktop/test/recorder-long.py [seconds, default 7500]

Two synthetic tracks (the permission-free tone sources), default 10 s
segments. Every minute it samples the app's CPU and memory and the gaps so
far. At the end it stops, waits for the filed recording, and checks: no gap,
no lost segment, the file's length within 2 s of wall time, every segment
uploaded. The samples go to <tmp>/samples.json for the benchmark table.
"""
import glob, json, os, secrets, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
BIN = os.environ.get("AMUX_LONG_APP_BIN") or os.path.join(REPO, "Amux.app", "Contents", "MacOS", "amux")
SERVER = os.environ["AMUX_E2E_SERVER_BIN"]
SECONDS = int(sys.argv[1]) if len(sys.argv) > 1 else 7500
TMP = tempfile.mkdtemp(prefix="amux-recorder-long-")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
HOME, SPOOL, RECS = (os.path.join(TMP, n) for n in ("home", "spool", "recordings"))
s = socket.socket(); s.bind(("127.0.0.1", 0)); PORT = s.getsockname()[1]; s.close()
TOKEN, URL = secrets.token_hex(16), f"https://localhost:{PORT}"
CTX = ssl.create_default_context(); CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE
fails = 0


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:600]}"), flush=True)
    fails += 0 if ok else 1


os.makedirs(os.path.join(HOME, "tmux"))
env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}
env.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(PORT), AMUX_AUTH_TOKEN=TOKEN, AMUX_ISOLATED="1", AMUX_NO_SELF_ADOPT="1",
           TMUX_TMPDIR=os.path.join(HOME, "tmux"), AMUX_RECORDINGS_DIR=RECS,
           AMUX_DASHBOARD_DIR=os.path.join(REPO, "crates", "amux-dashboard", "static"))
server = subprocess.Popen([SERVER], env=env, stdout=open(os.path.join(TMP, "server.log"), "w"), stderr=subprocess.STDOUT)
for _ in range(120):
    try:
        urllib.request.urlopen(URL + "/health", context=CTX, timeout=3); break
    except OSError:
        time.sleep(0.5)
subprocess.run(["xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")], check=True)
json.dump({"connections": [{"name": "long", "url": URL}], "lastUrl": URL, "cleanExit": True,
           "windows": [{"url": URL + "/", "frame": "{{120, 120}, {1000, 700}}", "selected": True}]}, open(CFG, "w"))
app = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE,
                       AMUX_RECORDER_SPOOL=SPOOL), stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
time.sleep(12)


def rec(q):
    out = STATE + ".recorder.json"
    before = os.path.getmtime(out) if os.path.exists(out) else 0
    subprocess.run([os.path.join(TMP, "wsctl"), "send", str(app.pid), "amux://test/recorder?" + q], capture_output=True)
    for _ in range(200):
        if os.path.exists(out) and os.path.getmtime(out) != before:
            break
        time.sleep(0.05)
    return json.load(open(out))


samples = []
try:
    st = rec("op=start&sources=tone,tone2")
    rid = st["id"]
    t0 = time.time()
    print(f"recording {rid} for {SECONDS}s; samples every 60 s", flush=True)
    while time.time() - t0 < SECONDS:
        time.sleep(60)
        r = subprocess.run(["ps", "-o", "%cpu=,rss=", "-p", str(app.pid)], capture_output=True, text=True).stdout.split()
        st = rec("op=state")
        samples.append({"t_s": round(time.time() - t0), "cpu_pct": float(r[0]), "rss_mb": int(r[1]) // 1024,
                        "segments": st.get("segments"), "pending_upload": st.get("pending_upload"), "gaps": st.get("gaps")})
        json.dump(samples, open(os.path.join(TMP, "samples.json"), "w"))
        if len(samples) % 10 == 0:
            print(json.dumps(samples[-1]), flush=True)
    wall = time.time() - t0
    rec("op=stop")
    sc = None
    for _ in range(600):
        f = glob.glob(os.path.join(RECS, f"*_{rid}.json"))
        if f:
            sc = json.load(open(f[0])); break
        time.sleep(1)
    check("the long recording is filed", sc is not None)
    if sc:
        sess = sc["session"]
        check("no gap in the whole recording", sess["gaps"] == [], sess["gaps"])
        check("no lost segment", sess["lost_chunks"] == 0 and sc["partial"] is False, sess)
        dur = float(subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0",
                                    os.path.join(RECS, sc["file"])], capture_output=True, text=True).stdout.strip() or 0)
        check(f"the file is {dur:.1f}s for {wall:.1f}s of recording", abs(dur - wall) < 2.0, dur)
    cpu = [x["cpu_pct"] for x in samples]
    rss = [x["rss_mb"] for x in samples]
    print("benchmark: " + json.dumps({"minutes": len(samples), "cpu_pct_mean": round(sum(cpu) / len(cpu), 2), "cpu_pct_max": max(cpu),
                                      "rss_mb_first": rss[0], "rss_mb_last": rss[-1], "rss_mb_max": max(rss),
                                      "max_pending_upload": max(x["pending_upload"] or 0 for x in samples)}), flush=True)
finally:
    app.send_signal(signal.SIGTERM)
    try:
        app.wait(30)
    except subprocess.TimeoutExpired:
        app.kill()
    server.send_signal(signal.SIGTERM)
    try:
        server.wait(15)
    except subprocess.TimeoutExpired:
        server.kill()
    subprocess.run(["env", "-u", "TMUX", "tmux", "-S", os.path.join(HOME, "tmux", f"tmux-{os.getuid()}", "default"), "kill-server"],
                   capture_output=True)
print(("PASS" if not fails else f"FAIL ({fails})") + f"  dir: {TMP}", flush=True)
sys.exit(1 if fails else 0)
