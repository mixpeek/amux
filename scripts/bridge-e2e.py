#!/usr/bin/env python3
"""The chat bridge end to end: WhatsApp, Slack, Telegram and Discord mirrored
onto amux Messages conversations, a real Claude Code worker picking the
message up, and its answer back in the chat (bridge/mod.rs).

    AMUX_E2E_SERVER_BIN=<amux-server built from this tree> scripts/bridge-e2e.py

Real: a private amux server built from this tree, Messages (conversations:
a worker's thread, a group's channel, @mention routing, delivery), a real
Claude Code worker answering, the turn-end relay posting its answer, the
bridge's outbound loop, the needsyou hold, the webhook signature checks,
Telegram's poll and Discord's channel reads.

Stand-in: the four platforms' servers. One local HTTP server speaks each
platform's wire format (WhatsApp Graph `/{phone}/messages` and signed
webhooks, Slack `chat.postMessage` and signed events, Telegram
`getUpdates`/`sendMessage`, Discord `/channels/{id}/messages`), so amux runs
its shipped adapters against it. A real WhatsApp number needs the owner's
Meta account (docs/bridge.md).
"""
import hashlib, hmac, http.server, json, os, re, secrets, signal, socket, ssl, subprocess, sys, tempfile, threading, time, urllib.parse, urllib.request

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SERVER = os.environ["AMUX_E2E_SERVER_BIN"]
TMP = tempfile.mkdtemp(prefix="amux-bridge-e2e-")
HOME = os.path.join(TMP, "home")
# tmux's socket lives here, not under TMP: macOS caps a socket path at 104
# bytes and $TMPDIR is long enough that the worker failed to start with "File
# name too long" while the run went on to wait four minutes for its answers.
TMUX_DIR = tempfile.mkdtemp(prefix="abe-", dir="/tmp")
TMUX_SOCK = os.path.join(TMUX_DIR, f"tmux-{os.getuid()}", "default")
OWNER_PHONE, STRANGER_PHONE = "15550102030", "15559998888"
APP_SECRET, VERIFY, SLACK_SECRET = "wa-secret", "wa-verify", "slack-secret"
fails = 0
timings = {}


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:700]}"), flush=True)
    fails += 0 if ok else 1


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


# ── the platforms' stand-in ────────────────────────────────────────────────
class Platforms(http.server.BaseHTTPRequestHandler):
    sent = []            # (platform, conversation, text)
    tg_updates = []
    discord_msgs = {}    # channel -> [msg]

    def log_message(self, *a):
        pass

    def reply(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)

    def body(self):
        n = int(self.headers.get("Content-Length", 0))
        return json.loads(self.rfile.read(n) or b"{}")

    def do_GET(self):
        p = self.path
        if p.startswith("/tg/bot") and "/getUpdates" in p:
            off = int(re.search(r"offset=(-?\d+)", p).group(1)) if "offset=" in p else 0
            ups = [u for u in Platforms.tg_updates if u["update_id"] >= off]
            return self.reply(200, {"ok": True, "result": ups})
        m = re.match(r"/discord/channels/(\w+)/messages", p)
        if m:
            after = re.search(r"after=(\d+)", p)
            msgs = [x for x in Platforms.discord_msgs.get(m.group(1), []) if not after or int(x["id"]) > int(after.group(1))]
            return self.reply(200, list(reversed(msgs)))
        self.reply(404, {"error": "no such stand-in route " + p})

    def do_POST(self):
        p, b = self.path, self.body()
        if p.startswith("/wa/") and p.endswith("/messages"):
            assert self.headers.get("Authorization") == "Bearer wa-token", "WhatsApp send without the token"
            Platforms.sent.append(("whatsapp", b["to"], b["text"]["body"], b.get("recipient_type")))
            return self.reply(200, {"messaging_product": "whatsapp", "messages": [{"id": f"wamid.out.{len(Platforms.sent)}"}]})
        if p == "/slack/chat.postMessage":
            Platforms.sent.append(("slack", b["channel"], b["text"], None))
            return self.reply(200, {"ok": True, "ts": f"{time.time():.6f}"})
        if p.startswith("/tg/bot") and p.endswith("/sendMessage"):
            Platforms.sent.append(("telegram", str(b["chat_id"]), b["text"], None))
            return self.reply(200, {"ok": True, "result": {"message_id": len(Platforms.sent)}})
        m = re.match(r"/discord/channels/(\w+)/messages", p)
        if m:
            Platforms.sent.append(("discord", m.group(1), b["content"], None))
            return self.reply(200, {"id": str(10**15 + len(Platforms.sent))})
        self.reply(404, {"error": "no such stand-in route " + p})


plat = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Platforms)
threading.Thread(target=plat.serve_forever, daemon=True).start()
PB = f"http://127.0.0.1:{plat.server_port}"

# ── a private amux ─────────────────────────────────────────────────────────
PORT, TOKEN = free_port(), secrets.token_hex(16)
URL = f"https://localhost:{PORT}"
CTX = ssl.create_default_context(); CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE
os.makedirs(HOME)
with open(os.path.join(HOME, "server.env"), "w") as f:
    f.write("\n".join([
        f"WHATSAPP_TOKEN=wa-token", "WHATSAPP_PHONE_NUMBER_ID=1234", f"WHATSAPP_APP_SECRET={APP_SECRET}", f"WHATSAPP_VERIFY_TOKEN={VERIFY}",
        "SLACK_BOT_TOKEN=xoxb-test", f"SLACK_SIGNING_SECRET={SLACK_SECRET}", "TELEGRAM_BOT_TOKEN=tg-token",
        "DISCORD_BOT_TOKEN=dc-token", "DISCORD_CHANNELS=777",
        f"AMUX_BRIDGE_WHATSAPP_BASE={PB}/wa", f"AMUX_BRIDGE_SLACK_BASE={PB}/slack", f"AMUX_BRIDGE_TELEGRAM_BASE={PB}/tg",
        f"AMUX_BRIDGE_DISCORD_BASE={PB}/discord",
        f"AMUX_OWNER_PHONE=+{OWNER_PHONE}", "AMUX_BRIDGE_OWNERS=slack:UOWNER,telegram:4242,discord:9001", ""]))
env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE", "AMUX_SESSION", "AMUX_WORKER")}
# The worker runs THIS checkout's CLI (`amux post --thread`), not whatever
# is installed, which can be older than the change under test.
BIN = os.path.join(TMP, "bin")
os.makedirs(BIN)
subprocess.run(["cp", os.path.join(REPO, "amux"), os.path.join(BIN, "amux")], check=True)
env["PATH"] = BIN + os.pathsep + env.get("PATH", "")
# The worker is real: tmux is this run's own (TMUX_TMPDIR, $TMUX removed), so
# the server's guard against spawning from a throwaway home is lifted on purpose.
env.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(PORT), AMUX_AUTH_TOKEN=TOKEN, AMUX_NO_SELF_ADOPT="1", AMUX_ALLOW_TMUX_SPAWN_FROM_TEST_HOME="1",
           TMUX_TMPDIR=TMUX_DIR, AMUX_DASHBOARD_DIR=os.path.join(REPO, "crates", "amux-dashboard", "static"))
server = subprocess.Popen([SERVER], env=env, stdout=open(os.path.join(TMP, "server.log"), "w"), stderr=subprocess.STDOUT)


def call(method, path, body=None, headers=None, raw=None):
    h = {"Authorization": "Bearer " + TOKEN, "Content-Type": "application/json", **(headers or {})}
    data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
    req = urllib.request.Request(URL + path, data=data, method=method, headers=h)
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=30) as r:
            t = r.read()
            try:
                return r.status, json.loads(t or b"{}")
            except ValueError:
                return r.status, t.decode()
    except urllib.error.HTTPError as e:
        t = e.read()
        try:
            return e.code, json.loads(t or b"{}")
        except ValueError:
            return e.code, t.decode()


for _ in range(120):
    try:
        if call("GET", "/health")[0] == 200:
            break
    except OSError:
        pass
    time.sleep(0.5)


def wa_hook(frm, text, mid, group=None, name="Ethan", secret=APP_SECRET):
    msg = {"from": frm, "id": mid, "type": "text", "text": {"body": text}}
    if group:
        msg["group_id"] = group
    raw = json.dumps({"object": "whatsapp_business_account", "entry": [{"changes": [{"value": {
        "contacts": [{"wa_id": frm, "profile": {"name": name}}], "messages": [msg]}}]}]}).encode()
    sig = "sha256=" + hmac.new(secret.encode(), raw, hashlib.sha256).hexdigest()
    return call("POST", "/api/bridge/hook/whatsapp", raw=raw, headers={"X-Hub-Signature-256": sig})


def slack_event(event, ts=None):
    raw = json.dumps({"type": "event_callback", "event": event}).encode()
    ts = ts or str(int(time.time()))
    sig = "v0=" + hmac.new(SLACK_SECRET.encode(), b"v0:" + ts.encode() + b":" + raw, hashlib.sha256).hexdigest()
    return call("POST", "/api/bridge/hook/slack", raw=raw, headers={"X-Slack-Request-Timestamp": ts, "X-Slack-Signature": sig})


def sent_to(platform, conv, since):
    return [s for s in Platforms.sent[since:] if s[0] == platform and s[1] == conv]


def answer(token):
    """A worker's answer containing `token`, never the owner's own words sent
    back (an echo carries the prompt, so it also contains the token)."""
    return lambda t: token in t and not t.startswith("owner:") and "exactly" not in t and "codeword did I" not in t


def wait_sent(platform, conv, since, pred=lambda t: True, timeout=240):
    end = time.time() + timeout
    while time.time() < end:
        got = [s for s in sent_to(platform, conv, since) if pred(s[2])]
        if got:
            return got
        time.sleep(1)
    return []


W = "bridgew"
try:
    # ── the worker, in the #chatops channel (a group is a worker tag) ──
    s, v = call("POST", "/api/sessions", {"name": W, "dir": TMP, "model": "sonnet", "desc": "Answers chat messages briefly."})
    check("a worker", s in (200, 201), v)
    with open(os.path.join(HOME, "sessions", f"{W}.env"), "a") as f:
        f.write("\nCC_TAGS=chatops\n")
    s, v = call("POST", f"/api/sessions/{W}/start", {})
    # A 2xx says the start was accepted; the pane existing says it started.
    up = False
    for _ in range(30):
        if subprocess.run(["env", "-u", "TMUX", "tmux", "-S", TMUX_SOCK, "has-session", "-t", f"amux-{W}"],
                          capture_output=True).returncode == 0:
            up = True
            break
        time.sleep(1)
    check("the worker starts (Claude Code)", s in (200, 201, 202) and up, (s, v, "tmux pane up" if up else "no tmux pane"))
    if not up:
        raise SystemExit("the worker did not start; every answer below would only time out")
    time.sleep(20)

    # ── adversarial first: nothing reaches amux unsigned or unlinked ──
    s, _ = wa_hook(OWNER_PHONE, "forged", "wamid.forged", secret="wrong")
    check("a WhatsApp webhook with a bad signature is refused", s == 401, s)
    s, v = call("GET", f"/api/bridge/hook/whatsapp?hub.mode=subscribe&hub.verify_token={VERIFY}&hub.challenge=12345")
    check("the WhatsApp subscription handshake echoes the challenge", s == 200 and str(v) == "12345", v)
    n = len(Platforms.sent)
    wa_hook(STRANGER_PHONE, f"/link {W}", "wamid.s1", name="Stranger")
    time.sleep(3)
    s, st = call("GET", "/api/bridge")
    check("a stranger cannot link a chat", not any(l["conversation"] == STRANGER_PHONE for l in st["links"]), st["links"])
    check("and gets no reply that would advertise the bot", not sent_to("whatsapp", STRANGER_PHONE, n) or "Only the owner" in sent_to("whatsapp", STRANGER_PHONE, n)[0][2])

    # ── WhatsApp direct chat with the owner, linked to the worker ──
    print("whatsapp direct chat")
    n = len(Platforms.sent)
    wa_hook(OWNER_PHONE, f"/link {W}", "wamid.1")
    got = wait_sent("whatsapp", OWNER_PHONE, n, lambda t: "Linked" in t, 20)
    check("the owner links the chat from WhatsApp, and is told", bool(got), Platforms.sent[n:])
    n = len(Platforms.sent)
    t0 = time.time()
    wa_hook(OWNER_PHONE, "Reply with exactly the word PONG-7731 and nothing else.", "wamid.2")
    wa_hook(OWNER_PHONE, "Reply with exactly the word PONG-7731 and nothing else.", "wamid.2")  # the platform retries
    got = wait_sent("whatsapp", OWNER_PHONE, n, answer("PONG-7731"))
    timings["whatsapp_dm_round_trip_s"] = round(time.time() - t0, 1)
    check(f"the worker's answer comes back to WhatsApp ({timings['whatsapp_dm_round_trip_s']}s)", bool(got), Platforms.sent[n:])
    s, posts = call("GET", f"/api/conversations/w:{W}/posts?limit=100")
    rows = posts.get("posts", []) if isinstance(posts, dict) else []
    filed = [p for p in rows if "PONG-7731" in p.get("body", "") and p.get("author_key") == "person:owner"]
    check("the message is in the worker's Messages thread, once (a retried webhook is not posted twice)", len(filed) == 1, rows[-5:])
    answered = [p for p in rows if p.get("author_key") == f"worker:{W}" and "PONG-7731" in p.get("body", "")]
    check("and the worker's answer is a post in that thread", bool(answered), rows[-5:])
    time.sleep(3)
    check("and the answer is sent once", len(wait_sent("whatsapp", OWNER_PHONE, n, answer("PONG-7731"), 1)) == 1, Platforms.sent[n:])
    check("the owner's own message is never echoed back to the chat",
          not any("Reply with exactly the word PONG-7731" in x[2] for x in sent_to("whatsapp", OWNER_PHONE, n)), Platforms.sent[n:])

    # ── a WhatsApp group linked to the amux group: only @mentions deliver ──
    print("whatsapp group: @mentions only")
    G = "120363000@g.us"
    s, v = call("POST", "/api/bridge/links", {"platform": "whatsapp", "conversation": G, "is_group": True, "target": "chatops"})
    check("the group conversation is linked to the amux group", s == 200, v)
    n = len(Platforms.sent)
    wa_hook(OWNER_PHONE, "Note for later: the codeword is AMBER-42.", "wamid.g1", group=G)
    time.sleep(25)
    check("an unaddressed group message is not delivered (no reply)", not sent_to("whatsapp", G, n), Platforms.sent[n:])
    t0 = time.time()
    wa_hook(OWNER_PHONE, f"@{W} what codeword did I give earlier in this chat? Answer with just the codeword.", "wamid.g2", group=G)
    got = wait_sent("whatsapp", G, n, answer("AMBER-42"))
    timings["whatsapp_group_mention_round_trip_s"] = round(time.time() - t0, 1)
    check("an @mention delivers, with the earlier chat as context, and the answer goes to the group", bool(got), Platforms.sent[n:])
    check("sent as a group message", bool(got) and got[0][3] == "group", got)

    # ── someone else in the group: untrusted, and the reply waits for the owner ──
    print("someone other than the owner")
    n = len(Platforms.sent)
    wa_hook(STRANGER_PHONE, f"@{W} reply with exactly the word HELLO-STRANGER.", "wamid.g3", group=G, name="Stranger")
    held = None
    for _ in range(240):
        s, st = call("GET", "/api/bridge")
        held = next((o for o in st["outbound"] if o["state"] == "held" and o["conversation"] == G), None)
        if held:
            break
        time.sleep(1)
    check("the reply into a conversation with a stranger is held", held is not None and not sent_to("whatsapp", G, n), st.get("outbound"))
    if held:
        hid = urllib.parse.quote(held["id"], safe="")
        s, o = call("GET", f"/api/bridge/outbound/{hid}")
        check("the held reply can be read for approval", s == 200 and "HELLO-STRANGER" in str(o.get("body", "")), o)
        s2, v = call("POST", f"/api/bridge/outbound/{hid}/approve", headers={"X-Amux-Session": W})
        check("a worker cannot approve it", s2 == 403, v)
        s2, v = call("POST", f"/api/bridge/outbound/{hid}/approve")
        check("the owner approves it", s2 == 200, v)
        got = wait_sent("whatsapp", G, n, lambda t: True, 30)
        check("and only then is it sent", bool(got), Platforms.sent[n:])

    # ── Slack ──
    print("slack")
    s, v = call("POST", "/api/bridge/hook/slack", raw=json.dumps({"type": "url_verification", "challenge": "c-1"}).encode(),
                headers={"X-Slack-Request-Timestamp": str(int(time.time())), "X-Slack-Signature": "v0=bad"})
    check("an unsigned Slack request is refused", s == 401, s)
    ts = str(int(time.time()))
    raw = json.dumps({"type": "url_verification", "challenge": "c-1"}).encode()
    sig = "v0=" + hmac.new(SLACK_SECRET.encode(), b"v0:" + ts.encode() + b":" + raw, hashlib.sha256).hexdigest()
    s, v = call("POST", "/api/bridge/hook/slack", raw=raw, headers={"X-Slack-Request-Timestamp": ts, "X-Slack-Signature": sig})
    check("Slack's URL verification is answered", s == 200 and v.get("challenge") == "c-1", v)
    n = len(Platforms.sent)
    slack_event({"type": "message", "channel": "DOWNER", "channel_type": "im", "user": "UOWNER", "text": f"/link {W}", "ts": "1.1"})
    wait_sent("slack", "DOWNER", n, lambda t: "Linked" in t, 20)
    t0 = time.time()
    slack_event({"type": "message", "channel": "DOWNER", "channel_type": "im", "user": "UOWNER", "text": "Reply with exactly SLACK-OK-55.", "ts": "1.2"})
    got = wait_sent("slack", "DOWNER", n, answer("SLACK-OK-55"))
    timings["slack_round_trip_s"] = round(time.time() - t0, 1)
    check(f"Slack: linked by the owner, answered ({timings['slack_round_trip_s']}s)", bool(got), Platforms.sent[n:])

    # ── Telegram (its own poll and /link, delivery through the bridge) ──
    print("telegram")
    n = len(Platforms.sent)
    Platforms.tg_updates += [{"update_id": 1, "message": {"message_id": 1, "chat": {"id": 4242, "type": "private"}, "from": {"id": 4242, "username": "ethan"}, "text": f"/link {W}"}}]
    time.sleep(8)
    Platforms.tg_updates += [{"update_id": 2, "message": {"message_id": 2, "chat": {"id": 4242, "type": "private"}, "from": {"id": 4242, "username": "ethan"}, "text": "Reply with exactly TG-OK-88."}}]
    t0 = time.time()
    got = wait_sent("telegram", "4242", n, answer("TG-OK-88"))
    timings["telegram_round_trip_s"] = round(time.time() - t0, 1)
    check(f"Telegram: through the bridge, answered from the transcript ({timings['telegram_round_trip_s']}s)", bool(got), Platforms.sent[n:])

    # ── Discord (channel reads) ──
    print("discord")
    Platforms.discord_msgs["777"] = [{"id": "1000", "author": {"id": "1", "username": "x"}, "content": "history before amux"}]
    time.sleep(10)
    n = len(Platforms.sent)
    Platforms.discord_msgs["777"].append({"id": "1001", "author": {"id": "9001", "username": "ethan"}, "content": "/link chatops"})
    wait_sent("discord", "777", n, lambda t: "Linked" in t, 30)
    t0 = time.time()
    Platforms.discord_msgs["777"].append({"id": "1002", "author": {"id": "9001", "username": "ethan"}, "content": f"@{W} reply with exactly DC-OK-99."})
    got = wait_sent("discord", "777", n, answer("DC-OK-99"))
    timings["discord_round_trip_s"] = round(time.time() - t0, 1)
    check(f"Discord: a channel linked to the group, an @mention answered ({timings['discord_round_trip_s']}s)", bool(got), Platforms.sent[n:])
    s, gp = call("GET", "/api/conversations/g:chatops/posts?limit=100")
    check("Discord history from before the link is not replayed", "history before" not in json.dumps(gp))

    # ── `amux post`: another message from the worker into its thread ──
    n = len(Platforms.sent)
    raw = "A second message: EXTRA-123".encode()
    s, v = call("POST", f"/api/conversations/w:{W}/posts", raw=raw, headers={"X-Amux-Session": W, "Content-Type": "text/plain"})
    got = wait_sent("whatsapp", OWNER_PHONE, n, lambda t: "EXTRA-123" in t, 20)
    check("a worker's own post in its thread goes to every chat linked to it", s in (200, 201) and bool(got), (s, v, Platforms.sent[n:]))
    check("not echoed twice into the same chat", len(got) == 1, got)
finally:
    server.send_signal(signal.SIGTERM)
    try:
        server.wait(20)
    except subprocess.TimeoutExpired:
        server.kill()
    subprocess.run(["env", "-u", "TMUX", "tmux", "-S", TMUX_SOCK, "kill-server"],
                   capture_output=True)
    plat.shutdown()

print("timings: " + json.dumps(timings))
print(("PASS" if not fails else f"FAIL ({fails})") + f"  dir: {TMP}", flush=True)
sys.exit(1 if fails else 0)
