#!/usr/bin/env python3
"""PreToolUse Bash guard: amux email is the amux API ONLY, never Mail.app.

Ethan, 2026-08-13: "make sure amux is never using mail.app — it should
exclusively be using the amux api to send/read, etc. emails."

WHY THIS EXISTS. The amux server's email API (`$AMUX_URL/api/email/*`) is already
Mail.app-free: it uses the Gmail API for connected accounts and REFUSES the
AppleScript path (`applescript_not_ported`, a 501) for anything else. So the
only way a Mail.app email happens on this box is an agent HAND-ROLLING
`osascript -e 'tell application "Mail" ...'` in a Bash command, bypassing the
API. That is bad in three concrete ways, each of which has bitten:
  - Mail.app sends from the machine's DEFAULT account — Ethan's PERSONAL one —
    not the intended `from` (GE-617, 2026-08-13: a customer reply went out under
    his personal address, recorded "Reply sent via Mail.app").
  - It bypasses the amux send-audit ledger (`/api/email/log`), so the send is
    invisible to attribution.
  - A hand-rolled `set content` reply silently sends a BLANK email
    (`feedback_email_reliability`).
The CLAUDE.md policy already forbids this, but a policy an agent can ignore is
not "make sure". This hook is the enforcement: the command is BLOCKED and the
agent is routed to the sanctioned API.

PRECISION. Blocks only a Bash command that BOTH invokes `osascript` AND targets
`application "Mail"` — so the iMessage owner-alert osascript (application
"Messages"), the iTerm2 automation (application "iTerm2"), and a mere grep/echo
that mentions the string (no osascript) are all left alone. Reading mail via
Mail.app is blocked too (any osascript driving Mail.app), because read also has a
sanctioned API path (`/api/email/inbox|search|message`).

Fail-open: any parse/error lets the command through — a guard must never wedge a
tool call (the amux hook rule).
"""
import json
import os
from pathlib import Path
import re
import shlex
import sys


def owner_email() -> str:
    value = os.environ.get("AMUX_OWNER_EMAIL", "").strip()
    if value:
        return value
    env_file = Path(os.environ.get("AMUX_HOME", Path.home() / ".amux")) / "server.env"
    try:
        for raw in env_file.read_text().splitlines():
            line = raw.strip()
            if line.startswith("export "):
                line = line[7:].lstrip()
            if not line.startswith("AMUX_OWNER_EMAIL="):
                continue
            parsed = shlex.split(line.split("=", 1)[1], comments=True)
            return parsed[0].strip() if parsed else ""
    except (OSError, ValueError):
        pass
    return ""


def main() -> None:
    try:
        payload = json.load(sys.stdin)
    except Exception:
        sys.exit(0)  # unparseable -> allow (fail-open)

    if payload.get("tool_name") != "Bash":
        sys.exit(0)

    cmd = ((payload.get("tool_input") or {}).get("command") or "")
    if not cmd:
        sys.exit(0)

    # Require BOTH signals so a bare grep/echo of the phrase, or a Messages /
    # iTerm2 osascript, is not caught — only an osascript that drives Mail.app.
    has_osascript = re.search(r"\bosascript\b", cmd) is not None
    targets_mail = re.search(r"""application\s+["']mail["']""", cmd, re.IGNORECASE) is not None

    if has_osascript and targets_mail:
        configured_owner = owner_email()
        from_example = f',"from":{json.dumps(configured_owner)}' if configured_owner else ""
        owner_hint = "" if configured_owner else " Set AMUX_OWNER_EMAIL before sending."
        sys.stderr.write(
            "BLOCKED — amux email is EXCLUSIVELY the amux email API, NEVER Mail.app "
            "(owner's rule, 2026-08-13).\n"
            "Mail.app sends from the machine's DEFAULT (the owner's personal) account, "
            "bypasses the /api/email/log audit ledger, and a hand-rolled reply can "
            "send a BLANK email. Use the API instead:\n"
            "  SEND:  curl -sk -X POST -H 'Content-Type: application/json' "
            "-H \"X-Amux-Session: $AMUX_SESSION\" \\\n"
            "           -d '{\"to\":\"x@y.z\",\"subject\":\"...\",\"body\":\"...\""
            f"{from_example}}}' \"$AMUX_URL/api/email/send\"\n"
            f"{owner_hint}\n"
            "  REPLY: POST \"$AMUX_URL/api/email/reply\"  {message_id, body, from}\n"
            "  READ:  GET \"$AMUX_URL/api/email/inbox\"  ·  /search?q=  ·  /message/<id>\n"
            "If the account is not a connected Gmail account, CONNECT it "
            "(GET \"$AMUX_URL/api/gmail/auth?account=<email>\") — do NOT fall back to "
            "Mail.app.\n"
            "(mail-app-guard)\n"
        )
        sys.exit(2)  # block

    sys.exit(0)


main()
