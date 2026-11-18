#!/usr/bin/env python3
"""The call bridge: amux asks for a call, Twilio places it, and xAI's realtime
voice model holds the conversation (docs/design/phone-calls.md).

    amux ── POST /<secret>/calls ──▶ bridge ── Twilio REST ──▶ phone
                                       ▲  │ call audio (Twilio Media Streams, μ-law 8 kHz)
                                       │  ▼
                                    xAI realtime voice (audio/pcmu, no transcoding)
    amux ◀── events, result (transcript, outcome) ── bridge

It runs where Twilio can reach a WebSocket: the amux tunnel is HTTP long-poll,
so it cannot run on the Mac. Its contract with amux is the three HTTP calls in
the design doc; amux sends no credential with the first, so the bridge's own
address carries a secret path segment (AMUX_CALL_BRIDGE_URL =
https://<host>/<BRIDGE_SECRET>). Twilio's requests carry a per-call key instead.

Environment:
  XAI_API_KEY, TWILIO_ACCOUNT_SID, TWILIO_AUTH_TOKEN
  BRIDGE_PUBLIC_URL   https://<host> as Twilio reaches this bridge
  BRIDGE_SECRET       the path segment amux's requests carry
  BRIDGE_PORT         default 8790
  BRIDGE_FAKE_DIAL=1  tests: no Twilio REST; the test plays the phone over /media
  XAI_REALTIME_MODEL  default grok-voice-latest
  XAI_SUMMARY_MODEL   default grok-4.3

Every line it logs about a call starts with `verdict=` so a sweep finds it.
"""
import asyncio
import base64
import json
import logging
import os
import secrets
import time

import aiohttp
from aiohttp import web

log = logging.getLogger("call-bridge")
XAI = "https://api.x.ai/v1"
TWILIO = "https://api.twilio.com/2010-04-01"
RESULT_STATUSES = {"completed", "no_answer", "busy", "failed", "canceled"}
# Twilio's CallStatus, as amux names it.
TWILIO_END = {"completed": "completed", "busy": "busy", "no-answer": "no_answer", "failed": "failed", "canceled": "canceled"}

END_CALL_TOOL = {
    "type": "function",
    "name": "end_call",
    "description": "Hang up. Call this once the goal is done or cannot be done, after saying goodbye.",
    "parameters": {
        "type": "object",
        "properties": {
            "outcome": {"type": "string", "description": "One line: what was achieved, e.g. 'Booked: table for 2, Fri 7pm, under Ethan'."},
            "summary": {"type": "string", "description": "Two or three sentences on how the call went and anything the worker must follow up."},
        },
        "required": ["outcome"],
    },
}


# Whatever amux sends, the person who answers hears first that this is an AI.
DISCLOSE = ("\n\nRule of this line: your first sentence on the call says plainly that you are an AI assistant "
            "and on whose behalf you are calling, using the words \"AI assistant\".")


def env(k, d=""):
    return os.environ.get(k, d).strip()


class Call:
    def __init__(self, req):
        self.id = req["call_id"]
        self.to = req["to"]
        self.frm = req.get("from") or env("TWILIO_FROM")
        self.instructions = req.get("instructions") or ""
        self.voice = (req.get("voice") or "ara").lower()
        self.max_minutes = float(req.get("max_minutes") or 10)
        self.result_url = req["result_url"]
        self.events_url = req.get("events_url") or req["result_url"].rsplit("/", 1)[0] + "/events"
        self.token = req["result_token"]
        self.key = secrets.token_urlsafe(24)
        self.sid = ""
        self.answered_at = None
        self.media_started = False
        self.transcript = []          # [{who, text, t}]
        self.outcome = ""
        self.summary = ""
        self.reported = False
        self.end_status = ""          # Twilio's own word for how the call ended, when it said
        self.hangup = asyncio.Event()


CALLS: dict[str, Call] = {}


async def post_amux(call, url, body):
    headers = {"Authorization": f"Bearer {call.token}", "X-Amux-Call-Token": call.token}
    try:
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=20)) as s:
            async with s.post(url, json=body, headers=headers, ssl=False) as r:
                text = await r.text()
                if r.status >= 300:
                    log.warning("verdict=amux_report_refused call=%s url=%s http=%s body=%s", call.id, url, r.status, text[:200])
                return r.status
    except Exception as e:  # noqa: BLE001 - logged, the call goes on
        log.warning("verdict=amux_report_failed call=%s url=%s error=%s", call.id, url, e)
        return 0


async def event(call, name):
    await post_amux(call, call.events_url, {"event": name})


async def twilio(method, path, data=None):
    sid, tok = env("TWILIO_ACCOUNT_SID"), env("TWILIO_AUTH_TOKEN")
    auth = aiohttp.BasicAuth(sid, tok)
    async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=20)) as s:
        async with s.request(method, f"{TWILIO}/Accounts/{sid}{path}", data=data, auth=auth) as r:
            body = await r.json(content_type=None)
            if r.status >= 300:
                raise RuntimeError(body.get("message") or f"Twilio HTTP {r.status}")
            return body


async def hang_up(call, why):
    log.info("verdict=call_hangup call=%s why=%s", call.id, why)
    call.hangup.set()
    if call.sid and not env("BRIDGE_FAKE_DIAL"):
        try:
            await twilio("POST", f"/Calls/{call.sid}.json", {"Status": "completed"})
        except Exception as e:  # noqa: BLE001
            log.warning("verdict=call_hangup_failed call=%s error=%s", call.id, e)


async def summarize(call):
    """When the model hung up without saying how it went (or never got to), the
    outcome is read from the transcript."""
    lines = "\n".join(f"{t['who']}: {t['text']}" for t in call.transcript)
    if not lines.strip():
        return "", ""
    prompt = ("A phone call an AI assistant made. Its goal:\n" + call.instructions[-1500:] + "\n\nTranscript:\n" + lines +
              "\n\nAnswer as JSON {\"outcome\": one line saying what was achieved, \"summary\": two or three sentences}.")
    try:
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=40)) as s:
            async with s.post(f"{XAI}/chat/completions", headers={"Authorization": f"Bearer {env('XAI_API_KEY')}"},
                              json={"model": env("XAI_SUMMARY_MODEL", "grok-4.3"), "response_format": {"type": "json_object"},
                                    "messages": [{"role": "user", "content": prompt}]}) as r:
                d = await r.json(content_type=None)
        out = json.loads(d["choices"][0]["message"]["content"])
        return str(out.get("outcome", "")), str(out.get("summary", ""))
    except Exception as e:  # noqa: BLE001
        log.warning("verdict=call_summary_failed call=%s error=%s", call.id, e)
        return "", ""


async def report(call, status, error=""):
    if call.reported:
        return
    call.reported = True
    if status == "completed" and not call.outcome:
        call.outcome, s = await summarize(call)
        call.summary = call.summary or s
    dur = round(time.time() - call.answered_at) if call.answered_at else 0
    body = {"status": status, "outcome": call.outcome, "summary": call.summary, "transcript": call.transcript, "duration_s": dur}
    if error:
        body["error"] = error
    await event(call, "ended")
    code = await post_amux(call, call.result_url, body)
    log.info("verdict=call_reported call=%s status=%s turns=%d duration_s=%d amux_http=%s", call.id, status, len(call.transcript), dur, code)
    CALLS.pop(call.id, None)


# ── amux → bridge ──

async def create_call(request):
    if request.match_info["secret"] != env("BRIDGE_SECRET") or not env("BRIDGE_SECRET"):
        return web.json_response({"error": "not found"}, status=404)
    try:
        call = Call(await request.json())
    except (KeyError, ValueError) as e:
        return web.json_response({"error": f"bad request: {e}"}, status=400)
    if not call.to.startswith("+"):
        return web.json_response({"error": "to must be E.164"}, status=400)
    CALLS[call.id] = call
    base = env("BRIDGE_PUBLIC_URL").rstrip("/")
    ws = base.replace("https://", "wss://").replace("http://", "ws://")
    if env("BRIDGE_FAKE_DIAL"):
        call.sid = "FAKE" + call.key[:12]
    else:
        twiml = f'<Response><Connect><Stream url="{ws}/media/{call.id}/{call.key}"/></Connect></Response>'
        try:
            d = await twilio("POST", "/Calls.json", {
                "To": call.to, "From": call.frm, "Twiml": twiml,
                "StatusCallback": f"{base}/status/{call.id}/{call.key}",
                "StatusCallbackEvent": ["initiated", "ringing", "answered", "completed"],
                "TimeLimit": str(int(call.max_minutes * 60) + 30),
            })
        except Exception as e:  # noqa: BLE001
            CALLS.pop(call.id, None)
            log.warning("verdict=call_dial_refused call=%s error=%s", call.id, e)
            return web.json_response({"error": str(e)}, status=502)
        call.sid = d["sid"]
    log.info("verdict=call_dialing call=%s sid=%s to=%s***", call.id, call.sid, call.to[:5])
    asyncio.get_running_loop().call_later(call.max_minutes * 60 + 120, lambda: asyncio.ensure_future(report(call, "failed", "the call never reported an end")))
    return web.json_response({"provider_sid": call.sid, "media_path": f"/media/{call.id}/{call.key}"})


# ── Twilio → bridge ──

def lookup(request):
    call = CALLS.get(request.match_info["call_id"])
    if not call or not secrets.compare_digest(call.key, request.match_info["key"]):
        return None
    return call


async def status_callback(request):
    call = lookup(request)
    if not call:
        return web.Response(status=404)
    st = (await request.post()).get("CallStatus", "")
    log.info("verdict=call_status call=%s twilio_status=%s", call.id, st)
    if st == "ringing":
        await event(call, "ringing")
    elif st == "in-progress":
        call.answered_at = call.answered_at or time.time()
        await event(call, "answered")
    elif st in TWILIO_END:
        call.end_status = TWILIO_END[st]
        call.hangup.set()
        # A call with a conversation reports from the media handler, after the transcript closes.
        if not call.media_started:
            await report(call, TWILIO_END[st])
    return web.Response(text="ok")


async def media(request):
    call = lookup(request)
    if not call:
        return web.Response(status=404)
    tw = web.WebSocketResponse()
    await tw.prepare(request)
    call.media_started = True
    stream_sid = None
    xai_ws = None
    sess = aiohttp.ClientSession()
    t0 = time.time()
    callee_at = {}          # item_id -> its line in the transcript: xAI resends a turn's text as it grows
    agent_text = {}         # response/item id -> agent text so far
    ending = False
    seen = {}

    def said(who, text, item=None):
        text = (text or "").strip()
        if not text:
            return
        if item and item in callee_at:
            call.transcript[callee_at[item]]["text"] = text
            return
        call.transcript.append({"who": who, "text": text, "t": round(time.time() - t0, 1)})
        if item:
            callee_at[item] = len(call.transcript) - 1

    async def from_xai():
        nonlocal ending
        async for msg in xai_ws:
            if msg.type != aiohttp.WSMsgType.TEXT:
                continue
            e = json.loads(msg.data)
            t = e.get("type", "")
            seen[t] = seen.get(t, 0) + 1
            if t in ("response.output_audio.delta", "response.audio.delta") and stream_sid:
                await tw.send_json({"event": "media", "streamSid": stream_sid, "media": {"payload": e["delta"]}})
            elif t == "input_audio_buffer.speech_started" and stream_sid:
                await tw.send_json({"event": "clear", "streamSid": stream_sid})   # the callee talks over the agent
            elif t.startswith("conversation.item.input_audio_transcription."):
                said("callee", e.get("transcript") or e.get("text") or "", e.get("item_id") or None)
            elif t in ("response.output_audio_transcript.done", "response.audio_transcript.done"):
                said("agent", e.get("transcript", ""))
            elif t in ("response.output_audio_transcript.delta", "response.audio_transcript.delta"):
                k = e.get("item_id") or e.get("response_id") or ""
                agent_text[k] = agent_text.get(k, "") + e.get("delta", "")
            elif t == "response.function_call_arguments.done" and e.get("name") == "end_call":
                try:
                    a = json.loads(e.get("arguments") or "{}")
                except ValueError:
                    a = {}
                call.outcome, call.summary = str(a.get("outcome", "")), str(a.get("summary", ""))
                await xai_ws.send_json({"type": "conversation.item.create", "item": {
                    "type": "function_call_output", "call_id": e.get("call_id"), "output": "{\"ok\":true}"}})
                ending = True
                # Let the goodbye finish playing before the line drops.
                asyncio.get_running_loop().call_later(3.0, lambda: asyncio.ensure_future(hang_up(call, "end_call")))
            elif t == "error":
                log.warning("verdict=xai_error call=%s error=%s", call.id, json.dumps(e)[:300])

    async def open_xai():
        nonlocal xai_ws
        model = env("XAI_REALTIME_MODEL", "grok-voice-latest")
        xai_ws = await sess.ws_connect(f"wss://api.x.ai/v1/realtime?model={model}",
                                       headers={"Authorization": f"Bearer {env('XAI_API_KEY')}"}, heartbeat=20)
        await xai_ws.send_json({"type": "session.update", "session": {
            "instructions": call.instructions + DISCLOSE,
            "voice": call.voice,
            "turn_detection": {"type": "server_vad", "idle_timeout_ms": 5000},
            "audio": {"input": {"format": {"type": "audio/pcmu"}, "transcription": {"model": "grok-transcribe"}},
                      "output": {"format": {"type": "audio/pcmu"}}},
            "tools": [END_CALL_TOOL],
        }})
        return asyncio.ensure_future(from_xai())

    reader = None
    limit = asyncio.get_running_loop().call_later(call.max_minutes * 60, lambda: asyncio.ensure_future(hang_up(call, "max_minutes")))
    try:
        async for msg in tw:
            if msg.type != aiohttp.WSMsgType.TEXT:
                continue
            m = json.loads(msg.data)
            ev = m.get("event")
            if ev == "start":
                stream_sid = m["start"]["streamSid"]
                call.answered_at = call.answered_at or time.time()
                reader = await open_xai()
                log.info("verdict=call_media_started call=%s", call.id)
            elif ev == "media" and xai_ws is not None and not xai_ws.closed:
                await xai_ws.send_json({"type": "input_audio_buffer.append", "audio": m["media"]["payload"]})
            elif ev == "stop":
                break
            if call.hangup.is_set() and env("BRIDGE_FAKE_DIAL"):
                break
    finally:
        limit.cancel()
        for k, txt in agent_text.items():
            if txt and not any(x["who"] == "agent" and x["text"] == txt.strip() for x in call.transcript):
                said("agent", txt)
        if xai_ws is not None:
            await xai_ws.close()
        if reader:
            reader.cancel()
        await sess.close()
        if not tw.closed:
            await tw.close()
        call.transcript.sort(key=lambda x: x["t"])
        # Twilio's status callback usually lands just after the stream stops; a
        # call that failed mid-conversation must not be reported as completed.
        if not env("BRIDGE_FAKE_DIAL"):
            for _ in range(20):
                if call.end_status:
                    break
                await asyncio.sleep(0.25)
        log.info("verdict=call_media_ended call=%s ended_by_agent=%s xai_events=%s", call.id, ending,
                 ",".join(f"{k}:{v}" for k, v in sorted(seen.items())))
        await report(call, call.end_status or "completed", "" if call.end_status in ("", "completed") else f"Twilio ended the call: {call.end_status}")
    return tw


async def fake_calls(request):
    """Tests only (BRIDGE_FAKE_DIAL): where each call's media stream is, so the
    test can be the phone for a call amux placed."""
    if not env("BRIDGE_FAKE_DIAL") or request.match_info["secret"] != env("BRIDGE_SECRET"):
        return web.json_response({"error": "not found"}, status=404)
    return web.json_response({c.id: f"/media/{c.id}/{c.key}" for c in CALLS.values()})


async def health(_):
    return web.json_response({"ok": True, "calls": len(CALLS), "fake_dial": bool(env("BRIDGE_FAKE_DIAL"))})


def app():
    a = web.Application()
    a.add_routes([
        web.get("/health", health),
        web.post("/{secret}/calls", create_call),
        web.get("/{secret}/calls", fake_calls),
        web.post("/status/{call_id}/{key}", status_callback),
        web.get("/media/{call_id}/{key}", media),
    ])
    return a


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(name)s %(message)s")
    missing = [k for k in ("XAI_API_KEY", "BRIDGE_SECRET", "BRIDGE_PUBLIC_URL") if not env(k)]
    if not env("BRIDGE_FAKE_DIAL"):
        missing += [k for k in ("TWILIO_ACCOUNT_SID", "TWILIO_AUTH_TOKEN") if not env(k)]
    if missing:
        raise SystemExit("call-bridge: missing " + ", ".join(missing))
    web.run_app(app(), host=env("BRIDGE_HOST", "127.0.0.1"), port=int(env("BRIDGE_PORT", "8790")))
