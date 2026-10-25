#!/usr/bin/env python3
"""muse-sync: bring commands queued on the amux server's Mac to this one.

Muse reads ~/.muse-cli/inbox.jsonl only on the Mac it is paired with. When
amux runs on a different Mac, `muse-send` from a worker queues there, where
nothing reads it. Run on the paired Mac, this copies new lines (by id) from
the server's inbox into the local one, so Muse's own poll relays them, and
the `muse-reply` line inside each answers back through the same server.
If Muse can write files on this Mac but not run commands, it appends
{"id": "<command id>", "text": "<reply>"} to ~/.muse-cli/outbox.jsonl and
each run here sends those back the same way (once each).

When amux runs on this same Mac (~/.amux/endpoint.json exists and there is no
amux.json), the inbox is already the local one, so there is nothing to fetch:
each run only sends back the outbox replies, through the local server.

Reads ~/.muse-cli/amux.json:
  {"url": "https://<server>:8824", "token": "<owner token>",
   "remote_inbox": "/Users/<you>/.muse-cli/inbox.jsonl"}   (remote_inbox optional)

Usage:
  muse-sync              copy new commands once, print how many
  muse-sync --install    also run it every minute (launchd)
  muse-sync --uninstall
"""
import json
import os
import ssl
import subprocess
import sys
import urllib.parse
import urllib.request

INBOX_DIR = os.path.expanduser("~/.muse-cli")
INBOX = os.path.join(INBOX_DIR, "inbox.jsonl")
CONFIG = os.path.join(INBOX_DIR, "amux.json")
LOG = os.path.join(INBOX_DIR, "sync.log")
OUTBOX = os.path.join(INBOX_DIR, "outbox.jsonl")
SENT = os.path.join(INBOX_DIR, "outbox-sent.txt")
LABEL = "com.amux.muse-sync"
PLIST = os.path.expanduser(f"~/Library/LaunchAgents/{LABEL}.plist")


def config() -> dict:
    with open(CONFIG, encoding="utf-8") as f:
        return json.load(f)


def local_ids() -> set:
    ids = set()
    try:
        with open(INBOX, encoding="utf-8") as f:
            for line in f:
                try:
                    ids.add(json.loads(line).get("id"))
                except ValueError:
                    pass
    except FileNotFoundError:
        pass
    return ids


def fetch(cfg: dict) -> str:
    remote = cfg.get("remote_inbox") or os.path.join(os.path.expanduser("~"), ".muse-cli", "inbox.jsonl")
    url = cfg["url"].rstrip("/") + "/api/file/raw?path=" + urllib.parse.quote(remote)
    req = urllib.request.Request(url, headers={"Authorization": "Bearer " + cfg.get("token", "")})
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE  # amux serves a self-signed certificate
    with urllib.request.urlopen(req, timeout=20, context=ctx) as r:
        return r.read().decode("utf-8", errors="replace")


def amux_is_local() -> bool:
    return os.path.exists(os.path.expanduser("~/.amux/endpoint.json"))


def sync() -> int:
    if not os.path.exists(CONFIG) and amux_is_local():
        # Same Mac as amux: workers already queue into this inbox. Only the reply path needs work.
        print(f"muse-sync: amux is local, inbox needs no copy; {send_outbox()} repl(ies) sent back")
        return 0
    try:
        cfg = config()
    except (OSError, ValueError) as e:
        print(f"muse-sync: no usable {CONFIG}: {e}", file=sys.stderr)
        return 2
    try:
        body = fetch(cfg)
    except urllib.error.HTTPError as e:
        if e.code == 404:  # nothing queued on the server yet
            print("muse-sync: 0 new (no inbox on the server)")
            return 0
        print(f"muse-sync: server refused: {e.code}", file=sys.stderr)
        return 1
    except OSError as e:
        print(f"muse-sync: server unreachable: {e}", file=sys.stderr)
        return 1
    have = local_ids()
    new = []
    for line in body.splitlines():
        try:
            e = json.loads(line)
        except ValueError:
            continue
        if e.get("id") and e["id"] not in have:
            new.append(line.strip())
            have.add(e["id"])
    if new:
        os.makedirs(INBOX_DIR, exist_ok=True)
        with open(INBOX, "a", encoding="utf-8") as f:
            f.write("\n".join(new) + "\n")
    print(f"muse-sync: {len(new)} new, {send_outbox()} repl(ies) sent back")
    return 0


def send_outbox() -> int:
    """Send replies Muse wrote to the outbox file, each once, via muse-reply."""
    try:
        with open(OUTBOX, encoding="utf-8") as f:
            rows = [json.loads(l) for l in f if l.strip()]
    except (OSError, ValueError):
        return 0
    try:
        with open(SENT, encoding="utf-8") as f:
            sent = set(f.read().split())
    except OSError:
        sent = set()
    reply = os.path.join(os.path.dirname(os.path.realpath(sys.argv[0])), "muse-reply")
    n = 0
    for i, row in enumerate(rows):
        key = f"{row.get('id')}:{i}"
        if key in sent or not row.get("id") or not row.get("text"):
            continue
        r = subprocess.run([reply, row["id"]], input=row["text"], text=True, capture_output=True)
        if r.returncode == 0:
            with open(SENT, "a", encoding="utf-8") as f:
                f.write(key + "\n")
            n += 1
        else:
            print(f"muse-sync: reply {row['id']} not sent: {r.stderr.strip()}", file=sys.stderr)
    return n


def install() -> int:
    exe = os.path.realpath(sys.argv[0])
    plist = f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>/usr/bin/python3</string><string>{exe}</string></array>
  <key>StartInterval</key><integer>60</integer>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>{LOG}</string>
  <key>StandardErrorPath</key><string>{LOG}</string>
</dict></plist>
"""
    os.makedirs(os.path.dirname(PLIST), exist_ok=True)
    with open(PLIST, "w", encoding="utf-8") as f:
        f.write(plist)
    uid = str(os.getuid())
    subprocess.run(["launchctl", "bootout", f"gui/{uid}/{LABEL}"], capture_output=True)
    r = subprocess.run(["launchctl", "bootstrap", f"gui/{uid}", PLIST], capture_output=True, text=True)
    if r.returncode != 0:
        print(f"launchctl bootstrap failed: {r.stderr.strip()}", file=sys.stderr)
        return 1
    print(f"installed {LABEL}: runs every minute, log {LOG}")
    return 0


def uninstall() -> int:
    subprocess.run(["launchctl", "bootout", f"gui/{os.getuid()}/{LABEL}"], capture_output=True)
    if os.path.exists(PLIST):
        os.remove(PLIST)
    print(f"removed {LABEL}")
    return 0


def main() -> int:
    args = sys.argv[1:]
    if args and args[0] in ("-h", "--help"):
        print(__doc__)
        return 0
    if args and args[0] == "--uninstall":
        return uninstall()
    code = sync()
    if args and args[0] == "--install":
        return install() or code
    return code


if __name__ == "__main__":
    sys.exit(main())
