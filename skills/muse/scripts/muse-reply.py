#!/usr/bin/env python3
"""muse-reply: send Muse's answer back to the amux worker that asked.

Muse runs this on the Mac after doing a command that came from muse-send.
It finds the command by id in ~/.muse-cli/inbox.jsonl, posts the reply to
that worker through the local amux server (it arrives as owner input, with
the original command quoted for context) and records it in
~/.muse-cli/replies.jsonl.

Usage:
  muse-reply <id> "done: tomorrow has 3 meetings, first at 9:30"
  some-command | muse-reply <id>
  muse-reply --to <worker> "work for you, unprompted"
"""
import hashlib
import json
import os
import ssl
import sys
import urllib.request
from datetime import datetime

INBOX_DIR = os.path.expanduser("~/.muse-cli")
INBOX = os.path.join(INBOX_DIR, "inbox.jsonl")
REPLIES = os.path.join(INBOX_DIR, "replies.jsonl")


# On a Mac with no amux server of its own (a laptop whose Mac app talks to a
# server elsewhere), ~/.muse-cli/amux.json names that server:
#   {"url": "https://<server>:8824", "token": "<owner token>"}
CONFIG = os.path.join(INBOX_DIR, "amux.json")


def config() -> dict:
    try:
        with open(CONFIG, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def amux_url() -> str:
    if config().get("url"):
        return config()["url"].rstrip("/")
    try:
        with open(os.path.expanduser("~/.amux/endpoint.json"), encoding="utf-8") as f:
            url = json.load(f).get("canonical_url", "")
        if url:
            return url.rstrip("/")
    except (OSError, ValueError):
        pass
    return os.environ.get("AMUX_URL", "https://localhost:8824").rstrip("/")


def find(entry_id: str) -> dict:
    try:
        with open(INBOX, encoding="utf-8") as f:
            for line in f:
                try:
                    e = json.loads(line)
                except ValueError:
                    continue
                if e.get("id") == entry_id:
                    return e
    except FileNotFoundError:
        pass
    return {}


def post(session: str, text: str) -> int:
    # A stable msg_id makes a retry of the same reply a no-op on the server: a
    # send that outlived the timeout had still landed, and the retry doubled it.
    msg_id = "muse-" + hashlib.sha256(f"{session}\n{text}".encode()).hexdigest()[:24]
    req = urllib.request.Request(
        f"{amux_url()}/api/sessions/{session}/send",
        data=json.dumps({"text": text, "msg_id": msg_id}).encode(),
        headers={"Content-Type": "application/json",
                 **({"Authorization": "Bearer " + config()["token"]} if config().get("token") else {})},
        method="POST",
    )
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE  # amux serves a self-signed certificate
    try:
        with urllib.request.urlopen(req, timeout=60, context=ctx) as r:
            return r.status
    except urllib.error.HTTPError as e:
        print(f"amux refused the reply: {e.code} {e.read()[:300].decode(errors='replace')}", file=sys.stderr)
        return e.code
    except TimeoutError:
        # The server holds a send to a busy worker; it usually lands anyway.
        print(f"no answer from {amux_url()} in 60 s; it may still be delivered, and retrying is safe"
              " (the same reply is sent only once)", file=sys.stderr)
        return 0
    except OSError as e:
        print(f"amux unreachable at {amux_url()}: {e}", file=sys.stderr)
        return 0


def main() -> int:
    args = sys.argv[1:]
    if not args or args[0] in ("-h", "--help"):
        print(__doc__)
        return 0 if args else 2
    if args[0] == "--to":
        if len(args) < 2:
            print(__doc__)
            return 2
        entry, session, args = {}, args[1], args[2:]
    else:
        entry = find(args[0])
        if not entry:
            print(f"no command {args[0]} in {INBOX}", file=sys.stderr)
            return 1
        session, args = entry.get("reply_to", ""), args[1:]
        if not session:
            print(f"command {entry['id']} was sent without a reply address (not from an amux worker)", file=sys.stderr)
            return 1
    text = " ".join(args).strip()
    if not text and not sys.stdin.isatty():
        text = sys.stdin.read().strip()
    if not text:
        print("nothing to send", file=sys.stderr)
        return 2
    if entry:
        asked = entry["text"].split("\n\n(From amux worker")[0]
        body = f"[Muse, replying to {entry['id']}: \"{asked}\"]\n\n{text}"
    else:
        body = f"[Muse]\n\n{text}"
    status = post(session, body)
    os.makedirs(INBOX_DIR, exist_ok=True)
    with open(REPLIES, "a", encoding="utf-8") as f:
        f.write(json.dumps({
            "id": entry.get("id"), "to": session, "status": status,
            "ts": datetime.now().astimezone().isoformat(timespec="seconds"), "text": text,
        }) + "\n")
    if 200 <= status < 300:
        print(f"sent to {session}")
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
