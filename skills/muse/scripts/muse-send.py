#!/usr/bin/env python3
"""muse-send — queue a command for Ethan's Muse (main chat).

Appends one JSON line to ~/.muse-cli/inbox.jsonl on this Mac.
Muse polls that file every ~5 minutes from its side (via the paired
Mac connection), relays each new line into the main chat verbatim,
and the main agent executes it with full conversation context.

Sent from an amux worker ($AMUX_SESSION set), the command carries a reply
line: Muse runs `muse-reply <id> "..."` on this Mac and the answer arrives
in that worker as owner input. --no-reply leaves it off.

Usage:
  muse-send "check my calendar for tomorrow"
  muse-send check the brex balance        (args are joined)
  echo "summarize my unread mail" | muse-send
  muse-send --no-reply "fire and forget"
  muse-send --status                      (show queued/unacked count)
"""
import json
import os
import socket
import subprocess
import sys
import uuid
from datetime import datetime

INBOX_DIR = os.path.expanduser("~/.muse-cli")
INBOX = os.path.join(INBOX_DIR, "inbox.jsonl")


def tmux_session() -> str:
    # $AMUX_SESSION can be missing from a shell the worker started.
    if not os.environ.get("TMUX"):
        return ""
    try:
        name = subprocess.run(["tmux", "display-message", "-p", "#S"], capture_output=True,
                              text=True, timeout=5).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""
    # amux names a worker's tmux session amux-<worker>.
    return name[len("amux-"):] if name.startswith("amux-") else ""


def queue(text: str, reply: bool) -> dict:
    os.makedirs(INBOX_DIR, exist_ok=True)
    entry = {
        "id": uuid.uuid4().hex[:12],
        "ts": datetime.now().astimezone().isoformat(timespec="seconds"),
        "host": socket.gethostname(),
        "cwd": os.getcwd(),
        "text": text.strip(),
    }
    session = os.environ.get("AMUX_SESSION", "").strip() or tmux_session()
    if reply and session:
        entry["reply_to"] = session
        # Muse relays the text verbatim, so the way back has to be in it.
        entry["text"] += (
            f"\n\n(From amux worker {session} on {entry['host']}. When done, send the result back"
            f" with work and context: muse-reply {entry['id']} \"<your reply>\""
            f". A longer reply can be piped: ... | muse-reply {entry['id']})"
        )
    with open(INBOX, "a", encoding="utf-8") as f:
        f.write(json.dumps(entry) + "\n")
    return entry


def count() -> int:
    try:
        with open(INBOX, encoding="utf-8") as f:
            return sum(1 for line in f if line.strip())
    except FileNotFoundError:
        return 0


def main() -> int:
    args = sys.argv[1:]
    if args and args[0] in ("-h", "--help"):
        print(__doc__)
        return 0
    if args and args[0] == "--status":
        print(f"{count()} command(s) in {INBOX}")
        print("Muse polls every ~5 min; a command is done when it shows up in the main chat.")
        return 0
    reply = True
    if args and args[0] == "--no-reply":
        reply, args = False, args[1:]
    text = " ".join(args).strip()
    if not text and not sys.stdin.isatty():
        text = sys.stdin.read().strip()
    if not text:
        print(__doc__)
        return 2
    entry = queue(text, reply)
    back = f" Replies go to {entry['reply_to']}." if "reply_to" in entry else ""
    print(f"queued {entry['id']} — Muse picks it up within ~5 min and runs it in the main chat.{back}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
