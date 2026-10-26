#!/usr/bin/env python3
"""14-day self check-in loop. Daily guard, answer-gated. NO model call at fire time.

Why this is a plain script and not a worker prompt (same reasoning as
scripts/daily-philosophy.sh, AMUX-4579): `self` is an isolated worker, so amux
does not deliver tmux prompts into it, and a `kind: shell` schedule wakes no
model turn and spends none of the owner's plan window. The question sets are
curated here rather than composed at fire time, so the recurring send costs
nothing. Folding answers into the wiki and resetting the clock is a model task
that happens when Ethan actually answers, not on the fire.

Modes (argv[1]):
  guard    (default) daily: open a check-in if due, else nudge/escalate an open one
  preview  render the current cycle's file to disk and print it; no text, no state change
  arm      flip armed=true (first live send is gated on the owner's go-ahead)
  answered record that Ethan answered: reset the 14-day clock, advance the cycle

Env knobs for testing: CHECKIN_FORCE_TODAY=YYYY-MM-DD, CHECKIN_DRY_RUN=1,
CHECKIN_DEEPLINK_FMT (e.g. 'amux://file?path={path}') once amux ships the deep link.
AMUX_OWNER_PHONE is injected by the scheduler at fire time, exactly as daily-philosophy relies on.
"""
import json
import os
import subprocess
import sys
import datetime
from pathlib import Path

CHECKIN_DIR = Path("/Users/ethan/Vault/Self/Check-ins")
STATE = CHECKIN_DIR / "state.json"
PHONE = os.environ.get("AMUX_OWNER_PHONE", "").strip()
TODAY = os.environ.get("CHECKIN_FORCE_TODAY") or datetime.date.today().isoformat()
DRY = os.environ.get("CHECKIN_DRY_RUN", "0") == "1"
DEEPLINK_FMT = os.environ.get("CHECKIN_DEEPLINK_FMT", "").strip()

CADENCE_DAYS = 14
NUDGE1_DAY = 3
ESCALATE_DAY = 7
NUDGE2_DAY = 11
ANSWER_BYTES_DELTA = 200  # growth over the blank template that counts as "he started answering"

SPOTLIGHTS = [
    ("Spotlight: the relationship",
     "The mission is a real relationship and kids, soon. In the last two weeks, were you in even one "
     "room of aligned strangers, and did you follow up with anyone real? What is the honest blocker right now?"),
    ("Spotlight: Dan",
     "You have written that one real friendship moves your baseline more than any date. When did you last "
     "call Dan by voice, not text, and say one true thing? If it has been a while, what is in the way?"),
    ("Spotlight: beginner-ness and white space",
     "You lost a physical goal at the scale of the old 63-parks project and never replaced it, and the "
     "Wednesday white space keeps getting eaten. What did you start that you are visibly bad at, and did "
     "you defend any empty time this cycle?"),
    ("Spotlight: the loop",
     "Where did the simulation run this cycle (body tension plus a worst-case future)? Did you catch it and "
     "ask what is actually wrong right now? What is the loop reaching for lately?"),
    ("Spotlight: the optimization reflex",
     "The fix-and-optimize reflex pays off in business and drains you in health and appearance. What did you "
     "optimize this cycle that genuinely mattered, and what did you over-research instead of acting on?"),
    ("Spotlight: the van version of you",
     "The real work is bringing the van version of you, relaxed and unarmored, into ordinary life. Where were "
     "you unarmored with a person in the last two weeks? Where did you armor up when you did not need to?"),
]


def dt(s):
    return datetime.date.fromisoformat(s)


def days_between(a, b):
    return (dt(b) - dt(a)).days


def load_state():
    return json.loads(STATE.read_text())


def save_state(st):
    tmp = CHECKIN_DIR / "state.json.tmp"
    tmp.write_text(json.dumps(st, indent=2) + "\n")
    os.replace(tmp, STATE)  # atomic within the same directory


def deeplink(path):
    return DEEPLINK_FMT.format(path=path) if DEEPLINK_FMT else ""


def render_file(cycle, date, spot_idx, prev_file):
    spot_name, spot_prompt = SPOTLIGHTS[spot_idx]
    prev = f"[[{Path(prev_file).stem}]]" if prev_file else "first cycle"
    return f"""---
type: self-check-in
cycle: {cycle}
opened: {date}
answered:
spotlight: {spot_name}
---

# Self Check-in, {date} (cycle {cycle})

Fortnightly. Answer in place under each question, in this file. Rough and honest beats polished. Blank is a legitimate answer. When you save with answers, the clock resets and I fold the signal into the wiki.

Previous: {prev}

---

## 1. Scoreboard
Since the last check-in, what actually moved? One line each.
- Work (say which of Mixpeek / amux / the exit actually got your hours):
- Body:
- A real relationship (romantic, or Dan):

## 2. The pattern that ran you
Which of your five patterns ran you in the last two weeks: manufactured friction when things were good, a performance reflex, a withdrawal, or a rumination loop you fed? Name the specific moment, not the category.
>

## 3. The avoided conversation
What is the one hard conversation or decision you are still dodging, where being wrong actually costs something? What would get you to have it before the next check-in?
>

## 4. Subtraction
Mixpeek, amux, and the big-tech exit are all still open. In the last two weeks did you cut anything, or only add? What does keeping all three open for another cycle actually cost you?
>

## 5. The one uncomfortable thing
Name the single most uncomfortable thing you could do before the next check-in that future-you would thank you for. One thing, not a list. You are choosing it.
>

## 6. {spot_name}
{spot_prompt}
>

---

**State of me right now (one honest line):**
>

**One promise I am keeping to myself this cycle (nobody is watching):**
>
"""


def send_imessage(msg):
    if DRY or not PHONE:
        print(f"self-checkin: [dry/no-phone] would text: {msg!r}")
        return True
    r = subprocess.run(
        ["osascript",
         "-e", "on run {msg, ph}",
         "-e", 'tell application "Messages"',
         "-e", "set s to first service whose service type = iMessage",
         "-e", "set b to buddy ph of s",
         "-e", "send msg to b",
         "-e", "end tell",
         "-e", "end run",
         "--", msg, PHONE],
        capture_output=True, text=True)
    if r.returncode != 0:
        print(f"self-checkin: iMessage send FAILED: {r.stderr[:300]}", file=sys.stderr)
        return False
    return True


def open_text(cycle, path):
    link = deeplink(str(path))
    tail = f"\nOpen: {link}" if link else (
        f"\nOpen Self/Check-ins/{Path(path).name} in the vault, or answer in the self Chat tab.")
    return (f"14-day check-in, cycle {cycle}. Six questions, about ten minutes. Rough and honest beats polished. "
            f"The clock does not reset until you actually answer.{tail}")


def nudge_text(cycle, path):
    link = deeplink(str(path))
    tail = f"\n{link}" if link else f"\nSelf/Check-ins/{Path(path).name}, or the self Chat tab."
    return f"Still open: your check-in from {Path(path).stem} (cycle {cycle}). Two minutes of honesty moves this.{tail}"


def escalate_text(cycle, path):
    link = deeplink(str(path))
    tail = f"\n{link}" if link else f"\nSelf/Check-ins/{Path(path).name}"
    return (f"Day 7 on an unanswered check-in (cycle {cycle}). This is the loop you set up to force yourself. "
            f"If the questions are wrong, say so in the self Chat tab and I will change them.{tail}")


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "guard"
    st = load_state()

    if mode == "preview":
        cyc = st.get("cycle", 1)
        spot = (cyc - 1) % len(SPOTLIGHTS)
        path = CHECKIN_DIR / f"{TODAY}.md"
        path.write_text(render_file(cyc, TODAY, spot, st.get("last_file")))
        print(f"wrote {path}\n")
        print(path.read_text())
        return 0

    if mode == "arm":
        st["armed"] = True
        save_state(st)
        print("self-checkin: armed")
        return 0

    if mode == "start":
        # Go live now: arm and open the current cycle today, bypassing the 14-day
        # gate (that gate gets its first anchor from this answer). Run on the
        # owner's go-ahead after he has reviewed the first file.
        st["armed"] = True
        if st.get("open"):
            print("self-checkin: a check-in is already open; not starting another")
            save_state(st)
            return 0
        cyc = st.get("cycle", 1)
        spot = (cyc - 1) % len(SPOTLIGHTS)
        path = CHECKIN_DIR / f"{TODAY}.md"
        if not path.exists():
            path.write_text(render_file(cyc, TODAY, spot, st.get("last_file")))
        baseline = path.stat().st_size
        ok = send_imessage(open_text(cyc, str(path)))
        st["open"] = {"file": path.name, "opened": TODAY, "cycle": cyc, "spotlight": spot,
                      "baseline_bytes": baseline, "nudges_sent": 0, "escalated": False,
                      "answered_detected": False}
        save_state(st)
        print(f"self-checkin: started cycle {cyc} ({'texted' if ok else 'TEXT FAILED'})")
        return 0

    if mode == "answered":
        op = st.get("open")
        if op:
            st["last_file"] = op["file"]
        st["last_answered"] = TODAY
        st["open"] = None
        st["cycle"] = st.get("cycle", 1) + 1
        save_state(st)
        print(f"self-checkin: reset; next cycle {st['cycle']} due in {CADENCE_DAYS} days")
        return 0

    # guard (default)
    if not st.get("armed"):
        print("self-checkin: disarmed; awaiting owner go-ahead")
        return 0

    op = st.get("open")
    if op:
        f = CHECKIN_DIR / op["file"]
        cur = f.stat().st_size if f.exists() else 0
        if not op.get("answered_detected") and cur - op.get("baseline_bytes", 0) > ANSWER_BYTES_DELTA:
            op["answered_detected"] = True
            save_state(st)
            print("self-checkin: answers detected in file; going quiet, awaiting fold + reset")
            return 0
        if op.get("answered_detected"):
            print("self-checkin: open check-in has answers; awaiting fold + reset")
            return 0
        n = days_between(op["opened"], TODAY)
        sent = False
        if n >= ESCALATE_DAY and not op.get("escalated"):
            sent = send_imessage(escalate_text(op["cycle"], f))
            op["escalated"] = True
        elif n >= NUDGE2_DAY and op.get("nudges_sent", 0) < 2:
            sent = send_imessage(nudge_text(op["cycle"], f))
            op["nudges_sent"] = 2
        elif n >= NUDGE1_DAY and op.get("nudges_sent", 0) < 1:
            sent = send_imessage(nudge_text(op["cycle"], f))
            op["nudges_sent"] = 1
        elif n > NUDGE2_DAY and (n - NUDGE2_DAY) % 4 == 0:
            sent = send_imessage(nudge_text(op["cycle"], f))
        save_state(st)
        print(f"self-checkin: open cycle {op['cycle']}, day {n}, {'texted' if sent else 'no action today'}")
        return 0

    # no open check-in: is one due?
    base = st.get("last_answered") or st.get("created") or TODAY
    n = days_between(base, TODAY)
    if n < CADENCE_DAYS:
        print(f"self-checkin: next check-in in {CADENCE_DAYS - n} day(s)")
        return 0

    cyc = st.get("cycle", 1)
    spot = (cyc - 1) % len(SPOTLIGHTS)
    path = CHECKIN_DIR / f"{TODAY}.md"
    if not path.exists():
        path.write_text(render_file(cyc, TODAY, spot, st.get("last_file")))
    baseline = path.stat().st_size
    ok = send_imessage(open_text(cyc, str(path)))
    st["open"] = {"file": path.name, "opened": TODAY, "cycle": cyc, "spotlight": spot,
                  "baseline_bytes": baseline, "nudges_sent": 0, "escalated": False,
                  "answered_detected": False}
    save_state(st)
    print(f"self-checkin: opened cycle {cyc} ({'texted' if ok else 'TEXT FAILED'})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
