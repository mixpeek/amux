# The chat bridge: WhatsApp, Slack, Telegram and Discord into amux

A conversation on a chat platform, linked to an amux worker or group. What you
write there is filed in **messages** and delivered to the worker; the worker's
answer comes back to the chat. Code: `crates/amux-server/src/bridge/`,
`api/bridge.rs`. Proof: `scripts/bridge-e2e.py`.

## The shape (owner decision, 2026-10-27)

- **Connectors own each platform:** its credentials (Settings > Connectors) and
  a small adapter that receives and sends (`bridge/adapters.rs`).
- **Messages own delivery:** an inbound message is a durable message with
  turn-boundary delivery, receipts and history, like any other.
- **The bridge is one table and one rule between them** (`bridge_links`). A new
  platform is an adapter, never a new subsystem.

### Who gets a message

| Conversation | Linked to | Delivered to |
|---|---|---|
| Direct chat | a worker | that worker, every message |
| Direct chat | a group | only the workers a message @mentions |
| Group chat | a worker or a group | only the workers a message @mentions |

Every message is filed in messages either way. An unaddressed one is handed to
the next mentioned worker as the conversation's recent context, so "@worker
what did I say earlier?" works.

### How the answer gets back

- **Automatically.** When a turn that began with a bridged message ends, the
  turn's final text (read from the worker's own transcript, never its screen)
  goes back to the chat.
- **Explicitly.** `amux reply br-xxxxxxxxxxxx --stdin` sends another message.
  The ref is printed in the delivered message.

### Trust

- Only your identities speak with your authority: `AMUX_BRIDGE_OWNERS`
  (`whatsapp:<digits>,slack:<user id>,telegram:<id>,discord:<id>`), and
  `AMUX_OWNER_PHONE` for WhatsApp.
- Anyone else is delivered as quoted, untrusted input, and cannot `/link`.
- A conversation that has ever included someone other than you is shared. A
  reply into it waits for your approval: a needsyou card with a
  `#bridge-approve=` link.
- Webhooks are checked against the platform's signature before anything is
  read: WhatsApp's app secret, and Slack's signing secret with a five-minute
  replay window.

## Setting up each platform

| Platform | Receives by | Needs a public URL | Keys |
|---|---|---|---|
| WhatsApp | webhook `/api/bridge/hook/whatsapp` | yes | WHATSAPP_TOKEN, WHATSAPP_PHONE_NUMBER_ID, WHATSAPP_APP_SECRET, WHATSAPP_VERIFY_TOKEN |
| Slack | Events API `/api/bridge/hook/slack` | yes | SLACK_BOT_TOKEN, SLACK_SIGNING_SECRET |
| Telegram | long poll | no | TELEGRAM_BOT_TOKEN |
| Discord | channel reads every 4 s | no | DISCORD_BOT_TOKEN, DISCORD_CHANNELS |

Then, from the chat: `/link <worker or group>`. Or from amux:
`POST /api/bridge/links {"platform","conversation","is_group","target"}`.

### WhatsApp

1. **A Meta app with the WhatsApp product** (developers.facebook.com, free).
   Meta's test number works for a direct chat with up to five numbers. Each
   one confirms with a code sent to that WhatsApp.
2. **A real number** for anything beyond testing. That number cannot also be
   on the WhatsApp app. Paid conversations start only when amux messages
   first; replies inside 24 hours of your message are free.
3. **WhatsApp groups need an Official Business Account** (the verified green
   check): Meta's Groups API is limited to those, caps a group at 8, and
   members join by invite link. Until then, a direct chat with the amux number
   linked to an amux group gives the same result for you alone: @mention a
   worker to reach it.
4. **A public HTTPS URL** for the webhook, for example Tailscale Funnel on
   this Mac's amux, or the cloud front door.
5. **The webhook routes open to the platform.** `/api/bridge/hook/` must be a
   public prefix in `api/auth.rs` (the handler checks the platform's signature
   first). `auth.rs` is an `[owner]` path in `scripts/review-tiers.txt`, so
   that one-line change waits for the owner's approval of its diff; until it
   lands, WhatsApp and Slack webhooks get 401. Telegram and Discord poll, so
   they work without it.

## Status

`GET /api/bridge`: which platforms are configured and which keys are missing,
every link, and recent replies with their state (sent, held, failed with
attempts). Log verdicts: `bridge_inbound_filed`, `bridge_outbound_sent`,
`bridge_outbound_held`, `bridge_outbound_failed`, `bridge_relay_turn`,
`bridge_*_signature_refused`.
