#!/usr/bin/env python3
"""Find frustration signals in Ethan's own messages — deterministically.

Ethan, 2026-08-22: "make a sched that goes thru my messages and infers
frustration for example repetitive messages, frustration etc what ever you
think but make sure it has the ability to autofix amux shit that is identified
from messages."

WHAT THIS DOES AND DELIBERATELY DOES NOT DO
-------------------------------------------
It computes CANDIDATES and ranks them. It does not judge them, and it does not
call a model. Ethos rule 2: repetition is a string comparison and a marker is a
regex, so paying a model to find them would be spending judgment on arithmetic.
The judging — "is this actually frustration, and what amux defect is under it"
— is what the scheduled SESSION does with this output, and that is a real
judgment call worth a model.

Rule 5, the one that decides whether this is useful in six months: it must
DISCRIMINATE, not accumulate. A daily list of forty maybe-frustrated messages
is a log nobody reads. So every signal here is scored, the output is capped,
and a candidate has to clear a bar to appear at all. If a quiet day prints
nothing, that is the correct output and the scheduled session should say so.

THE SIGNALS, strongest first. Each is here because it means something specific:

  repeat-after-done   Ethan asked for something whose card is already done or
                      verified. The strongest signal in the file: it means a
                      fix shipped and did not reach him, which is a defect in
                      amux even when the code was right.
  near-duplicate      He said the same thing twice. Repetition is the clearest
                      frustration signal there is and it needs no interpreting.
  rapid-reprompt      A short imperative within 10 minutes of a previous
                      message to the same lane ("just do it now", "?", "well").
                      The shape of "you did not do what I asked".
  marker              Explicit language: still/again/why is/doesn't work/broken,
                      profanity, "??", "i said". Weakest on its own, which is
                      why it is scored lowest and never appears alone unless
                      it is strong.

WHY `type='user' AND origin=''`
-------------------------------
That is Ethan and nobody else. Peer relays are type='session' with origin set
to the sending lane; scheduler fires are type='schedule' with the schedule
title as origin. Measured 2026-08-22: 509 of his messages in 7 days against
4,061 lane relays, so getting this filter wrong would drown the signal in the
fleet talking to itself. `cmd_history.ts` is in MILLISECONDS.
"""
import json
import os
import difflib
import re
import sqlite3
import sys
import time
from collections import defaultdict

DB = os.environ.get("AMUX_DB") or os.path.expanduser("~/.amux/amux.db")
DAYS = float(os.environ.get("FRUSTRATION_SCAN_DAYS", "3"))
MAX_OUT = int(os.environ.get("FRUSTRATION_SCAN_MAX", "12"))

# A marker is worth little alone and a lot next to a repeat, which is why these
# are additive scores rather than a filter.
MARKERS = [
    (r"\bstill\b", 2, "still"),
    (r"\bagain\b", 2, "again"),
    (r"\bwhy is\b|\bwhy does\b|\bwhy did\b", 2, "why-is"),
    (r"do(?:es)?n'?t work|not working|isn'?t working", 3, "doesnt-work"),
    (r"\bbroken\b|\bbroke\b", 2, "broken"),
    (r"\bi (?:already )?said\b|as i said", 3, "i-said"),
    (r"\balready\b", 1, "already"),
    (r"\?\?+", 2, "double-question"),
    # NOT bare "shit": measured on the real corpus it fires on "the gsuite shit"
    # and "amux shit", which is how Ethan says "stuff". A marker that matches a
    # register rather than a mood is noise dressed as signal.
    (r"\bwtf\b|\bfuck|\bgoddamn|\bffs\b", 3, "profanity"),
    (r"\byou did ?n'?t\b|\byou never\b", 3, "you-didnt"),
    (r"\bnothing happen|\bno response\b|\bsilent\b", 2, "no-response"),
]
# Short imperatives that mean "you did not do it", as opposed to new work.
#
# TWO CLASSES, and conflating them made a CONTINUATION read as a chase.
# `and\?*` matched `^and\b`, so "and send the email with the ns" and "and
# suggest a call this week" both scored as re-prompts — both were Ethan
# finishing one thought in a second message, 8 and 9 seconds after the first,
# which is the opposite of "you did not do it". Measured 2026-08-25: 2 of the
# 3 new candidates that sweep produced, and the reprompt kind's whole meaning
# is "a lane went quiet or a delivery did not land".
#
# `and` and `well` are prods ONLY when they are the entire message ("and?",
# "well?"). Followed by content they introduce more work. The others carry
# their meaning with a tail — "did you push it", "status of the sweep" — so
# they keep the prefix form.
REPROMPT_BARE = re.compile(r"^(and|well|now|go)\?*$", re.I)
REPROMPT_PREFIX = re.compile(
    r"^(just do it|do it|continue|\?+|status\??|"
    r"any update|did you|are you)\b",
    re.I,
)


def is_reprompt(norm):
    """A chase, not a continuation. See REPROMPT_BARE above for why the split."""
    return bool(REPROMPT_BARE.match(norm) or REPROMPT_PREFIX.match(norm))
CONTROL = re.compile(
    r"(?:continue|go ahead|go on|proceed|keep going|yes|yep|ok(?:ay)?|sure|"
    r"do it|just do it|next|more|status|thanks?|ty|nice|good|perfect|done)"
    r"[.!]*",
    re.I,
)
TIME_PREFIX = re.compile(r"^\s*\[\d{1,2}:\d{2}\s*(?:AM|PM)\]\s*", re.I)
ATTACH = re.compile(r"@?/\S*/uploads/\S+")
WS = re.compile(r"\s+")


def normalize(t):
    """Strip what varies between two utterances of the same request.

    The timestamp prefix and an attachment path are noise for equality: the
    same complaint sent twice carries two different clock times and, if he
    re-screenshots, two different upload paths. Leaving either in would make
    every repeat look unique, which is the failure this whole script exists to
    avoid.
    """
    t = TIME_PREFIX.sub("", t or "")
    t = ATTACH.sub("", t)
    return WS.sub(" ", t.lower()).strip()


# ---- PASTED CONTENT IS NOT ETHAN'S WORDS (AF-255) --------------------------
#
# `normalize` strips the timestamp and the attachment path and nothing else, so
# everything Ethan PASTES — a forwarded email, a meeting transcript, a canary's
# output — was scored as though he had written it. Measured on the 2026-08-26
# run: 4 of 9 candidates were this artifact, including the second-highest
# scorer, a customer meeting transcript whose "you didn't" / "no response" /
# "i said" markers were spoken by MEETING PARTICIPANTS. The sweep's own
# instruction says "do not infer a mood and file it"; the scanner was inferring
# one from a third party's words and attributing it to Ethan.
PASTE_HEADER = re.compile(
    r"^\s*(meeting title|meeting participants|meeting date|attendees|from|to|subject|sent)\s*:",
    re.I | re.M,
)
SIG_DELIM = re.compile(r"^\s*--\s*$", re.M)
FENCE = re.compile(r"```.*?```", re.S)
QUOTED_LINE = re.compile(r"^\s*>.*$", re.M)


def own_words(t):
    """What is left after removing what Ethan quoted rather than wrote.

    Conservative on purpose: it removes only regions with an unambiguous paste
    marker. A forwarded body with no marker at all still survives here — that
    case is handled at the PAIR level by `ask_similarity`, which is the right
    layer for it because a quote is only detectable as a quote when it appears
    twice.
    """
    t = FENCE.sub(" ", t or "")
    t = QUOTED_LINE.sub(" ", t)
    # A paste header or a signature delimiter starts a block that runs to the
    # end. Truncate at the EARLIEST of them.
    cut = len(t)
    for rx in (PASTE_HEADER, SIG_DELIM):
        m = rx.search(t)
        if m:
            cut = min(cut, m.start())
    return WS.sub(" ", t[:cut]).strip()


def ask_similarity(a, b):
    """Similarity of the two ASKS, not of what they both quote.

    Two messages that forward the SAME email share hundreds of words, so plain
    jaccard reports ~1.0 while the actual requests differ completely. Removing
    the longest shared run and comparing the remainders separates the two cases
    without needing to recognise any quote format:

      genuinely repeated ask  -> the shared run IS the whole message, both
                                 remainders are empty, and the original
                                 similarity stands.
      different asks, one quote -> the shared run is the quote, the remainders
                                 are Ethan's two different requests, and their
                                 similarity is the honest answer.

    Returns (similarity, shared_word_count).
    """
    aw, bw = a.split(), b.split()
    sm = difflib.SequenceMatcher(a=aw, b=bw, autojunk=False)
    m = sm.find_longest_match(0, len(aw), 0, len(bw))
    if m.size < 20:
        return similarity(a, b), m.size
    rem_a = " ".join(aw[: m.a] + aw[m.a + m.size :]).strip()
    rem_b = " ".join(bw[: m.b] + bw[m.b + m.size :]).strip()
    # Nothing meaningful left on one side = the shared run WAS the message.
    if len(rem_a.split()) < 4 or len(rem_b.split()) < 4:
        return similarity(a, b), m.size
    return similarity(rem_a, rem_b), m.size


def shingles(s, n=4):
    w = s.split()
    return {" ".join(w[i : i + n]) for i in range(max(0, len(w) - n + 1))} or {s}


def similarity(a, b):
    """Jaccard over 4-word shingles. Cheap, and it does not need to be better:
    the output is a CANDIDATE for a model to judge, so a false pair costs one
    line of a session's attention and a missed pair costs a signal we already
    have other routes to."""
    A, B = shingles(a), shingles(b)
    if not A or not B:
        return 0.0
    return len(A & B) / len(A | B)


def main():
    if not os.path.exists(DB):
        print(json.dumps({"error": f"no db at {DB}"}))
        return 2
    cutoff_ms = int((time.time() - DAYS * 86400) * 1000)
    conn = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    rows = conn.execute(
        "SELECT id, ts, COALESCE(session,''), text FROM cmd_history "
        "WHERE type='user' AND COALESCE(origin,'')='' AND ts >= ? "
        "AND COALESCE(text,'') <> '' ORDER BY ts ASC",
        (cutoff_ms,),
    ).fetchall()

    msgs = []
    for mid, ts, sess, text in rows:
        norm = normalize(text)
        # SHORT = NOT A REQUEST, which is a statement about the REPEAT branch and
        # was being applied to both (AF-224). Its own comment says "cannot be a
        # repeat", and `continue` dropped the row from `msgs` entirely, so it
        # never reached the re-prompt branch either.
        #
        # That silently disabled the re-prompt signal for the terse prods it
        # exists to catch. Measured 2026-08-25, normalized lengths: "go" 2,
        # "and" 3, "now" 3, "and?" 4, "well?" 5, "status?" 7 — every one under
        # the gate, and every one a chase. "continue" survives on exactly 8
        # characters, which is why the branch looked like it worked.
        #
        # So the REPROMPT table listed `and`, `well`, `now`, `go` and `status?`
        # as prods while the gate above guaranteed none of them could arrive.
        # The only form those tokens could ever match was the PREFIX one — "and
        # <more work>" — i.e. a continuation, which is the opposite of a chase.
        # They generated false positives exclusively (2 of 3 new candidates in
        # that sweep) and could never generate a true one.
        #
        # Marked control rather than dropped: control rows are excluded from the
        # repeat branch (what the gate wanted) and kept for the re-prompt branch,
        # where the timing carries the meaning.
        if len(norm) < 8:
            msgs.append({"id": mid, "ts": ts, "session": sess, "text": text,
                         "norm": norm, "own": own_words(text), "control": True})
            continue
        # CONTROL WORDS ARE NOT REQUESTS. "continue" appeared six times in the
        # first run as a near-duplicate of itself across three days, which is
        # exactly the accumulate-not-discriminate failure rule 5 warns about:
        # true, worthless, and it would have been top of the list every day
        # forever. They are still eligible for the re-prompt signal, where the
        # timing is what carries the meaning.
        if CONTROL.fullmatch(norm):
            msgs.append({"id": mid, "ts": ts, "session": sess, "text": text,
                         "norm": norm, "own": own_words(text), "control": True})
            continue
        msgs.append({"id": mid, "ts": ts, "session": sess, "text": text,
                     "norm": norm, "own": own_words(text), "control": False})

    findings = defaultdict(lambda: {"score": 0, "why": [], "msgs": []})

    # 1. NEAR-DUPLICATE — the clearest signal, and it needs no interpreting.
    real = [m for m in msgs if not m["control"]]
    for i, a in enumerate(real):
        for b in real[i + 1 :]:
            sim, shared = ask_similarity(a["norm"], b["norm"])
            if sim >= 0.55:
                gap_s = (b["ts"] - a["ts"]) / 1000
                # UNDER A MINUTE IS NOT A HUMAN REPEATING THEMSELVES. Two
                # byte-identical messages seconds apart is amux delivering one
                # message twice — a delivery defect, not a mood — and calling it
                # frustration would send someone to read Ethan's tone when the
                # bug is in send_dedup. Found on the first run: ids 30451/30452,
                # identical, same second, consecutive ids.
                # SAME SESSION OR IT IS NOT A REPEAT, AND THIS GUARD BELONGS
                # TO BOTH BRANCHES. It used to live inside the sub-60s branch
                # below, while the comment there claimed a cross-session pair
                # "is skipped outright rather than falling through to the repeat
                # branch below". It was not: only pairs that were ALSO under a
                # minute and ALSO near-identical were skipped, and everything
                # else fell through exactly as the comment said it would not.
                #
                # Live specimen, 2026-08-24 sweep: id 31240 to `random` and id
                # 31973 to `tubescience`, both "whats the status?", 31.4h apart.
                # Reported as `random` repeating itself with jaccard 1.00 — the
                # top-scoring candidate of the run. Two different lanes each
                # being asked for status is ordinary operation, and the finding
                # cost a sweep slot and produced a verdict of "undecidable".
                #
                # Ethos rule 6: the promise was in the comment and not in the code.
                if a["session"] != b["session"]:
                    continue
                if gap_s < 60 and sim > 0.98:
                    # SAME SESSION OR IT IS NOT A DELIVERY DEFECT. Measured
                    # 2026-08-23: ids 31157/31158 are the SAME text 12s apart to
                    # nissan and autodesk — Ethan fanning one instruction to two
                    # lanes, which is ordinary operation. This branch called it
                    # "a DELIVERY defect, not frustration" and told the reader to
                    # go check send_dedup, which is a confident wrong answer
                    # about a bug that does not exist. A cross-session pair is
                    # also NOT a `repeat` (he did not ask twice, he addressed two
                    # workers), so it is skipped outright rather than falling
                    # through to the repeat branch below.
                    key = f"double-delivery:{a['id']}"
                    f = findings[key]
                    f["score"] = max(f["score"], 8)
                    f["why"].append(
                        f"IDENTICAL messages {gap_s:.0f}s apart (ids {a['id']}/{b['id']}) — "
                        "this is a DELIVERY defect, not frustration: check send_dedup and "
                        "cmd_history.delivery before reading anything into the wording"
                    )
                    f["msgs"] = [a, b]
                    continue
                key = f"repeat:{a['id']}"
                f = findings[key]
                f["score"] = max(f["score"], 6 + int(sim * 4))
                f["why"].append(
                    f"repeated request (jaccard {sim:.2f}, {gap_s/3600:.1f}h apart)"
                )
                f["msgs"] = [a, b]

    # 2. RAPID RE-PROMPT — a short imperative hard after a previous message to
    #    the same lane. New work does not arrive that way; chasing does.
    by_sess = defaultdict(list)
    for m in msgs:
        by_sess[m["session"]].append(m)
    for sess, group in by_sess.items():
        for i in range(1, len(group)):
            prev, cur = group[i - 1], group[i]
            gap_s = (cur["ts"] - prev["ts"]) / 1000
            if gap_s <= 600 and is_reprompt(cur["norm"]) and len(cur["norm"]) < 60:
                key = f"reprompt:{cur['id']}"
                f = findings[key]
                f["score"] = max(f["score"], 5)
                f["why"].append(f"re-prompt {gap_s:.0f}s after the previous message to {sess}")
                f["msgs"] = [prev, cur]

    # 3. MARKERS — additive, never sufficient alone unless strong.
    for m in msgs:
        hits, sc = [], 0
        # AF-255: markers score what ETHAN wrote, never what he pasted. The
        # 2026-08-26 run scored a customer meeting transcript at 13 on
        # "still/again/i-said/already/you-didnt/no-response" — every one of
        # them spoken by a meeting participant.
        for pat, w, name in MARKERS:
            if re.search(pat, m["own"]):
                hits.append(name)
                sc += w
        if not hits:
            continue
        key = None
        for k, f in findings.items():
            if any(x["id"] == m["id"] for x in f["msgs"]):
                key = k
                break
        if key is None:
            if sc < 4:  # a lone weak marker is noise, not a finding
                continue
            key = f"marker:{m['id']}"
            findings[key]["msgs"] = [m]
        findings[key]["score"] += sc
        findings[key]["why"].append("markers: " + ",".join(hits))

    out = []
    for key, f in findings.items():
        first = f["msgs"][0]
        out.append(
            {
                "score": f["score"],
                "kind": key.split(":")[0],
                "session": first["session"],
                "why": sorted(set(f["why"])),
                "messages": [
                    {
                        "id": m["id"],
                        "when": time.strftime("%m-%d %H:%M", time.localtime(m["ts"] / 1000)),
                        "session": m["session"],
                        "text": m["text"][:400],
                    }
                    for m in f["msgs"]
                ],
            }
        )
    out.sort(key=lambda x: -x["score"])
    print(
        json.dumps(
            {
                "window_days": DAYS,
                "ethan_messages_scanned": len(msgs),
                "candidates": len(out),
                "shown": min(len(out), MAX_OUT),
                "findings": out[:MAX_OUT],
                "note": (
                    "CANDIDATES, not verdicts. Judge each one: is there an amux defect "
                    "under it, or was this ordinary iteration? An empty list is a real "
                    "and reportable result."
                ),
            },
            indent=1,
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
