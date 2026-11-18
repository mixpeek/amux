#!/usr/bin/env python3
"""The call bridge, end to end without a phone line: this test is Twilio. It
asks the bridge for a call (BRIDGE_FAKE_DIAL), opens the media stream as Twilio
would, and a second xAI voice plays the person who answers. Audio both ways is
μ-law 8 kHz in 20 ms frames, paced in real time, with silence between turns, as
Twilio sends it. A stand-in amux takes the events and the result.

    XAI_API_KEY=... python3 cloud/call-bridge/test_bridge.py

Passes when the result reaches amux with status completed, both sides in the
transcript, an outcome, and the agent hung up by itself before the limit.
With AMUX_URL_UNDER_TEST and a call id, it reports to that amux instead (see
--amux in main()).
"""
import argparse
import asyncio
import base64
import json
import os
import socket
import subprocess
import sys
import time

import aiohttp
from aiohttp import web

HERE = os.path.dirname(os.path.abspath(__file__))
FRAME = 160                      # 20 ms of μ-law at 8 kHz
SILENCE = b"\xff" * FRAME
HOST_PERSONA = ("You answer the phone at Lucia's, a small Italian restaurant in the West Village, New York. "
                "Speak briefly and naturally, like a busy host. You take reservations by phone; there is no "
                "deposit and no card is needed. Ask for the date, time, party size and a name, offer a nearby "
                "time if the asked one is full (Friday 7pm is full, 7:30pm is open), confirm the booking back, "
                "and say goodbye. Start by answering: \"Lucia's, how can I help you?\"")
# Not amux's own wording on purpose: the bridge must make the agent disclose whatever it is given.
GOAL = ("You are calling on behalf of Ethan Steininger.\n"
        "Goal: book a table for 2 people this Friday around 7pm under the name Ethan. If 7pm is taken, "
        "anything between 6:30pm and 8pm is fine. Do not agree to any payment or give a card number; if "
        "they need one, say Ethan will call back. When the booking is confirmed or impossible, say goodbye "
        "and call end_call with the outcome.\nLimits: do not share personal data beyond the name. "
        "If you cannot complete the goal, say you will follow up, and end politely.")


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


async def stand_in_amux(port, got):
    async def ev(r):
        got["events"].append((await r.json()).get("event")); return web.json_response({"ok": True})

    async def res(r):
        got["auth"] = r.headers.get("X-Amux-Call-Token")
        got["result"] = await r.json(); return web.json_response({"ok": True})
    a = web.Application()
    a.add_routes([web.post("/api/calls/{id}/events", ev), web.post("/api/calls/{id}/result", res)])
    run = web.AppRunner(a); await run.setup(); await web.TCPSite(run, "127.0.0.1", port).start()
    return run


async def phone(bridge, media_path, limit_s):
    """Twilio's side of the media stream, with an xAI voice as the callee."""
    to_bridge = bytearray()       # callee audio waiting to go down the line
    to_callee = bytearray()       # agent audio waiting to reach the callee's ear
    stats = {"frames_out": 0, "frames_in": 0, "clears": 0}
    async with aiohttp.ClientSession() as s:
        callee = await s.ws_connect("wss://api.x.ai/v1/realtime?model=grok-voice-latest",
                                    headers={"Authorization": f"Bearer {os.environ['XAI_API_KEY']}"})
        await callee.send_json({"type": "session.update", "session": {
            "instructions": HOST_PERSONA, "voice": "eve", "turn_detection": {"type": "server_vad"},
            "audio": {"input": {"format": {"type": "audio/pcmu"}}, "output": {"format": {"type": "audio/pcmu"}}}}})
        line = await s.ws_connect(bridge.replace("http", "ws") + media_path)
        await line.send_json({"event": "connected", "protocol": "Call", "version": "1.0.0"})
        await line.send_json({"event": "start", "sequenceNumber": "1", "streamSid": "MZtest",
                              "start": {"streamSid": "MZtest", "callSid": "CAtest", "mediaFormat":
                                        {"encoding": "audio/x-mulaw", "sampleRate": 8000, "channels": 1}}})
        # The phone is picked up: the host greets first.
        await asyncio.sleep(0.5)
        await callee.send_json({"type": "response.create"})
        done = asyncio.Event()

        async def callee_out():
            async for m in callee:
                if m.type == aiohttp.WSMsgType.TEXT:
                    e = json.loads(m.data)
                    if e.get("type") in ("response.output_audio.delta", "response.audio.delta"):
                        to_bridge.extend(base64.b64decode(e["delta"]))

        async def line_in():
            async for m in line:
                if m.type != aiohttp.WSMsgType.TEXT:
                    break
                e = json.loads(m.data)
                if e.get("event") == "media":
                    to_callee.extend(base64.b64decode(e["media"]["payload"]))
                elif e.get("event") == "clear":
                    stats["clears"] += 1; to_callee.clear()
            done.set()

        async def clock():
            """Every 20 ms one frame each way, as a phone line does."""
            nxt = time.monotonic()
            seq = 2
            while not done.is_set():
                out = bytes(to_bridge[:FRAME]) if to_bridge else SILENCE
                del to_bridge[:FRAME]
                inn = bytes(to_callee[:FRAME]) if to_callee else SILENCE
                del to_callee[:FRAME]
                stats["frames_out"] += 1; stats["frames_in"] += inn != SILENCE
                try:
                    await line.send_json({"event": "media", "sequenceNumber": str(seq), "streamSid": "MZtest",
                                          "media": {"payload": base64.b64encode(out.ljust(FRAME, b"\xff")).decode()}})
                    await callee.send_json({"type": "input_audio_buffer.append", "audio": base64.b64encode(inn.ljust(FRAME, b"\xff")).decode()})
                except Exception:  # noqa: BLE001 - the line closed
                    break
                seq += 1
                nxt += 0.02
                await asyncio.sleep(max(0, nxt - time.monotonic()))

        tasks = [asyncio.ensure_future(x) for x in (callee_out(), line_in(), clock())]
        try:
            await asyncio.wait_for(done.wait(), limit_s)
            hung_up_by_agent = True
        except asyncio.TimeoutError:
            hung_up_by_agent = False
            await line.send_json({"event": "stop", "streamSid": "MZtest"})
            await asyncio.sleep(1)
        for t in tasks:
            t.cancel()
        await callee.close()
        if not line.closed:
            await line.close()
    return hung_up_by_agent, stats


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--limit", type=float, default=150)
    args = ap.parse_args()
    assert os.environ.get("XAI_API_KEY"), "XAI_API_KEY is needed"
    bport, aport = free_port(), free_port()
    bridge = f"http://127.0.0.1:{bport}"
    env = dict(os.environ, BRIDGE_PORT=str(bport), BRIDGE_SECRET="s3cret", BRIDGE_PUBLIC_URL=bridge, BRIDGE_FAKE_DIAL="1")
    blog = open(os.path.join(os.environ.get("TMPDIR", "/tmp"), f"call-bridge-test-{bport}.log"), "w")
    proc = subprocess.Popen([sys.executable, os.path.join(HERE, "bridge.py")], env=env, stdout=blog, stderr=subprocess.STDOUT)
    got = {"events": [], "result": None}
    runner = await stand_in_amux(aport, got)
    fails = []

    def check(label, ok, detail=""):
        print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
        if not ok:
            fails.append(label)
    try:
        async with aiohttp.ClientSession() as s:
            for _ in range(50):
                try:
                    async with s.get(bridge + "/health") as r:
                        if r.status == 200:
                            break
                except aiohttp.ClientError:
                    await asyncio.sleep(0.2)
            base = f"http://127.0.0.1:{aport}/api/calls/CALL-TEST"
            req = {"call_id": "CALL-TEST", "to": "+12125550100", "from": "+12125550199", "instructions": GOAL,
                   "voice": "Ara", "max_minutes": 3, "result_url": base + "/result", "events_url": base + "/events",
                   "result_token": "tok-123"}
            async with s.post(bridge + "/wrong/calls", json=req) as r:
                check("a request without the bridge's secret path is refused", r.status == 404, r.status)
            async with s.post(bridge + "/s3cret/calls", json=req) as r:
                d = await r.json()
                check("the bridge accepts the call and answers a provider id", r.status == 200 and d.get("provider_sid"), d)
            async with s.get(bridge + "/media/CALL-TEST/not-the-key") as r:
                check("the media stream needs the call's key", r.status == 404, r.status)
        t0 = time.time()
        by_agent, stats = await phone(bridge, d["media_path"], args.limit)
        took = time.time() - t0
        for _ in range(100):
            if got["result"]:
                break
            await asyncio.sleep(0.5)
        res = got["result"] or {}
        tr = res.get("transcript") or []
        print("\n  transcript:")
        for t in tr:
            print(f"    {t['t']:6.1f}s {t['who']:6} {t['text']}")
        print(f"\n  outcome: {res.get('outcome')!r}\n  summary: {res.get('summary')!r}\n  {took:.0f}s, {stats}\n")
        check("the agent hung up by itself, before the limit", by_agent, f"{took:.0f}s")
        check("the result reached amux with this call's token", got.get("auth") == "tok-123", got.get("auth"))
        check("status completed", res.get("status") == "completed", res.get("status"))
        check("the transcript has both sides", {t["who"] for t in tr} >= {"agent", "callee"}, tr)
        first = next((t["text"] for t in tr if t["who"] == "agent"), "")
        check("the agent says it is an AI calling for Ethan", "ethan" in first.lower() and ("ai" in first.lower().split() or "assistant" in first.lower()), first)
        check("an outcome that names the booking", any(w in (res.get("outcome") or "").lower() for w in ("book", "reserv", "confirm")), res.get("outcome"))
        check("amux heard the call end", "ended" in got["events"], got["events"])
        check("the agent's audio reached the callee", stats["frames_in"] > 50, stats)

        # A call Twilio says failed mid-conversation is reported failed, not completed.
        got["result"] = None
        async with aiohttp.ClientSession() as s:
            async with s.post(bridge + "/s3cret/calls", json=dict(req, call_id="CALL-FAIL")) as r:
                mp = (await r.json())["media_path"]
            line = await s.ws_connect(bridge.replace("http", "ws") + mp)
            await line.send_json({"event": "start", "streamSid": "MZf", "start": {"streamSid": "MZf", "callSid": "CAf"}})
            await asyncio.sleep(1)
            key = mp.rsplit("/", 1)[1]
            async with s.post(f"{bridge}/status/CALL-FAIL/{key}", data={"CallStatus": "failed"}) as r:
                pass
            await line.send_json({"event": "stop", "streamSid": "MZf"})
            for _ in range(40):
                if got["result"]:
                    break
                await asyncio.sleep(0.5)
            await line.close()
        res = got["result"] or {}
        check("a call Twilio says failed mid-call is reported failed", res.get("status") == "failed" and "failed" in res.get("error", ""), res)
    finally:
        proc.terminate()
        await runner.cleanup()
        blog.close()
        print("  bridge log:", blog.name)
    print("PASS: call bridge" if not fails else f"call bridge: {len(fails)} check(s) FAILED")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
