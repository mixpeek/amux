#!/usr/bin/env python3
"""A chat helper for the intent e2e server (AMUX_CHAT_CLAUDE_BIN).

Speaks `claude -p --output-format stream-json` like e2e/fixtures/fake-claude-stream.py,
and answers the household's requests from the seeded mail and calendar it
reads through the server's own API (the way a real helper would, with
`amux get`). It uses the primitives the brief names: a reminder is a
one-time schedule, a draft email is a send through the email API (held for
the owner, never released here), a to-do is a card on her board. The persona
runs use the real model; this keeps the specs about amux.
"""
import json, os, re, ssl, sys, time, urllib.error, urllib.request, urllib.parse

PROMPT = sys.stdin.read()
SID = "1f0e0d0c-0b0a-4908-8706-050403020101"
URL = os.environ.get("AMUX_URL", "").rstrip("/")
CTX = ssl.create_default_context(); CTX.check_hostname = False; CTX.verify_mode = ssl.CERT_NONE


def api(path, body=None):
    if not URL:
        return None
    req = urllib.request.Request(URL + path, method="POST" if body is not None else "GET",
                                 data=json.dumps(body).encode() if body is not None else None,
                                 headers={"Content-Type": "application/json", "X-Amux-Session": os.environ.get("AMUX_SESSION", "")})
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=20) as r:
            return json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:  # the server's refusal, with its reason
        try:
            return {"error": "%s %s" % (e.code, e.read().decode()[:300])}
        except Exception:  # noqa: BLE001
            return {"error": str(e)}
    except Exception as e:  # noqa: BLE001 - a fixture reports, never crashes
        return {"error": str(e)}


def mail(q):
    r = api("/api/email/search?" + urllib.parse.urlencode({"q": q, "days": 180, "limit": 20}))
    return r if isinstance(r, list) else []


def events():
    r = api("/api/cal-events")
    r = r if isinstance(r, list) else (r or {}).get("events", []) if isinstance(r, dict) else []
    return sorted(r, key=lambda e: e.get("start", ""))


p = PROMPT.lower()
# The owner's words: the paragraph between the delivery's opening (which
# ends in a colon) and "Nobody pasted this.", never the opening itself, which
# names email, calendar and school in its instructions.
_end = PROMPT.rfind("\n\nNobody pasted this.")
_start = PROMPT.rfind(":\n\n", 0, _end) if _end >= 0 else -1
body = PROMPT[_start + 3:_end] if _start >= 0 else PROMPT
b = body.lower()
status = ""
goal = re.search(r"keep this goal for me: ([^.]*)", b)
goal = goal.group(1) if goal else ""
if "keep this goal" in b or "your last update on this goal" in b:
    nxt = [e for e in events() if e.get("start", "") >= time.strftime("%Y-%m-%d")]
    if "calendar" in goal and nxt:
        e = nxt[0]
        status = "STATUS: Next is %s on %s; what it needs is ready." % (e["title"], e["start"][:10])
    elif "school" in goal:
        status = "STATUS: 3 school forms open; the soonest is Rafi's field trip slip, due Friday."
    elif "dinner" in goal:
        status = "STATUS: This week's dinners are planned around the CSA box; next pickup is Thursday."
    elif "enrollment" in goal:
        status = "STATUS: Open enrollment is not done yet; the FSA election needs a choice."
    else:
        status = "STATUS: Watching for anything new."
    answer = status + "\n\nI'll keep this up to date and only read and draft. Nothing goes out without your OK."
elif "remind me" in b and "camp" in b:
    when = time.strftime("%Y-%m-%dT10:00", time.localtime(time.time() + 86400))
    api("/api/schedules", {"title": "Reminder: call the camp director about Rafi's waitlist", "kind": "tmux",
                           "once": when, "command": "Post the reminder", "session": os.environ.get("AMUX_SESSION", "")})
    thread = mail("camp kaylie rafi")
    answer = ("Done: I'll remind you tomorrow at 10:00 to call Marcy Feld, the Camp Kaylie director, about Rafi's Session 2 waitlist. "
              "Her number is (845) 555-0142.\n\nFrom your email: Rafi was #4 on the waitlist, and your follow-up %d days ago has no answer yet."
              % 12 if thread else "Done: I'll remind you tomorrow at 10:00 to call the camp director.")
elif "remind me" in b and "application" in b:
    when = time.strftime("%Y-%m-%dT19:00", time.localtime(time.time() + 86400))
    api("/api/schedules", {"title": "Reminder: fill in Ezra's school application", "kind": "tmux", "once": when, "command": "Post the reminder", "session": os.environ.get("AMUX_SESSION", "")})
    answer = "Done: I'll remind you tomorrow evening to fill in Ezra's Hudson Prep application."
elif "hudson prep" in b and ("done or missing" in b or "confirmation" in b):
    got = mail("hudson prep application received")
    answer = ("**Verdict: missing.** There is no \"Application received\" email from Hudson Prep. Their reminder says the parent "
              "statement is still missing (the teacher recommendation arrived). The deadline is in 9 days." if not got else "**Verdict: done.**")
elif "nudge" in b and "camp" in b:
    held = api("/api/email/send", {"to": "marcy@campkaylie.test", "subject": "Re: Rafi Levin - Session 2 waitlist",
                                   "body": "Hi Marcy,\n\nI wanted to check in again on Rafi's spot on the Session 2 waitlist. He is still "
                                           "very excited about camp. Is there any news, or anything we can do?\n\nThanks so much,\nDana"})
    answer = ("I wrote a second nudge to Marcy Feld. It is waiting for your OK below." if (held or {}).get("approval_id")
              else "I could not save the draft: %s" % (held or {}).get("error", "no answer"))
elif "add" in b and ("to-do" in b or "todo" in b or "to do" in b):
    what = re.sub(r"^.*?(add|put)\s+", "", body.strip(), flags=re.I).split(" to my")[0].strip().rstrip(".") or body.strip()
    card = api("/api/intent/todos", {"title": what[:80]})
    answer = ("Added to your to-dos: %s." % what) if (card or {}).get("id") else "I could not add it: %s" % json.dumps(card)[:300]
elif "school" in b and "inbox" in b:
    answer = ("I found 4 school items:\n\n1. **Rafi's field trip permission slip** and $15, due Friday (Ms. Ortiz).\n2. **Picture day** order form, due on picture day.\n"
              "3. **Ezra's emergency contact form**, update in the parent portal in 4 days.\n4. **Conference sign-ups** open in 2 days at 7pm.")
elif "dinner" in b:
    answer = ("Dinners this week (Noa is vegetarian, fish is OK):\n\n- Mon: butternut squash soup and grilled cheese\n- Tue: salmon, rice, roasted carrots\n"
              "- Wed: kale and white bean pasta\n- Thu: veggie tacos\n- Fri: shakshuka with CSA eggs\n- Sat: apple crisp night, homemade pizza\n- Sun: leftovers")
elif "conference" in b:
    answer = ("Book in this order when sign-ups open:\n\n1. **Mr. Brandt (Math), Ezra**: C+ with 3 missing assignments.\n2. **Mr. Ruiz (Science), Ezra**: B, lab report not submitted.\n"
              "3. **Ms. Okafor (English), Ezra**: A-.\n4. **Ms. Hale (Social Studies), Ezra**: A.\n\nFor Rafi, ask Ms. Ortiz about reading fluency.")
elif "tova" in b or "388" in b or "rental" in b:
    answer = ("**Car rental reconciliation**\n\n| Item | Amount |\n|---|---|\n| Hertz, Burlington VT (total) | $776.40 |\n| Half each | $388.20 |\n"
              "| Tova paid (Venmo) | $388.00 |\n| Difference | $0.20 |\n\nThe trip was the Vermont ski weekend on your calendar. Tova's payment covers her half less 20 cents; nothing to chase.")
elif "calendar" in b:
    nxt = [e for e in events() if e.get("start", "") >= time.strftime("%Y-%m-%d")][:3]
    answer = "Coming up:\n\n" + "\n".join("- %s, %s" % (e["title"], e["start"][:16].replace("T", " ")) for e in nxt)
elif "bus" in b:
    answer = "I checked: the school bus runs on a delayed schedule on snow days, two hours late."
else:
    answer = "On it. Here is what I found: nothing that needs you right now."


def out(v):
    sys.stdout.write(json.dumps(v) + "\n"); sys.stdout.flush()


out({"type": "system", "subtype": "init", "session_id": SID, "model": "fake-helper"})
out({"type": "stream_event", "event": {"type": "message_start"}})
out({"type": "stream_event", "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}})
for i in range(0, len(answer), 40):
    out({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": answer[i:i + 40]}}})
    time.sleep(0.01)
out({"type": "result", "subtype": "success", "is_error": False, "session_id": SID, "total_cost_usd": 0.0,
     "duration_api_ms": 50, "num_turns": 1, "result": answer, "usage": {"input_tokens": 1, "output_tokens": 1}})
