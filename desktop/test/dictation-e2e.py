#!/usr/bin/env python3
"""Dictation in the Mac app, end to end (owner, 2026-11-16: "Improve the
dictation flow test it e2e and clean it up and keep iterating until its as
clean as wisperflow").

    desktop/test/dictation-e2e.py [path/to/Amux.app]

A quiet copy of the app on a private server serving this checkout's
dashboard. The microphone is a spoken file (AMUX_DESKTOP_DICTATION_FILE,
made with `say`), so nothing records the room. 🌐 is pressed and let go
through amux://test/globe, the same handler the keyboard reaches.

Checks: holding 🌐 in a text box shows the listening pill (no buttons, at the
bottom of the window); letting go puts the words at the caret, the pill goes,
and the time from letting go to text is measured against Wispr Flow's median
(0.6 s); the server kept the take (text, what was heard, audio); a quick 🌐
tap dictates nothing; Esc throws a take away; 🌐🌐 dictates hands-free until ✓.
Needs macOS 26 (on-device speech).
"""
import json, os, re, signal, socket, ssl, subprocess, sys, tempfile, time, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-dictation-e2e-")
CFG, LOG, ST = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
SPEECH = os.path.join(TMP, "take.aiff")
SAID = "Ship the pricing page today and tell the research worker when it is live."
WISPR_MEDIAN_MS = 600
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


def log():
    return open(LOG, errors="replace").read() if os.path.exists(LOG) else ""


# `say` run from an Intel python under Rosetta writes an empty file; arm64 first.
if sh("arch", "-arm64", "say", "-o", SPEECH, SAID).returncode != 0:
    sh("say", "-o", SPEECH, SAID)
assert os.path.getsize(SPEECH) > 50000, "say wrote no speech"
WSCTL = os.path.join(TMP, "wsctl")
assert sh("xcrun", "swiftc", "-O", "-o", WSCTL, os.path.join(HERE, "wsctl.swift")).returncode == 0
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
SERVER = os.environ.get("AMUX_E2E_SERVER_BIN") or os.path.expanduser("~/.local/bin/amux-server-rs")
_s = socket.socket(); _s.bind(("127.0.0.1", 0)); PORT = _s.getsockname()[1]; _s.close()
url = f"https://localhost:{PORT}"
SHOME = os.path.join(TMP, "home"); os.makedirs(os.path.join(SHOME, "tmux"))
_env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}
_env.update(AMUX_HOME=SHOME, AMUX_RS_PORT=str(PORT), AMUX_ISOLATED="1", AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(SHOME, "tmux"),
            AMUX_DASHBOARD_DIR=os.path.join(REPO, "crates", "amux-dashboard", "static"))
SLOG = os.path.join(TMP, "server.log")
server = subprocess.Popen([SERVER], env=_env, stdout=open(SLOG, "w"), stderr=subprocess.STDOUT)
_ctx = ssl.create_default_context(); _ctx.check_hostname, _ctx.verify_mode = False, ssl.CERT_NONE


def api(path):
    return json.load(urllib.request.urlopen(url + path, context=_ctx, timeout=10))


for _ in range(120):
    try:
        if urllib.request.urlopen(url + "/health", context=_ctx, timeout=2).status == 200:
            break
    except OSError:
        pass
    time.sleep(0.5)

json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
           "windows": [{"url": url + "/#view=board", "frame": "{{80, 80}, {1200, 800}}"}]}, open(CFG, "w"))
p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST,
                                     AMUX_DESKTOP_DICTATION_FILE=SPEECH),
                     stdout=open(LOG, "a"), stderr=subprocess.STDOUT)


def hook(h, wait=0.6):
    sh(WSCTL, "send", str(p.pid), "amux://test/" + h)
    time.sleep(wait)


def js(code, timeout=15):
    out = ST + ".js"
    if os.path.exists(out):
        os.remove(out)
    hook("js-pane?i=0&code=" + urllib.parse.quote(code), 0)
    end = time.time() + timeout
    while time.time() < end and not os.path.exists(out):
        time.sleep(0.05)
    return open(out).read() if os.path.exists(out) else "no answer"


def pill():
    return json.loads(js("const p = document.getElementById('dict-popup'); const r = p.getBoundingClientRect();"
                         " return JSON.stringify({open: p.classList.contains('open'), ptt: p.classList.contains('dict-ptt'),"
                         " floating: p.classList.contains('dict-floating'), busy: p.classList.contains('dict-busy'),"
                         " status: document.getElementById('dict-popup-status').textContent,"
                         " buttons: getComputedStyle(p.querySelector('.dict-popup-btns')).display !== 'none',"
                         " bottom: Math.round(innerHeight - r.bottom), centre: Math.round(r.left + r.width / 2 - innerWidth / 2), height: Math.round(r.height)})"))


def box(text="Notes: ", caret=None):
    caret = len(text) if caret is None else caret
    return js("let t = document.getElementById('dict-e2e-box'); if (!t) { t = document.createElement('textarea'); t.id = 'dict-e2e-box';"
              " t.style.cssText = 'position:fixed;top:120px;left:120px;width:500px;height:120px;z-index:99999'; document.body.appendChild(t); }"
              f" t.value = {json.dumps(text)}; t.focus(); t.setSelectionRange({caret}, {caret}); return document.activeElement.id")


def value():
    return js("return document.getElementById('dict-e2e-box').value")


def words(s):
    return re.findall(r"[a-z]+", s.lower())


try:
    end = time.time() + 60
    while time.time() < end and js("return String(typeof window._dictNativeEvent)") != "function":
        time.sleep(1)
    js("_hkLoad && _hkLoad(); return 'ok'")
    time.sleep(1)

    # ── Hold 🌐 in a text box, let go: the words go in at the caret. ──
    box("Notes:  end", caret=7)
    hook("globe?down=1", 0.9)
    st = pill()
    check("holding 🌐 shows the listening pill, without buttons, centred at the bottom of the window",
          st["open"] and st["ptt"] and st["floating"] and not st["buttons"] and abs(st["centre"]) <= 2 and 0 < st["bottom"] <= 40 and st["height"] <= 40, st)
    check("and it shows no words while it listens", st["status"] == "", st)
    time.sleep(3)   # the file is ~5 s of speech, fed as fast as it reads
    t0 = time.time()
    hook("globe?down=0", 0)
    v = ""
    while time.time() - t0 < 10:
        v = value()
        if v != "Notes:  end":
            break
        time.sleep(0.02)
    stop_to_text = int((time.time() - t0) * 1000)
    heard = words(v)
    want = words(SAID)
    got = sum(1 for w in want if w in heard)
    check("letting go puts what was said into the box", got >= len(want) - 2, f"value={v!r}")
    check("at the caret, with the text around it kept", v.startswith("Notes: ") and v.endswith(" end") and "  " not in v.strip(), repr(v))
    check(f"letting go to text in under 1.5 s (took {stop_to_text} ms; Wispr Flow's median is {WISPR_MEDIAN_MS} ms)", stop_to_text < 1500, stop_to_text)
    time.sleep(0.5)
    check("the pill goes once the text is in", not pill()["open"], pill())
    m = re.search(r"verdict=dictation_native_done words=(\d+) ms=(\d+) finish_ms=(\d+) audio_bytes=(\d+)", log())
    check("the app logs the take: words, length, stop to text, audio", bool(m) and int(m.group(4)) > 1000, log()[-400:])
    if m:
        print(f"       app: recognizer finished {m.group(3)} ms after the stop; page round trip included: {stop_to_text} ms")
    h = api("/api/dictation/history")["items"]
    check("the server kept it: text, what was heard, and the audio", bool(h) and h[0]["raw_text"] and words(h[0]["text"]) == heard[1:-1]
          and os.listdir(os.path.join(SHOME, "dictation-audio")) if os.path.isdir(os.path.join(SHOME, "dictation-audio")) else False, h[:1])
    sl = open(SLOG, errors="replace").read()
    check("and the page beacons it with the measured wait", "dict_native_done" in sl and "stop_to_text_ms" in sl, sl[-300:])

    # ── A quick 🌐 tap dictates nothing. ──
    box("tap")
    hook("globe?tap=1", 1.0)
    check("a quick 🌐 tap does not start dictation", not pill()["open"] and value() == "tap", pill())

    # ── Esc throws a take away. ──
    n = log().count("verdict=dictation_native_started")
    box("esc")
    hook("globe?down=1", 1.2)
    js("document.getElementById('dict-e2e-box').dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true})); return 'ok'")
    hook("globe?down=0", 2.0)
    check("Esc during a take throws it away: nothing typed, pill gone",
          log().count("verdict=dictation_native_started") > n and "verdict=dictation_native_cancelled" in log() and value() == "esc" and not pill()["open"],
          (value(), pill(), log()[-300:]))

    # ── 🌐🌐 dictates hands-free, with ✕ and ✓, until ✓. ──
    box("Free: ")
    hook("globe?tap=2", 1.0)
    st = pill()
    check("🌐🌐 dictates hands-free: the pill has ✕ and ✓", st["open"] and not st["ptt"] and st["buttons"], st)
    time.sleep(4)
    js("document.querySelector('#dict-popup .dict-popup-btns .primary').click(); return 'ok'", 5)
    end = time.time() + 10
    while time.time() < end and value() == "Free: ":
        time.sleep(0.05)
    v = value()
    check("✓ puts the words in", v.startswith("Free: ") and sum(1 for w in want if w in words(v)) >= len(want) - 2, repr(v))
finally:
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(10)
    except subprocess.TimeoutExpired:
        p.kill()
    server.terminate()
print("PASS: dictation, all checks passed" if fails == 0 else f"dictation: {fails} check(s) FAILED (log {LOG}, server {SLOG})")
sys.exit(1 if fails else 0)
