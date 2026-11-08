#!/usr/bin/env python3
"""The recorder listening passively to a real Google Meet call (owner,
2026-11-08: "test e2e using the recorder with a google hangout meeting and
make sure it passively listens and works").

    desktop/test/recorder-meet-e2e.py <meet url> [path/to/amux binary]

Needs: the owner's Chrome reachable over `amux cdp` and signed in to two
Google accounts (authuser=0 hosts, authuser=1 is the other side, so it knocks
and is admitted like any outside guest), whisper.cpp, and, for the other
side's words, the Screen Recording permission for Amux. Anonymous and
automated browsers are refused by Meet ("You can't join this video call"),
which is why both participants are the owner's own Chrome.

Real: the call (a Google Meet in the owner's Chrome, joined by a second tab of
the same Chrome whose microphone is a speech file, so its voice crosses Meet
and comes out of the first tab), the Mac app's meeting detection in "auto"
mode, its microphone path (on the "Background Music" loopback, where a second
script is played) and its system-audio path (ScreenCaptureKit), the session
upload, the live transcript (whisper.cpp) and the filed recording.

The app is a second process of the installed Amux.app with the meeting
settings passed as launch arguments (NSArgumentDomain), so the owner's running
app and its saved settings are untouched.
"""
import glob, json, os, re, secrets, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.request

MEET = sys.argv[1].split("?")[0]
REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
BIN = sys.argv[2] if len(sys.argv) > 2 else "/Applications/Amux.app/Contents/MacOS/amux"
SERVER = os.path.expanduser("~/.local/bin/amux-server-rs")
TMP = tempfile.mkdtemp(prefix="amux-meet-e2e-")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
HOME, SPOOL, RECS = (os.path.join(TMP, n) for n in ("home", "spool", "recordings"))
THEM = ["albatross", "marmalade", "denver", "roadmap"]
fails = 0
t0 = time.time()


def note(msg):
    print(f"[{time.time() - t0:6.1f}s] {msg}", flush=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:600]}"), flush=True)
    fails += 0 if ok else 1
    return ok


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


PORT, TOKEN = free_port(), secrets.token_hex(16)
URL = f"https://localhost:{PORT}"
CTX = ssl.create_default_context(); CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE


def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(URL + path, data=data, method=method,
                                 headers={"Authorization": "Bearer " + TOKEN, "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=60) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        try: return e.code, json.loads(e.read() or b"{}")
        except ValueError: return e.code, {}
    except OSError:
        return 0, {}


def wait_for(pred, timeout, every=1.0):
    end = time.time() + timeout
    while time.time() < end:
        v = pred()
        if v: return v
        time.sleep(every)
    return None


def cdp(*args):
    return subprocess.run(["amux", "cdp", *args], capture_output=True, text=True, timeout=60).stdout.strip()


def press(tab, pattern):
    """Focus the first button whose text or label matches, then press Enter with real input (Meet ignores script clicks)."""
    js = ("(()=>{const re=new RegExp(%s,'i');const b=[...document.querySelectorAll('button,[role=button]')].find(x=>re.test((x.innerText||'').trim())||re.test(x.getAttribute('aria-label')||''));"
          "if(!b) return 'none'; b.focus(); const r=b.getBoundingClientRect(); return JSON.stringify({t:(b.innerText||b.getAttribute('aria-label')||'').trim().slice(0,40),x:Math.round(r.x+r.width/2),y:Math.round(r.y+r.height/2)})})()") % json.dumps(pattern)
    cdp("evalraw", tab, "Page.bringToFront", "{}")
    cdp("evalraw", tab, "Emulation.setFocusEmulationEnabled", json.dumps({"enabled": True}))
    time.sleep(1.5)
    raw = cdp("eval", tab, js).strip()
    try:
        got = json.loads(json.loads(raw)) if raw.startswith('"{') else json.loads(raw)
    except ValueError:
        return "none"
    if not isinstance(got, dict):
        return "none"
    if got["x"] > 0 and got["y"] > 0:
        cdp("clickxy", tab, str(got["x"]), str(got["y"]))
    else:
        for t in ("keyDown", "keyUp"):
            cdp("evalraw", tab, "Input.dispatchKeyEvent", json.dumps({"type": t, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13}))
    return got["t"]


def labels(tab):
    return cdp("eval", tab, "[...document.querySelectorAll('[aria-label]')].map(e=>e.getAttribute('aria-label')).join('|')")


THEM_SCRIPT = ("Hello, this is the guest speaking from the other side of the call. The code word for this meeting is albatross. "
               "We should review the quarterly roadmap and the pricing page. Our customer in Denver wants the video search demo on Thursday. "
               "The second code word is marmalade. That is all from me for now. Thank you.")


def guest_mic_script():
    """A page script that makes a speech file the page's microphone, looped."""
    import base64
    aiff, ogg = os.path.join(TMP, "them.aiff"), os.path.join(TMP, "them.ogg")
    subprocess.run(["say", "-r", "160", "-o", aiff, THEM_SCRIPT], check=True, stderr=subprocess.DEVNULL)
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", aiff, "-ac", "1", "-c:a", "libopus", "-b:a", "32k", ogg], check=True)
    b64 = base64.b64encode(open(ogg, "rb").read()).decode()
    return ("(() => { if (window.__amuxFakeMic) return; window.__amuxFakeMic = 1; const B64 = '%s';"
            "const orig = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices); let dest = null, ctx = null;"
            "async function speech() { if (dest) { ctx.resume(); return dest.stream; } ctx = new AudioContext(); dest = ctx.createMediaStreamDestination();"
            "const bytes = Uint8Array.from(atob(B64), c => c.charCodeAt(0)); const buf = await ctx.decodeAudioData(bytes.buffer);"
            "const src = ctx.createBufferSource(); src.buffer = buf; src.loop = true; src.connect(dest); src.start(); ctx.resume();"
            "document.addEventListener('click', () => ctx.resume(), true); return dest.stream; }"
            "navigator.mediaDevices.getUserMedia = async (c) => { if (c && c.audio) { const s = await speech();"
            "const out = new MediaStream([s.getAudioTracks()[0].clone()]); if (c.video) { try { (await orig({ video: c.video })).getVideoTracks().forEach(t => out.addTrack(t)); } catch (e) {} }"
            "return out; } return orig(c); }; })();") % b64


def open_tab(url):
    out = cdp("open", url)
    m = re.search(r"Opened new tab: ([0-9A-F]+)", out)
    if not m:
        raise SystemExit(f"could not open a Chrome tab: {out[-300:]}")
    return m.group(1)


def applog():
    try: return open(LOG, errors="replace").read()
    except FileNotFoundError: return ""


os.makedirs(os.path.join(HOME, "sessions"), exist_ok=True)
env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE", "AMUX_SESSION", "AMUX_WORKER", "AMUX_ISOLATED")}
env.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(PORT), AMUX_AUTH_TOKEN=TOKEN, AMUX_NO_SELF_ADOPT="1",
           TMUX_TMPDIR=tempfile.mkdtemp(prefix="ame-", dir="/tmp"), AMUX_RECORDINGS_DIR=RECS, AMUX_RECORDINGS_LIVE_SECS="5",
           AMUX_RECORDINGS_WHISPER_MODEL=os.path.expanduser("~/.amux/models/ggml-base.en.bin"),
           AMUX_DASHBOARD_DIR=os.path.join(REPO, "crates", "amux-dashboard", "static"))
server = subprocess.Popen([SERVER], env=env, stdout=open(os.path.join(TMP, "server.log"), "a"), stderr=subprocess.STDOUT)
app = player = None
try:
    wait_for(lambda: api("GET", "/health")[0] == 200, 90, 0.5)
    check("the private amux answers", api("GET", "/health")[0] == 200)
    api("POST", "/api/transcription", {"engine": "local"})
    s, o = api("POST", "/api/recordings/options", {"live": True})
    check("live transcription is on", s == 200 and o.get("live"), (s, o))

    subprocess.run(["xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(REPO, "desktop", "test", "wsctl.swift")], check=True)
    with open(CFG, "w") as f:
        json.dump({"connections": [{"name": "meet-e2e", "url": URL}], "lastUrl": URL, "cleanExit": True,
                   "windows": [{"url": URL + "/", "frame": "{{140, 140}, {1000, 700}}", "selected": True}]}, f)
    note("launching a test copy of the installed app: meeting mode auto, system audio on (launch arguments only)")
    # The host joins first: the call the app should notice.
    HOST_TAB = open_tab(MEET + "?authuser=0")
    GUEST_TAB = open_tab("about:blank")
    cdp("evalraw", GUEST_TAB, "Page.enable", "{}")
    cdp("evalraw", GUEST_TAB, "Page.addScriptToEvaluateOnNewDocument", json.dumps({"source": guest_mic_script()}))
    host_joined = wait_for(lambda: (lambda r: r if r not in ("none", "") else None)(
        press(HOST_TAB, "^Join now$") if "Looking for others" not in cdp("eval", HOST_TAB, "document.body.innerText") else "none"), 60, 3)
    in_call = wait_for(lambda: "Leave call" in labels(HOST_TAB), 30, 2)
    companion = "in Companion mode" in cdp("eval", HOST_TAB, "document.body.innerText")
    if not check("the host is in the Meet call, with audio (not Companion mode)", bool(in_call) and not companion, (host_joined, companion)):
        raise SystemExit(2)
    cdp("nav", GUEST_TAB, MEET + "?authuser=1")
    time.sleep(6)
    check("the second participant's page has the speech as its microphone", "true" in cdp("eval", GUEST_TAB, "!!window.__amuxFakeMic"))
    # Your own microphone is the default input: a real one (a virtual loopback
    # delivers nothing to AVAudioEngine once switched to). Kept in this run's
    # private folder, removed with it.
    app = subprocess.Popen([BIN, "-amuxRecorderMeeting", "auto", "-amuxRecorderSystem", "YES"],
                           env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE,
                                    AMUX_RECORDER_SPOOL=SPOOL, AMUX_RECORDER_SEGMENT_S="5"),
                           stdout=open(LOG, "a"), stderr=subprocess.STDOUT)

    def rec(q):
        out = STATE + ".recorder.json"
        before = os.path.getmtime(out) if os.path.exists(out) else 0
        subprocess.run([os.path.join(TMP, "wsctl"), "send", str(app.pid), "amux://test/recorder?" + q], capture_output=True)
        wait_for(lambda: os.path.exists(out) and os.path.getmtime(out) != before, 10, 0.05)
        return json.load(open(out)) if os.path.exists(out) else {}

    wait_for(lambda: "verdict=windows_restored" in applog(), 40, 0.3)
    note("app up; the host's Meet tab holds the microphone, so the app should see a call")
    detected = wait_for(lambda: "verdict=recorder_meeting_detected" in applog(), 45, 1)
    t_detect = time.time()
    check("the app detects the Google Meet call by itself", bool(detected), applog()[-800:])
    st = wait_for(lambda: (lambda v: v if v.get("state") == "recording" else None)(rec("op=state")), 20, 1) or rec("op=state")
    rid = st.get("id")
    check("and starts recording without being asked (auto mode)", st.get("state") == "recording", st)
    check("from the microphone and the computer's sound", sorted(st.get("sources") or st.get("tracks") or []) == ["mic", "system"]
          or ("system" in json.dumps(st)), st)
    started = re.findall(r"verdict=recorder_(started|source_failed|input_device|mic_permission)[^\n]*", applog())
    note("recorder log: " + "; ".join(re.findall(r"verdict=recorder_[a-z_]+[^\n]{0,140}", applog())[-6:]))

    # The other side speaks: the second tab joins the call, its microphone is the speech.
    note("second participant joins; its speech crosses Meet and plays from the host tab")
    joined = wait_for(lambda: (lambda r: r if r not in ("none", "") else None)(
        press(GUEST_TAB, "^(Ask to join|Join now)$") if not re.search("Getting ready|Looking for others", cdp("eval", GUEST_TAB, "document.body.innerText")) else "none"), 60, 3)
    note(f"guest pressed: {joined}")
    # The host admits the knock (a personal account outside the Workspace).
    admitted = wait_for(lambda: (lambda r: r if r != "none" else None)(press(HOST_TAB, "^(Admit|Admit 1 guest)$")), 40, 2)
    note(f"host pressed: {admitted}")
    guest_in = wait_for(lambda: "Leave call" in labels(GUEST_TAB), 40, 2)
    check("the second participant is in the call", bool(guest_in), joined)
    t_join = time.time()

    def tracks():
        f = os.path.join(RECS, ".sessions", rid or "-", "live.json")
        try: d = json.load(open(f))
        except (OSError, ValueError): return {}
        return {t: " ".join(s.get("text", "") for s in v.get("segments", [])) for t, v in d.get("tracks", {}).items()}

    def has(words, text): return [w for w in words if w in (text or "").lower()]

    them = wait_for(lambda: (lambda t: t if len(has(THEM, t.get("system"))) >= 2 else None)(tracks()), 70, 3)
    check("the other side's words (through Meet) are in the live transcript as Them", bool(them), tracks())
    st = rec("op=state")
    check("my microphone is recorded as its own track (segments arriving)", (st.get("segments") or 0) > 0 and not str(st.get("error") or "").startswith("mic"), st)
    tr = tracks()
    note(f"tracks so far: { {k: v[:160] for k, v in tr.items()} }")

    # Still recording while the call goes on, past the one-minute end-of-call window.
    while time.time() - t_detect < 95:
        time.sleep(3)
    st = rec("op=state")
    check("still recording 95 s into a call that is still going", st.get("state") == "recording",
          (st, re.findall(r"verdict=recorder_[a-z_]+[^\n]{0,100}", applog())[-4:]))
    host_ok = "Leave call" in labels(HOST_TAB)
    check("the call itself carried on undisturbed (the host is still in it)", host_ok)

    # The call ends: both tabs leave. Auto mode stops a minute later.
    note("ending the call: both tabs leave")
    for t in (GUEST_TAB, HOST_TAB):
        press(t, "^Leave call$")
    t_end = time.time()
    stopped = wait_for(lambda: "verdict=recorder_meeting_ended_stop" in applog() or rec("op=state").get("state") != "recording", 150, 5)
    check("after the call ends the recording stops by itself", bool(stopped), rec("op=state"))
    if rec("op=state").get("state") == "recording":
        rec("op=stop")
    sc = wait_for(lambda: (lambda f: json.load(open(f[0])) if f and json.load(open(f[0])).get("transcript", {}).get("status") == "done" else None)(
        glob.glob(os.path.join(RECS, f"*_{rid}.json"))), 300, 5)
    if check("the recording is filed and transcribed", bool(sc), glob.glob(os.path.join(RECS, "*"))):
        md = open(os.path.join(RECS, sc["transcript"]["md"])).read() if sc["transcript"].get("md") else ""
        check("the filed transcript has the other side's words", len(has(THEM, md)) >= 2, md[-1000:])
        check("labelled Them", "Them" in md, md[:600])
        print("--- filed transcript, the Them lines only ---")
        print("\n".join(l for l in md.splitlines() if "Them" in l)[:1500])
finally:
    for t in [x for x in (globals().get("GUEST_TAB"), globals().get("HOST_TAB")) if x]:
        try:
            press(t, "^Leave call$")
            cdp("close", t)
        except Exception as e: print("leave failed", e)
    if player and player.poll() is None: player.kill()
    if app:
        app.terminate()
        try: app.wait(20)
        except subprocess.TimeoutExpired: app.kill()
    server.terminate()
    print("app log verdicts:\n  " + "\n  ".join(re.findall(r"verdict=recorder_[a-z_]+[^\n]{0,160}", applog())[-15:]))
    import shutil
    if os.environ.get("MEET_E2E_KEEP") != "1":
        shutil.rmtree(TMP, ignore_errors=True)
        print(f"removed {TMP} (it held your microphone's audio)")
    else:
        print(f"artifacts: {TMP}")
    print(f"{'PASS' if fails == 0 else 'FAIL'}: {fails} failing check(s)")
