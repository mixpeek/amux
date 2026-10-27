#!/usr/bin/env python3
"""The native recorder (docs/design/recorder.md, MSG-98487), end to end on the
real built app and a private amux server.

    desktop/test/recorder-e2e.py [path/to/Amux.app]

Real: the app's recorder (writers, 2 s segments here, the manifest, gaps,
the menu bar dot, quit handling, crash recovery, the uploader), the server's
session API and assembly with ffmpeg, the filed recording and its sidecar.

Simulated: the audio itself. Two synthetic tone sources stand in for the
microphone and system audio (`sources=tone,tone2`), because macOS asks a
person to grant those permissions and this agent cannot click. A sleep is the
same handler a real sleep runs, around a source that goes quiet.

Measured: the app's CPU and memory idle and while recording two tracks.
"""
import glob, json, os, secrets, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
SERVER = os.environ.get("AMUX_E2E_SERVER_BIN") or os.path.expanduser("~/.local/bin/amux-server-rs")
TMP = tempfile.mkdtemp(prefix="amux-recorder-e2e-")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
HOME, SPOOL, RECS = (os.path.join(TMP, n) for n in ("home", "spool", "recordings"))
fails = 0
bench = {}


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:600]}"))
    fails += 0 if ok else 1


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


PORT, TOKEN = free_port(), secrets.token_hex(16)
URL = f"https://localhost:{PORT}"
CTX = ssl.create_default_context()
CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE


def api(path):
    req = urllib.request.Request(URL + path, headers={"Authorization": "Bearer " + TOKEN})
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=10) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, {}
    except OSError:
        return 0, {}


def start_server():
    os.makedirs(os.path.join(HOME, "tmux"), exist_ok=True)
    env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}
    env.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(PORT), AMUX_AUTH_TOKEN=TOKEN, AMUX_ISOLATED="1", AMUX_NO_SELF_ADOPT="1",
               TMUX_TMPDIR=os.path.join(HOME, "tmux"), AMUX_RECORDINGS_DIR=RECS,
               AMUX_DASHBOARD_DIR=os.path.join(REPO, "crates", "amux-dashboard", "static"))
    p = subprocess.Popen([SERVER], env=env, stdout=open(os.path.join(TMP, "server.log"), "a"), stderr=subprocess.STDOUT)
    end = time.time() + 60
    while time.time() < end and api("/health")[0] != 200:
        time.sleep(0.5)
    return p


def stop_server(p):
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(15)
    except subprocess.TimeoutExpired:
        p.kill()


def log():
    try:
        return open(LOG, errors="replace").read()
    except FileNotFoundError:
        return ""


subprocess.run(["xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")], check=True)


class App:
    def __init__(self, fresh=True):
        if fresh:
            with open(CFG, "w") as f:
                json.dump({"connections": [{"name": "rec-e2e", "url": URL}], "lastUrl": URL, "cleanExit": True,
                           "windows": [{"url": URL + "/", "frame": "{{120, 120}, {1100, 760}}", "selected": True}]}, f)
        n = log().count("verdict=windows_restored")
        self.p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE,
                                  AMUX_RECORDER_SPOOL=SPOOL, AMUX_RECORDER_SEGMENT_S="2", AMUX_RECORDER_FAKE_MIC="1"), stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
        end = time.time() + 25
        while log().count("verdict=windows_restored") <= n and time.time() < end:
            time.sleep(0.3)
        time.sleep(3)

    def send(self, u, settle=0.3):
        subprocess.run([os.path.join(TMP, "wsctl"), "send", str(self.p.pid), "amux://test/" + u], capture_output=True)
        time.sleep(settle)

    def rec(self, q):
        out = STATE + ".recorder.json"
        before = os.path.getmtime(out) if os.path.exists(out) else 0
        self.send("recorder?" + q, 0)
        end = time.time() + 10
        while time.time() < end and (not os.path.exists(out) or os.path.getmtime(out) == before):
            time.sleep(0.05)
        return json.load(open(out))

    def js(self, code, timeout=15):
        out = STATE + ".js"
        if os.path.exists(out):
            os.remove(out)
        self.send("js-pane?i=0&code=" + urllib.parse.quote(code), 0)
        end = time.time() + timeout
        while time.time() < end and not os.path.exists(out):
            time.sleep(0.1)
        return open(out).read() if os.path.exists(out) else "no answer"

    def usage(self):
        r = subprocess.run(["ps", "-o", "%cpu=,rss=", "-p", str(self.p.pid)], capture_output=True, text=True).stdout.split()
        return (float(r[0]), int(r[1]) // 1024) if len(r) == 2 else (-1, -1)

    def sample(self, seconds):
        cpu, mb = [], []
        for _ in range(int(seconds)):
            c, m = self.usage()
            cpu.append(c)
            mb.append(m)
            time.sleep(1)
        return round(sum(cpu) / len(cpu), 1), max(mb)

    def stop(self, sig=signal.SIGTERM):
        self.p.send_signal(sig)
        try:
            self.p.wait(20)
        except subprocess.TimeoutExpired:
            self.p.kill()


def sidecar(rid):
    f = glob.glob(os.path.join(RECS, f"*_{rid}.json"))
    return json.load(open(f[0])) if f else None


def wait_filed(rid, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        sc = sidecar(rid)
        if sc:
            return sc
        time.sleep(1)
    return None


def segments(rid):
    return sorted(glob.glob(os.path.join(SPOOL, rid, "*.aac")))


server = start_server()
a = None
try:
    a = App()
    st = a.rec("op=state")
    check("the app reports a recorder, idle", st.get("available") is True and st.get("state") == "idle", st)
    bench["idle_cpu_pct"], bench["idle_rss_mb"] = a.sample(8)

    print("the Record tab drives the app's recorder")
    a.js("switchView('record'); return 1")
    time.sleep(1)
    a.js("document.getElementById('recorder-btn').click(); return 1")
    time.sleep(3)
    page = a.js("return JSON.stringify({native: _nativeRecorderAvailable(), state: _nativeRec && _nativeRec.state,"
                " pressed: document.getElementById('recorder-btn').getAttribute('aria-pressed'),"
                " pause: !document.getElementById('recorder-pause').hidden, status: document.getElementById('recorder-status').textContent})")
    try:
        page = json.loads(page)
    except ValueError:
        page = {"error": page}
    st = a.rec("op=state")
    check("the page's button starts the app's recorder, and the page shows it", page.get("native") is True
          and st.get("state") == "recording" and page.get("pressed") == "true" and page.get("pause") is True
          and "keeps going if this window closes" in page.get("status", ""), {"page": page, "app": st})
    a.js("_recorderSetMeeting('ask'); return 1")
    time.sleep(1)
    check("the call setting reaches the app", a.js("return String(_nativeRec && _nativeRec.meeting)") == "ask")
    a.js("_recorderSetMeeting('off'); return 1")
    if st.get("state") == "recording":
        a.rec("op=stop")
        time.sleep(2)

    print("recording two tracks")
    t0 = time.time()
    st = a.rec("op=start&sources=tone,tone2")
    bench["start_ms"] = round((time.time() - t0) * 1000)
    rid = st.get("id", "")
    check("recording, both tracks", st.get("state") == "recording" and st.get("tracks") == ["mic", "system"], st)
    check("the menu bar shows it", st.get("status_item") is True, st)
    bench["recording_cpu_pct"], bench["recording_rss_mb"] = a.sample(8)
    segs = segments(rid)
    check("finished segments are on disk while recording", len(segs) >= 4, segs)
    s, sess = api(f"/api/recordings/sessions/{rid}")
    check("the server already holds segments of both tracks", s == 200 and all(
        (sess.get("tracks", {}).get(t, {}).get("received", 0) >= 1) for t in ("mic", "system")), sess)

    print("a sleep, then windows closed")
    a.rec("op=sleep&s=3")
    time.sleep(6)
    st = a.rec("op=state")
    m = json.load(open(os.path.join(SPOOL, rid, "manifest.json")))
    sleeps = [g for g in m["gaps"] if g["why"] == "sleep"]
    check("the sleep is recorded as a gap on both tracks", {g["track"] for g in sleeps} == {"mic", "system"}
          and all(g["dur_ms"] >= 2000 for g in sleeps), m["gaps"])
    a.send("js?code=" + urllib.parse.quote("window.close()"), 2)
    n_before = st.get("segments", 0)
    time.sleep(5)
    st = a.rec("op=state")
    check("closing the window does not stop the recording", st.get("state") == "recording" and st.get("segments", 0) > n_before, st)

    a.rec("op=stop")
    sc = wait_filed(rid)
    check("stopped, it is filed by the server", sc is not None, os.listdir(RECS) if os.path.isdir(RECS) else "no folder")
    if sc:
        sess = sc.get("session", {})
        check("filed with both tracks and their own files", sess.get("tracks") == ["mic", "system"]
              and len(sess.get("track_files", {})) == 2, sess)
        check("the sleep gap is in the filed recording", any(g.get("why") == "sleep" for g in sess.get("gaps", [])), sess.get("gaps"))
        check("nothing was lost", sc.get("partial") is False and sess.get("lost_chunks") == 0, sess)
        audio = os.path.join(RECS, sc["file"])
        dur = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", audio],
                             capture_output=True, text=True).stdout.strip()
        rec_s = (sc["ended_at"] - sc["started_at"]) / 1000
        check(f"the file's length matches the recording, gaps filled with silence ({dur}s of {rec_s:.1f}s)",
              dur and abs(float(dur) - rec_s) < 2.5, dur)
    end = time.time() + 15
    while time.time() < end and os.path.exists(os.path.join(SPOOL, rid)):
        time.sleep(0.5)
    check("the spool is cleared once filed", not os.path.exists(os.path.join(SPOOL, rid)), os.listdir(SPOOL))

    print("a crash mid-recording")
    st = a.rec("op=start&sources=tone")
    rid2 = st.get("id", "")
    time.sleep(5)
    open_before = glob.glob(os.path.join(SPOOL, rid2, ".tmp-*.aac"))
    a.stop(signal.SIGKILL)
    check("an open segment was on disk when it died", len(open_before) == 1, open_before)
    a = App(fresh=False)
    check("relaunched, it recovers the recording", f"verdict=recorder_recovered id={rid2}" in log(), log()[-800:])
    sc2 = wait_filed(rid2)
    check("the recovered recording is filed, marked recovered", sc2 is not None and sc2.get("recovered") is True, sc2)
    check("the open segment was rescued, not dropped", "rescued_open=1" in log(), log()[-800:])

    print("offline")
    stop_server(server)
    st = a.rec("op=start&sources=tone")
    rid3 = st.get("id", "")
    time.sleep(5)
    a.rec("op=stop")
    check("offline, the recording waits on disk", len(segments(rid3)) >= 2 and sidecar(rid3) is None, segments(rid3))
    server = start_server()
    a.rec("op=ship")
    sc3 = wait_filed(rid3, 60)
    check("back online, it is filed without anyone asking", sc3 is not None and sc3.get("partial") is False, sc3)

    print("quit while recording")
    a.rec("op=start&sources=tone")
    time.sleep(3)
    a.stop(signal.SIGTERM)
    check("a SIGTERM stops and saves the recording first", "verdict=recorder_stopped_for_quit signal=1" in log(), log()[-600:])
finally:
    if a:
        try:
            a.stop()
        except Exception:
            pass
    stop_server(server)
    # The private server by its socket, never `tmux kill-server` bare: inside a
    # pane $TMUX names the FLEET's server and wins over TMUX_TMPDIR.
    subprocess.run(["env", "-u", "TMUX", "tmux", "-S", os.path.join(HOME, "tmux", f"tmux-{os.getuid()}", "default"),
                    "kill-server"], capture_output=True)

print("benchmark: " + json.dumps(bench))
print(("PASS" if not fails else f"FAIL ({fails})") + f"  log: {LOG}")
sys.exit(1 if fails else 0)
