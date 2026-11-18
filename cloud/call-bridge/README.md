# Call bridge

Places the calls amux approves and holds the conversation: Twilio dials, the
call audio streams here over a WebSocket, and xAI's realtime voice model talks
(docs/design/phone-calls.md). It runs on the cloud host because Twilio needs a
public WebSocket, which the amux tunnel cannot carry.

## Contract

| From | Request | |
|---|---|---|
| amux | `POST /<BRIDGE_SECRET>/calls` | the call; answers `{provider_sid}` |
| Twilio | `POST /status/<call>/<key>` | ringing, answered, ended |
| Twilio | `GET /media/<call>/<key>` (WebSocket) | the call's audio, μ-law 8 kHz |
| bridge | `POST <amux>/api/calls/<id>/events`, `/result` | with the call's token |

amux sends no credential, so its address carries the secret:
`AMUX_CALL_BRIDGE_URL=https://cloud.amux.io/call-bridge/<BRIDGE_SECRET>`.

The bridge adds one rule to every call's instructions: the first sentence says
it is an AI assistant and for whom. The agent hangs up with an `end_call`
tool that carries the outcome; a call that ends otherwise has its outcome read
from the transcript.

## Tests

Both need `XAI_API_KEY` and talk to xAI for real. Neither needs Twilio: the
test plays the phone line, with a second xAI voice as the restaurant host.

    python3 cloud/call-bridge/test_bridge.py       # the bridge alone, a stand-in amux
    python3 cloud/call-bridge/test_amux_e2e.py     # through a private amux server

## Deploy (cloud host)

    sudo install -D -m 644 bridge.py /opt/amux/call-bridge/bridge.py
    sudo install -m 600 /dev/null /etc/amux/call-bridge.env   # then fill in the keys
    sudo cp call-bridge.service /etc/systemd/system/amux-call-bridge.service
    sudo systemctl enable --now amux-call-bridge
    # add nginx-location.conf to the cloud.amux.io server block, then
    sudo nginx -t && sudo systemctl reload nginx

Needs `python3-aiohttp`. Logs: `/var/log/amux-call-bridge.log`, every call line
starts with `verdict=`.

On the Mac, set `AMUX_CALL_BRIDGE_URL` and `TWILIO_FROM` under Connectors →
Phone. A first real call goes to the owner's own number (`AMUX_CALL_ALLOWLIST`).
