# Phone calls

Owner request (Ethan, 2026-11-12): "real-time outbound calling on my behalf", built
end to end from existing amux primitives, with calling as a first-class surface: "a tab
for workers (or global) ... which captures the capability, call logs, etc."

## What a call is

A worker asks amux to phone a number with a goal and the context it needs. A voice
model holds the conversation. When the call ends, amux stores the transcript and the
outcome, logs the call against the contact, and wakes the worker with an event so it
carries on.

```
worker ── amux call ──▶ amux (calls table, approval) ──▶ call bridge ──▶ Twilio ──▶ phone
                                                            │  ▲
                                                            ▼  │ audio (WebSocket)
                                                         xAI realtime voice
worker ◀── call.completed event ◀── amux ◀── result webhook (transcript, outcome)
```

## Pieces, mapped to primitives

| Need | Primitive | Where |
|---|---|---|
| Keys (Twilio SID, token, from-number; xAI key) | vault + connectors | a `phone` connector; values in the vault |
| Start a call | worker + CLI | `amux call <number> --stdin` (goal and context) |
| Approval for numbers outside the company | the email gate's approval path | a held call shows in the Calls tab; the owner approves it |
| When | scheduler | `once` for "call at 3pm"; a no-answer retry is a once schedule |
| Results | filesystem + CRM + events | transcript file, CRM interaction `call`, `call.completed` event |
| Wake the worker | event subscriptions | the caller is subscribed to its own call's completion |
| See and review | dashboard | the Calls tab (global, filterable by worker) |

No ninth primitive: a call is a row in a `calls` table with a status, like a held email.

## The call bridge runs in the cloud

Twilio streams call audio over a WebSocket to a public address, and the bridge relays
it to xAI's realtime API (`wss://api.x.ai/v1/realtime`). Neither half can run on this
Mac: the amux tunnel is HTTP long-poll and carries no WebSocket, and live audio must not
ride the shared Wi-Fi. The bridge is a small service on the amux cloud host, beside the
gateway (`cloud/call-bridge/`), adapted from xAI's phone-agent cookbook example.

Its contract with amux is three HTTP calls, so the bridge can be swapped (Twilio plus
xAI today; xAI SIP or another voice model later) without touching amux:

1. `POST <bridge>/calls` from amux: `{call_id, to, from, instructions, voice,
   max_minutes, result_url, result_token}`. The bridge places the call through Twilio's
   REST API with a `<Connect><Stream>` to itself and answers `{provider_sid}`.
2. `POST <amux>/api/calls/<id>/events` from the bridge, bearer `result_token`:
   `ringing`, `answered`, `ended` (status only, for the live view).
3. `POST <amux>/api/calls/<id>/result` from the bridge, bearer `result_token`:
   `{status, outcome, summary, transcript: [{who, text, t}], duration_s, recording_url?}`.

`result_token` is minted per call and stored hashed; a result for one call cannot be
written with another call's token.

## States

`requested` → `held` (needs the owner) → `approved` → `dialing` → `in_progress` →
`completed` | `no_answer` | `busy` | `failed` | `canceled`.

A call to a number on the owner's allowlist (his own numbers, company numbers) skips
`held`. Every other number is held until the owner approves that exact call (number,
goal, worker), the same rule as an external email. A standing approval can cover a
recurring kind of call. A worker can never approve.

## Instructions the voice model gets

Built by amux, not the worker, so the disclosure cannot be dropped:

1. "You are an AI assistant calling on behalf of Ethan Steininger." First sentence of
   every call, always.
2. The worker's goal and context.
3. Limits from the request (do not agree to payments, do not share personal data
   beyond what the context names, hang up after `max_minutes`).
4. "If you cannot complete the goal, say you will follow up, and end politely."

## The Calls tab

A global tab, one row per call, newest first: when, worker, who (contact name from the
CRM, else the number), goal, status chip, duration, outcome. Filter chips: worker,
status. A held call has Approve / Decline inline. A row opens a drawer with the goal,
the transcript (speaker turns), the outcome and summary, and links to the worker and
the CRM contact. The worker's own view gets the same list filtered to it. On a phone it
keeps the one-row rule: filters scroll sideways in the top bar.

## Events

`call.requested`, `call.held`, `call.started`, `call.completed` (with `status`,
`outcome`, `worker`, `to`, `contact`). Documented in the event catalog, so
`amux subscribe "a call to a customer ends without a booking"` compiles.

## Testing

- A fake bridge (`tests/` and an e2e fixture) implements the three-call contract and
  replays a scripted conversation, so the whole amux side (request, hold, approve,
  dial, result, transcript file, CRM log, event, worker wake, tab) runs in CI.
- The real proof is one call to the owner's own phone through the deployed bridge,
  with the transcript checked against what was said.

## Needs the owner

- A Twilio account and number (about $1-2 a month plus per-minute charges) and an xAI
  API key with realtime voice. Both are money.
- Legal: the AI disclosure is built in; recording consent rules differ by state, and
  automated calls to cell phones fall under the TCPA. The bridge records the transcript
  only, not audio, unless the owner turns recording on.
