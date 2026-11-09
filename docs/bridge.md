# The chat bridge: WhatsApp, iMessage, email, Slack, Telegram and Discord into amux

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

### Your own chats: WhatsApp, iMessage and email (no setup)

Three places only you write are a direct line to amux (`bridge/self_chat.rs`):

| Chat | Read by | Answers go back as |
|---|---|---|
| WhatsApp "message yourself" | the WhatsApp app's database on this Mac, every 3 s (`whatsapp_app.rs`) | a message in that chat |
| iMessage to your own number | Messages' `chat.db` on this Mac, every 3 s (`imessage.rs`) | an iMessage in that thread |
| Email from you to you | your Gmail account, every 60 s (`email.rs`) | a reply in the same email thread |

The first word says where it goes:

- `@worker what you want`: posted in #general and delivered to that worker,
  which does the work and answers in the thread.
- `#channel what you want`: posted in that channel (an @worker in it is
  delivered too). `#amux` is the channel even though a worker is also called amux.
- `@person what you want`: posted in your direct message with that person in
  messages; what they write back comes to the chat.

For email the subject is the first word (`Subject: @amux is the deploy out?`),
and the body follows; with no subject, the body's first line is. Reply in the
same email thread to continue: the reply lands in the same messages thread and
reaches the same worker without a new @. Your signature and the quoted mail
are cut.

Only answers come back: replies in threads the chat started, and the other
person's posts in a direct message you wrote in. Never the rest of a channel,
and nothing for a message older than a week. Anything that does not start with
`@` or `#` stays a note to yourself, and amux's own texts and reports to you
are left alone. A name that matches nothing gets one line back saying so.

- **WhatsApp:** answers are typed into the WhatsApp app, so the Mac must be
  unlocked and idle for about five seconds. While it is locked the answer
  waits (`bridge_outbound_waiting`). Needs `AMUX_OWNER_PHONE` and the
  Accessibility grant for `WhatsApp Send` (see `/whatsapp`). Off with
  `AMUX_WHATSAPP_APP=0`. amux opens WhatsApp in the background when it is not
  running, because its database is current only then.
- **iMessage:** your own thread is the one whose handle is `AMUX_OWNER_PHONE`,
  or one of `AMUX_IMESSAGE_SELF_HANDLES` (a comma list that also takes your
  Apple ID email). Reading `chat.db` needs Full Disk Access for
  `amux-server-rs`; without it `bridge_imessage_read_failed` says so once.
  Answers go through `~/.amux/imessage/send-imessage.sh` (the `/imessage`
  skill's script), and only ever to your own thread. Off with
  `AMUX_IMESSAGE_BRIDGE=0`.
- **Email:** the account is `AMUX_BRIDGE_EMAIL`, else `AMUX_OWNER_EMAIL`, and
  it must be a connected Gmail account. With neither set it is off: no other
  connected account is guessed. Replies go through the amux email API,
  so they are in `/api/email/log`. Off with `AMUX_BRIDGE_EMAIL=0`. What it has
  seen and sent is in `~/.amux/bridge-email.json`, so a restart neither
  repeats nor reads its own replies as yours.
- Verdicts: `bridge_self_chat_posted`, `bridge_self_chat_unroutable`,
  `bridge_whatsapp_app_filed`, `bridge_imessage_filed`, `bridge_email_filed`,
  and a `_read_failed` and `_off` for each.

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
