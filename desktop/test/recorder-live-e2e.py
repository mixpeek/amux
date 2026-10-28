#!/usr/bin/env python3
"""Live transcription, your notes, meeting notes and filing with workers
(owner, 2026-10-28), end to end on real speech.

    desktop/test/recorder-live-e2e.py [path/to/Amux.app]

Real:
- The speech. macOS `say` speaks a meeting script. The Mac leg plays it, in
  real time, into the "Background Music" loopback device: its input is the
  sound played into it, and with the Background Music app closed nothing
  reaches a speaker. The app records that input through its REAL microphone
  path (AVAudioEngine, `AMUX_RECORDER_INPUT_DEVICE`). The browser leg gives
  Chromium the same speech as its microphone (fake capture of a WAV file),
  through the Record tab's MediaRecorder.
- The app, the page, the server's session API, the `recordings-live` job,
  whisper.cpp, assembly, the finished transcript, the notes pass, the group
  channel post. The server is killed with SIGKILL mid-meeting and restarted.
- With AMUX_LIVE_E2E_REAL_HELPER=1, the helper model too.

Stand-in, said here: without that switch the helper model is a script
(`AMUX_HELPER_CLI`) that answers with fixed notes naming the test worker, so
the run does not spend a model call. Everything around it is real: the prompt
it receives (checked for the transcript and the fleet), the JSON parsing, the
filter that drops names not in the fleet, the files and the post.

Needs: whisper-cli and a model (the server's own engine), ffmpeg, the
"Background Music" device, and the app allowed to use the microphone.
"""
import glob, json, os, re, secrets, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
SERVER = os.environ.get("AMUX_E2E_SERVER_BIN") or os.path.expanduser("~/.local/bin/amux-server-rs")
TMP = tempfile.mkdtemp(prefix="amux-live-e2e-")
TMUX_DIR = tempfile.mkdtemp(prefix="ale-", dir="/tmp")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
HOME, SPOOL, RECS = (os.path.join(TMP, n) for n in ("home", "spool", "recordings"))
LOOPBACK = os.environ.get("AMUX_LIVE_E2E_DEVICE", "Background Music")
REAL_HELPER = os.environ.get("AMUX_LIVE_E2E_REAL_HELPER") == "1"
fails = 0
timings = {}

SCRIPT = [
    "This is the weekly recorder planning meeting.",
    "First topic. The live transcript has to appear while we are still talking, so nothing is lost if the laptop dies.",
    "The code word for this test is pelican.",
    "Second topic. Meeting notes need a short summary, the decisions, and the action items.",
    "We decided to ship the record hotkey this week.",
    "Action item: Ethan reviews the recorder on Friday.",
    "Third topic. The meeting should be filed with the amux recorder worker automatically.",
    "Fourth topic. Two hour recordings must never drop a segment, and a crash must keep everything said so far.",
    "Fifth topic. The calendar event names the meeting and lists who was there, so the notes know the participants.",
    "Sixth topic. The phone app records in the background too, and uploads when it is back online.",
    "That is everything for today. Thanks, everyone.",
]
WORDS = ["recorder", "pelican", "hotkey", "friday"]
# `say` from a translated (Rosetta) parent writes a header-only file: this
# Mac's python3 is x86_64. Run it native.
SAY = ["arch", "-arm64", "say"] if os.uname().machine == "x86_64" and subprocess.run(
    ["/usr/sbin/sysctl", "-n", "sysctl.proc_translated"], capture_output=True, text=True).stdout.strip() == "1" else ["say"]


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:700]}"), flush=True)
    fails += 0 if ok else 1
    return ok


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


def api(method, path, body=None, raw=None, ctype="application/json"):
    data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
    req = urllib.request.Request(URL + path, data=data, method=method,
                                 headers={"Authorization": "Bearer " + TOKEN, "Content-Type": ctype})
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=60) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read() or b"{}")
        except ValueError:
            return e.code, {}
    except OSError:
        return 0, {}


# ── a private amux, with its own fleet of two workers ──────────────────────
os.makedirs(os.path.join(HOME, "sessions"), exist_ok=True)
for name, tags, desc in [("recorder-lead", "recorder-team", "Owns the amux recorder: live transcripts, meeting notes and the record hotkey."),
                         ("payroll-bot", "finance", "Runs payroll and reconciles invoices.")]:
    with open(os.path.join(HOME, "sessions", f"{name}.env"), "w") as f:
        f.write(f'CC_TAGS="{tags}"\nCC_DESC="{desc}"\nCC_DIR="{TMP}"\n')
STUB = os.path.join(TMP, "helper-stub.sh")
PROMPT_SEEN = os.path.join(TMP, "prompt.txt")
with open(STUB, "w") as f:
    f.write(f"""#!/bin/sh
# A stand-in for the helper model: keeps the prompt it was given, answers fixed notes.
printf '%s' "$2" > {PROMPT_SEEN}
cat <<'EOF'
Here are the notes.
{{"title": "Weekly recorder planning", "summary": "The team planned live transcripts, meeting notes and the record hotkey.",
 "notes_md": "## Live transcript\\n- Must appear while talking.\\n\\n## Notes\\n- Summary, decisions, action items.",
 "decisions": ["Ship the record hotkey this week"], "action_items": ["Ethan reviews the recorder on Friday"],
 "workers": ["recorder-lead", "a-worker-that-does-not-exist"], "groups": ["recorder-team", "no-such-group"], "why": "It is about the recorder."}}
EOF
""")
os.chmod(STUB, 0o755)


def start_server():
    env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE", "AMUX_SESSION", "AMUX_WORKER", "AMUX_ISOLATED")}
    # Not isolated: the transcription jobs must run. tmux is this run's own.
    env.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(PORT), AMUX_AUTH_TOKEN=TOKEN, AMUX_NO_SELF_ADOPT="1",
               TMUX_TMPDIR=TMUX_DIR, AMUX_RECORDINGS_DIR=RECS, AMUX_RECORDINGS_LIVE_SECS="5",
               # The local engine, so no audio leaves this Mac and no key is spent.
               AMUX_RECORDINGS_WHISPER_MODEL=os.environ.get("AMUX_LIVE_E2E_MODEL", os.path.expanduser("~/.amux/models/ggml-base.en.bin")),
               AMUX_DASHBOARD_DIR=os.path.join(REPO, "crates", "amux-dashboard", "static"))
    if not REAL_HELPER:
        env.update(AMUX_HELPER_CLI=STUB, AMUX_HELPER_MODEL="stub")
        env.pop("ANTHROPIC_API_KEY", None)
    p = subprocess.Popen([SERVER], env=env, stdout=open(os.path.join(TMP, "server.log"), "a"), stderr=subprocess.STDOUT)
    end = time.time() + 90
    while time.time() < end and api("GET", "/health")[0] != 200:
        time.sleep(0.5)
    return p


def stop_server(p, sig=signal.SIGTERM):
    p.send_signal(sig)
    try:
        p.wait(20)
    except subprocess.TimeoutExpired:
        p.kill()


def applog():
    try:
        return open(LOG, errors="replace").read()
    except FileNotFoundError:
        return ""


def sidecar(rid):
    f = glob.glob(os.path.join(RECS, f"*_{rid}.json"))
    return json.load(open(f[0])) if f else None


def wait_for(pred, timeout, every=1.0):
    end = time.time() + timeout
    while time.time() < end:
        v = pred()
        if v:
            return v
        time.sleep(every)
    return None


def words_in(text):
    t = (text or "").lower()
    return [w for w in WORDS if w in t]


# ── the speech ─────────────────────────────────────────────────────────────
script_txt = os.path.join(TMP, "meeting.txt")
open(script_txt, "w").write("\n".join(SCRIPT))
aiff, wav = os.path.join(TMP, "meeting.aiff"), os.path.join(TMP, "meeting.wav")
speech_s = 0.0
for _attempt in range(3):
    # `say` has written a header-only file under load; a short script is a failed one.
    subprocess.run(SAY + ["-r", "165", "-o", aiff, "-f", script_txt], check=True, stderr=subprocess.DEVNULL)
    speech_s = float(subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", aiff],
                                    capture_output=True, text=True).stdout.strip() or 0)
    if speech_s > 20:
        break
    time.sleep(2)
if speech_s <= 20:
    raise SystemExit(f"say produced {speech_s:.1f}s of speech for the meeting script; nothing to test with")
subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", aiff, "-ar", "48000", "-ac", "1", wav], check=True)
print(f"meeting script: {speech_s:.1f}s of speech, words to find: {WORDS}", flush=True)
devs = subprocess.run(SAY + ["-a", "?"], capture_output=True, text=True).stdout
m = re.search(r"^\s*(\d+)\s+" + re.escape(LOOPBACK) + r"\s*$", devs, re.M)
LOOP_ID = m.group(1) if m else None

server = start_server()
app = None
try:
    check("the private amux answers", api("GET", "/health")[0] == 200)
    api("POST", "/api/transcription", {"engine": "local"})
    s, cfg = api("GET", "/api/recordings/config")
    if not check("it transcribes locally with whisper.cpp", cfg.get("transcriber", {}).get("engine") == "whisper.cpp", cfg.get("transcriber")):
        raise SystemExit(2)
    s, o = api("POST", "/api/recordings/options", {"live": True, "notes": True, "organize": True})
    check("live, notes and filing switch on", s == 200 and o.get("live") and o.get("notes") and o.get("organize"), (s, o))
    s, o = api("POST", "/api/recordings/options", {"notes": "yes"})
    check("an option that is not true or false is refused", s == 400, (s, o))

    # ── browser leg: the Record tab, Chromium's microphone is the speech file ──
    print("browser: the Record tab records real speech; the hotkey starts and stops it", flush=True)
    t0 = time.time()
    r = subprocess.run(["node", os.path.join(HERE, "recorder-live-browser.mjs"), URL, wav, str(int(speech_s + 14))],
                       capture_output=True, text=True, cwd=REPO, timeout=600)
    try:
        b = json.loads((r.stdout.strip().splitlines() or ["{}"])[-1])
    except ValueError:
        b = {"error": r.stdout[-400:] + r.stderr[-400:]}
    check("Alt+Shift+R starts and stops a recording in the page", b.get("hotkey") == "started and stopped", b)
    live_text = " ".join(b.get("lines") or [])
    check("the live panel showed the meeting's words while it was recording", len(words_in(live_text)) >= 2, b)
    if b.get("first_line_s") is not None:
        timings["browser_first_live_line_s"] = b["first_line_s"]
    check("and the first words came well before the meeting ended", b.get("first_line_s") is not None and b["first_line_s"] < speech_s, b)
    bid = b.get("id")
    sc = wait_for(lambda: (lambda v: v if v and v.get("transcript", {}).get("status") == "done" else None)(sidecar(bid)) if bid else None, 240)
    check("the browser recording is filed and transcribed", bool(sc), sidecar(bid) if bid else b)
    if sc:
        check("its finished transcript was built from the live pieces", sc["transcript"].get("live") is True, sc["transcript"])
        check("the notes typed during it are on the recording", "pelican check" in (sc.get("user_notes") or ""), sc.get("user_notes"))
        # "@recorder-lead" in the typed notes tags that worker: it gets the recording in its thread.
        tagged = wait_for(lambda: (lambda v: v if v and any(w.get("state") for w in v.get("send_to", [])) else None)(sidecar(bid)), 120)
        st = (tagged or sidecar(bid) or {}).get("send_to", [])
        check("an @worker typed in the notes sends it the recording", any(w.get("worker") == "recorder-lead" and w.get("state", "").startswith("posted") for w in st), st)
        s, th = api("GET", "/api/conversations/w:recorder-lead/posts?limit=20")
        msg = next((p for p in th.get("posts", []) if "A recording for you" in p.get("body", "")), None)
        body = (msg or {}).get("body", "")
        check("the worker's thread has it from the owner, with the audio and transcript paths", bool(msg) and msg.get("author_key") == "person:owner"
              and RECS in body and ".md" in body and "Audio:" in body and "Transcript:" in body, (s, th.get("posts", [])[-2:]))

    # ── Mac leg: the app's real microphone path, on the loopback ──
    print("mac: the app records the loopback through its real microphone path", flush=True)
    if not check(f"the {LOOPBACK} loopback device exists", bool(LOOP_ID), devs[:400]):
        raise SystemExit(2)
    subprocess.run(["xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")], check=True)
    with open(CFG, "w") as f:
        json.dump({"connections": [{"name": "live-e2e", "url": URL}], "lastUrl": URL, "cleanExit": True,
                   "windows": [{"url": URL + "/", "frame": "{{120, 120}, {1100, 760}}", "selected": True}]}, f)
    app = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE,
                           AMUX_RECORDER_SPOOL=SPOOL, AMUX_RECORDER_SEGMENT_S="5", AMUX_RECORDER_INPUT_DEVICE=LOOPBACK),
                           stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
    wait_for(lambda: "verdict=windows_restored" in applog(), 30, 0.3)
    time.sleep(3)

    def send(u):
        subprocess.run([os.path.join(TMP, "wsctl"), "send", str(app.pid), "amux://test/" + u], capture_output=True)

    def rec(q):
        out = STATE + ".recorder.json"
        before = os.path.getmtime(out) if os.path.exists(out) else 0
        send("recorder?" + q)
        wait_for(lambda: os.path.exists(out) and os.path.getmtime(out) != before, 10, 0.05)
        return json.load(open(out)) if os.path.exists(out) else {}

    def js(code):
        out = STATE + ".js"
        if os.path.exists(out):
            os.remove(out)
        send("js-pane?i=0&code=" + urllib.parse.quote(code))
        wait_for(lambda: os.path.exists(out), 15, 0.1)
        return open(out).read() if os.path.exists(out) else "no answer"

    st = rec("op=state")
    check("the app lists this Mac's microphones, the loopback among them", LOOPBACK in (st.get("inputs") or []), st)
    # A calendar event around this meeting: the recording is named and peopled by it.
    now = time.time()
    fmt = lambda t: time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(t))
    s, ev = api("POST", "/api/cal-events", {"title": "Recorder planning sync", "start": fmt(now - 60), "end": fmt(now + 600),
                                           "description": "e2e"})
    check("a calendar event for the meeting", s in (200, 201), (s, ev))
    st = rec("op=start&sources=mic")
    rid = st.get("id")
    started = st.get("state") == "recording"
    check("the app records from the chosen input", started and f"verdict=recorder_input_device device={LOOPBACK}" in applog(), (st, applog()[-600:]))
    if not started:
        raise SystemExit(2)
    js("switchView('record'); return 1")
    time.sleep(1)
    js("const t = document.getElementById('recorder-notes'); t.value = 'Mac leg notes: ask about pelican.'; _recorderNotesInput(t.value); return 1")
    t_start = time.time()
    player = subprocess.Popen(SAY + ["-r", "165", "-a", LOOP_ID, "-f", script_txt], stderr=subprocess.DEVNULL)
    # Live: words on the server while the meeting is still going.
    first = wait_for(lambda: (lambda r: r[1] if r[0] == 200 and r[1].get("segments") else None)(api("GET", f"/api/recordings/sessions/{rid}/live")), speech_s + 40, 2)
    t_first = time.time() - t_start
    timings["mac_first_live_line_s"] = round(t_first, 1)
    check("the first live words reach the server mid-meeting", bool(first) and t_first < speech_s, (t_first, first))
    sdir = os.path.join(RECS, ".sessions", rid)
    check("and they are on disk as they arrive (live.md), so a crash keeps them", bool(first) and os.path.exists(os.path.join(sdir, "live.md")),
          os.listdir(sdir) if os.path.isdir(sdir) else "no session folder")
    # A crash mid-meeting: SIGKILL the server. What was live is on disk; it picks up after a restart.
    before = json.load(open(os.path.join(sdir, "live.json"))) if os.path.exists(os.path.join(sdir, "live.json")) else {}
    n_before = sum(len(t.get("segments", [])) for t in before.get("tracks", {}).values())
    stop_server(server, signal.SIGKILL)
    check("after a SIGKILL of the server, the live transcript so far is still on disk", n_before > 0, before)
    time.sleep(4)
    server = start_server()
    check("the server comes back", api("GET", "/health")[0] == 200)
    page_saw = wait_for(lambda: (lambda t: t if words_in(t) else None)(js("return document.getElementById('recorder-live-text').textContent")), 40, 2)
    check("the app's Record tab shows the live transcript", bool(page_saw), page_saw)
    player.wait(timeout=speech_s + 60)
    time.sleep(8)
    live = api("GET", f"/api/recordings/sessions/{rid}/live")[1]
    live_text = " ".join(s["text"] for s in live.get("segments", []))
    check("after the restart the live transcript kept going (later words are in it)", len(words_in(live_text)) >= 3, live_text)
    t_meeting_end = time.time() - t_start
    st = rec("op=stop")
    sc = wait_for(lambda: (lambda v: v if v and v.get("transcript", {}).get("status") == "done" else None)(sidecar(rid)), 240)
    if check("the Mac recording is filed and transcribed", bool(sc), sidecar(rid)):
        timings["mac_meeting_s"] = round(t_meeting_end, 1)
        t = sc["transcript"]
        md = open(os.path.join(RECS, t["md"])).read() if t.get("md") else ""
        check("from the live pieces plus the tail", t.get("live") is True, t)
        check("and it has the meeting's words", len(words_in(md)) >= 3, md[-800:])
        check("the notes typed during the meeting are on the recording", "ask about pelican" in (sc.get("user_notes") or ""), sc.get("user_notes"))
        s, ok = api("POST", f"/api/recordings/{rid}/send", {"workers": [{"worker": "recorder-lead", "instructions": "Catalogue it under planning."}]})
        check("it can be sent to a worker from the Record tab, with instructions", s == 200, (s, ok))
        s, bad = api("POST", f"/api/recordings/{rid}/send", {"workers": ["no-such-worker"]})
        check("a worker that does not exist is refused", s == 400, (s, bad))
        n = wait_for(lambda: (lambda v: v if v and v.get("notes", {}).get("status") in ("done", "failed") else None)(sidecar(rid)), 180)
        n = n or sidecar(rid)
        notes = n.get("notes", {})
        check("the calendar event named the meeting", (n.get("meeting") or {}).get("title") == "Recorder planning sync", n.get("meeting"))
        if check("meeting notes are written", notes.get("status") == "done", notes):
            text = open(os.path.join(RECS, notes["md"])).read()
            check("with a title, decisions, action items and your notes", "# " in text and "## Decisions" in text
                  and "## Action items" in text and "ask about pelican" in text, text[:900])
            if not REAL_HELPER:
                prompt = open(PROMPT_SEEN).read() if os.path.exists(PROMPT_SEEN) else ""
                check("the model was given the transcript and the fleet", len(words_in(prompt)) >= 3
                      and "recorder-lead | recorder-team" in prompt and "payroll-bot" in prompt, prompt[:600])
            about = n.get("about") or {}
            if REAL_HELPER:
                check("the model filed it with the recorder worker, not payroll", "recorder-lead" in about.get("workers", [])
                      and "payroll-bot" not in about.get("workers", []), about)
            else:
                check("it is filed with the real worker and group only (a made-up name is dropped)",
                      about.get("workers") == ["recorder-lead"] and about.get("groups") == ["recorder-team"], about)
            s, posts = api("GET", "/api/conversations/g:recorder-team/posts?limit=20")
            rp = [p for p in posts.get("posts", []) if p.get("author_key") == "recorder:amux"]
            check("a summary is posted in the group's channel, by the Recorder", s == 200 and len(rp) >= 1, (s, posts))
            check("and it wakes nobody (no mention, no delivery)", all(not p.get("mentions") and not p.get("deliveries") for p in rp), rp)
            sent = wait_for(lambda: (lambda v: v if v and all(w.get("state") for w in v.get("send_to", [])) else None)(sidecar(rid)), 60)
            st2 = (sent or sidecar(rid) or {}).get("send_to", [])
            check("the tagged worker got it after the notes, with the instructions", any(w.get("worker") == "recorder-lead" and str(w.get("state", "")).startswith("posted") for w in st2), st2)
            s, th = api("GET", "/api/conversations/w:recorder-lead/posts?limit=40")
            msg = next((p for p in reversed(th.get("posts", [])) if "Catalogue it under planning" in p.get("body", "")), None)
            check("the message carries the notes, the calendar event and the instructions", bool(msg) and "Meeting notes:" in msg["body"]
                  and "Recorder planning sync" in msg["body"], (msg or {}).get("body", "")[:600])
            s, one = api("GET", f"/api/recordings/{rid}")
            check("the recording's notes can be read back", s == 200 and "Decisions" in (one.get("notes", {}).get("text") or ""), one.get("notes"))
    print("timings: " + json.dumps(timings), flush=True)
finally:
    if app:
        app.send_signal(signal.SIGTERM)
        try:
            app.wait(20)
        except subprocess.TimeoutExpired:
            app.kill()
    stop_server(server)
    subprocess.run(["env", "-u", "TMUX", "tmux", "-S", os.path.join(TMUX_DIR, f"tmux-{os.getuid()}", "default"), "kill-server"],
                   capture_output=True)

print(("PASS" if fails == 0 else f"FAIL ({fails})") + f"  dir: {TMP}")
sys.exit(1 if fails else 0)
