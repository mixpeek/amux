#!/usr/bin/env python3
"""Fleet-wide THEMATIC friction scan, across amux AND Mixpeek, deterministically.

Ethan, 2026-08-29 (MSG-35488): "look for opportunities that are thematic such
that we can bake them into our system prompt ... look for areas where we've
historically continually hit roadblocks or repeated snags. It might not be the
exact same problem every time but a thematic problem, like we always forget to
add permissions to the right place or we always forget to do integration tests."
Then, 2026-08-30: "it should span not just amux though but also Mixpeek."

WHAT THIS IS, AND HOW IT DIFFERS FROM ITS TWO NEIGHBOURS
--------------------------------------------------------
`frustration_scan.py` asks, of ONE message: is Ethan frustrated here, and is
there an amux defect under it. The unit is a message and the fix is a defect.

This script asks, of MANY days and BOTH repos: which CLASS of friction keeps
coming back, and which PROMPT, RULE or MECHANISM should absorb it so it stops.
The unit is a class and the fix is usually a sentence in a CLAUDE.md, a hook, or
a gate, rather than a code change. A class that appears in both repos is harness-level and
belongs in the global prompt; a class in one repo belongs in that repo's file.
That split IS the answer to "should this go in the system prompt", so the script
computes it rather than leaving the session to eyeball it.

It computes signals. It does not name themes and it does not call a model
(ethos rule 2). Naming what a cluster MEANS, and deciding which file absorbs it,
is the judgment the scheduled session is for.

RULE 5: IT MUST DISCRIMINATE, NOT ACCUMULATE
---------------------------------------------
A daily report of every signal is a log nobody reads, and this file would then
be the thing it is complaining about. So every signal is compared against its
own trailing baseline and only ACTIVE ones print: a signal is active when it is
materially above its own trailing rate, or when it is a standing structural
count over its threshold. A quiet day printing three lines is the correct
output.

RULE 4: SAY WHETHER THE PROBE RAN
----------------------------------
Every signal carries `measured` and `n_considered`. A source that could not be
read (mixpeek checkout absent, DB locked) reports measured=false with a reason,
never a zero. A zero from an unread source is the exact failure this fleet has
41 frustration entries about.

SOURCES
-------
  cmd_history (SQLite)   every lane's messages, both repos' lanes, one table
  issues      (SQLite)   the shared board; prefix -> repo mapping below
  frustrations.md        amux ledger, field-per-line format
  mixpeek/FRUSTRATIONS.md  Mixpeek ledger, checkbox format (different parser)
  the CLAUDE.md files    to answer "is this rule already written down?"

`cmd_history.ts` is in MILLISECONDS. `type='user' AND origin=''` is Ethan and
nobody else (peer relays are type='session' with origin set; scheduler fires are
type='schedule').
"""
import json
import os
import re
import sqlite3
import subprocess
import sys
import time
from collections import Counter, defaultdict

DB = os.environ.get("AMUX_DB") or os.path.expanduser("~/.amux/amux.db")
AMUX_REPO = os.environ.get("AMUX_REPO") or os.path.expanduser("~/Dev/amux")
MIXPEEK_REPO = os.environ.get("MIXPEEK_REPO") or os.path.expanduser("~/Dev/mixpeek")
DAYS = float(os.environ.get("FRICTION_DAYS", "1"))
BASELINE_DAYS = float(os.environ.get("FRICTION_BASELINE_DAYS", "14"))
MAX_PER_SIGNAL = int(os.environ.get("FRICTION_MAX_EVIDENCE", "6"))

# Board id prefix -> which repo's fix sites that card's friction points at.
# Unlisted prefixes are reported under 'other' rather than silently dropped:
# a prefix this table does not know is a REPORT, not a zero.
PREFIX_REPO = {
    "AMUX": "amux", "AF": "amux", "AC": "amux", "AG": "amux", "AR": "amux",
    "ETHAN": "amux",
    "BACKE": "mixpeek", "MI": "mixpeek", "TUBES": "mixpeek", "TG": "mixpeek",
    "MF": "mixpeek", "MG": "mixpeek", "MC": "mixpeek", "MHC": "mixpeek",
    "MR": "mixpeek", "MS": "mixpeek", "MO": "mixpeek", "MA": "mixpeek",
    "SP": "mixpeek", "SM": "mixpeek", "GE": "mixpeek", "GS": "mixpeek",
    "GT": "mixpeek", "GCA": "mixpeek", "CO": "mixpeek", "CR": "mixpeek",
    "BR": "mixpeek", "TE": "mixpeek", "LUCIH": "mixpeek", "ADM": "mixpeek",
    "AUTOD": "mixpeek", "PRIME": "mixpeek", "RH": "other", "MVS": "mixpeek",
}

# Lanes whose name says which repo they work in. Used to attribute a message
# theme to a repo when the message itself does not name one.
def lane_repo(session: str) -> str:
    s = (session or "").lower()
    if s.startswith("amux"):
        return "amux"
    if s.startswith("mixpeek") or s in {
        "backend", "ts-gke", "tubescience", "mvs-infra", "mvs-research",
        "studio-plg", "gtm-engine", "gtm-ticker", "cold-outbound", "primer",
        "primis", "nissan", "autodesk", "radio-canada", "creative-dna",
        "ai-video-editor", "general-canvas-apps", "social-media", "paid-social",
        "gtm-videos", "gtm-playbooks", "gtm-media-assets", "launch-videos",
    }:
        return "mixpeek"
    return "other"


# ---------------------------------------------------------------------------
# RULE CLASSES: the heart of the "bake it into the system prompt" question.
#
# Each class is a standing rule the fleet is supposed to already follow. The
# signal is not "Ethan said this once", it is "Ethan had to say it AGAIN, and
# the rule is ALREADY WRITTEN DOWN". A restated rule that is already in prose is
# evidence the prose is not the enforcement point, which is the single pattern
# under 9 of the 10 themes in the 2026-08-29 review.
#
# `doc_probe` is what we grep the CLAUDE.md files for to decide "already
# written". If a class is restated and the probe MISSES, the fix is cheap: write
# it. If it is restated and the probe HITS, prose already failed and the fix is a
# mechanism. The two verdicts want completely different work, which is why this
# is computed and not guessed.
# ---------------------------------------------------------------------------
RULE_CLASSES = [
    # `e2e` MUST BE ADJACENT TO A VERB, never bare (AF-450).
    #
    # The bare `\be2e\b` arm counted Ethan's own harness prompt NAMES as
    # restatements of the verification rule: "ISOLATED-E2E-20260903",
    # "September 3 Gemini E2E workflow", "E2E-Q-20260903-CLAUDE". Measured
    # 2026-09-03 — 9 of the signal's 18 window hits fired on that arm ALONE, and
    # every one of the nine was a task label rather than a demand. It reported
    # 4.2x against baseline where the honest figure is 2.8x.
    #
    # The theme this feeds ("Verification is something Ethan has to demand,
    # every single time") is one of the highest-value entries in the ledger, so
    # the noise landed on the signal least able to absorb it.
    #
    # NOT dropped, because "did you run e2e?" is a real restatement that no
    # other arm catches and `testing`'s pattern needs the word "tests" after it.
    # Requiring a verb within 24 non-sentence characters keeps all five demand
    # forms and drops all four labels.
    ("verification", r"\bverif(?:y|ied|ication)\b|\bactually (?:works|live|shipped)\b|\bprove it\b|\bin prod\b|"
                     r"\b(?:run|ran|rerun|re-run|do|did|does|pass(?:ed|es)?|no|any)\b[^.\n]{0,24}\be2e\b",
     r"verified|VERIFY\.md|verification"),
    ("evidence", r"\bevidence\b|\bshow me the\b|\bproof\b|\bpaste the\b|\bwhat did you run\b",
     r"evidence|--evidence"),
    ("testing", r"\b(?:integration|unit|e2e|regression) tests?\b|\badd a test\b|\bno tests?\b|\btest it\b",
     r"tests?|scripts/test-|cargo test"),
    ("permissions", r"\bpermission|\ballowlist|\bdenied\b|\bauth(?:z|orization)\b|\bcredential|\bapi key\b|\baccess\b",
     r"permission|credential|settings\.json"),
    ("deploy-live", r"\bdeploy(?:ed)?\b|\bship(?:ped)?\b|\bis it live\b|\bpush(?:ed)? to (?:main|prod)\b|\brollout\b",
     r"deploy|/health|builder"),
    # ATTRIBUTION MEANS THE SESSION/GIT SENSE, NOT THE MARKETING ONE (AF-392).
    # The bare `\battribut` and `\borigin\b` arms scored 3 of 3 hits on
    # 2026-09-01 that had nothing to do with who edited a file: "measurement
    # attribution" for GTM playbooks, "attribute meta-properties to each email",
    # and a deep-dive asking for "proper attribution" of worker schedules. n=3
    # against a 0.15/day baseline read as a 20x spike and was entirely noise,
    # which is the dangerous direction: a signal that manufactures an alarm.
    #
    # So `attribut` now has to co-occur with a word from the class it belongs to,
    # and the specific arms carry the rest. Measured against those three messages
    # and four constructed true positives: the false positives all drop, and TWO
    # true positives the old pattern MISSED now match (`X-Amux-Session` and
    # `misattributed`). Tightening it made it more sensitive, not less.
    ("attribution",
     r"(?s)(?:(?=.*\b(?:session|lane|worker|commit|author|git|blame|provenance)\b)\battribut)"
     r"|\bmisattribut|\bwho (?:did|wrote|sent|edited|committed)\b|\bwhich session\b"
     r"|\bX-Amux-Session\b|\bblame(?:d|s)?\b|\bprovenance\b|\bwrong (?:owner|author|session|lane)\b"
     r"|\borigin[- ]stamp",
     r"attribut|X-Amux-Session|origin"),
    ("autonomy", r"\bdon'?t ask\b|\byou have my authority\b|\bjust do it\b|\bstop asking\b|\bdo whatever you think\b|\byou don'?t need me\b",
     r"authority|act, then report|standing authority"),
    ("scope-decompose", r"\bone card\b|\bsplit\b|\bdecompose\b|\bseparate (?:card|issue|task)s?\b|\bumbrella\b",
     r"decompose|one card|per unit of work"),
    ("formatting", r"\bformat(?:ting)?\b|\bunreadable\b|\bwall of text\b|\bblank line\b|\bem.?dash\b|\bmarkdown\b",
     r"blank line|em-dash|formatting"),
    ("staleness", r"\bstale\b|\bout of date\b|\bold (?:data|number|card)\b|\bstill (?:says|shows)\b|\bnot updated\b",
     r"stale|freshness"),
    ("idle-stall", r"\bidle\b|\bstuck\b|\bwhy (?:did|are) (?:you|they) stop\b|\bkeep going\b|\bcontinue\b|\bnot moving\b",
     r"idle|nudge|board_drive"),
    ("backlog-growth", r"\bbacklog\b|\btoo many (?:cards|issues|todos)\b|\binfinite\b|\bgrowing\b|\bnever (?:closes|finishes)\b",
     r"WIP limit|backlog|forced disposition"),
    ("duplicate-work", r"\balready (?:did|done|fixed|exists)\b|\bduplicate\b|\bsame (?:thing|fix) (?:twice|again)\b|\bre.?doing\b",
     r"duplicate|dedupe|already"),
    ("instrument-lies", r"\bwrong (?:number|count|answer)\b|\blied?\b|\bsaid it (?:worked|was done)\b|\bfalse\b|\bmisleading\b",
     r"measured|n_considered|instrument"),
]

# Ledger AREA normalisation. Both repos name areas differently; a theme that
# spans repos is invisible unless the two vocabularies are mapped onto one.
AREA_CANON = [
    (r"attribut|origin|blame|provenance", "attribution"),
    (r"instrument|measur|probe|metric|observab|log", "instruments"),
    (r"gate|status|board|card|wip|verif", "board-gates"),
    (r"notice|nudge|message|deliver|send|relay", "messaging"),
    (r"cli|command|flag|script", "cli"),
    (r"deploy|build|ci|cd|pipeline|release", "deploy-ci"),
    (r"auth|permission|credential|secret|key", "auth-secrets"),
    (r"studio|ui|dashboard|spa|frontend|canvas", "ui"),
    (r"api|endpoint|route|controller", "api-contract"),
    (r"engine|extractor|retriev|cluster|index|pipeline", "engine"),
    (r"infra|k8s|gke|gcs|redis|shard|capacity|storage", "infra"),
    (r"\bdx\b|developer experience|dev tooling|local dev|bootstrap|setup|onboard", "dev-experience"),
    (r"\bprocess\b|workflow|convention|handoff", "process"),
    (r"doc|prompt|claude\.md|rule", "docs-prompt"),
    (r"test|fixture|harness", "testing"),
    (r"browser|chrome|playwright", "browser"),
    (r"schedul|cron|tick", "scheduler"),
    (r"cloud|tunnel|gateway", "cloud"),
]


# Ethan's routine drive commands. These are how he runs the fleet, not
# evidence that a rule failed. Counting "continue" as a restatement of the
# idle-stall rule put 11 hits on that class in one day, 7 of which were the
# single broadcast he sends every evening. A signal that fires on normal
# operation is a signal that cannot discriminate.
BARE_DRIVE = re.compile(
    r"^\s*(?:\[\d\d?:\d\d\s*[AP]M\]\s*)?"
    r"(?:continue|cont|go|go on|keep going|proceed|next|resume|more|ok(?:ay)?|"
    r"yes|yep|y|do it|just do it|\?+|\.+|k)\s*[.!?]*\s*$",
    re.I,
)


def is_bare_drive(text: str) -> bool:
    t = (text or "").strip()
    return len(t) < 25 or bool(BARE_DRIVE.match(t))


# Ethan routinely pastes a document, a transcript or a peer's answer BELOW his
# actual instruction. Matching a rule class or a repeat-phrase against that
# pasted body attributes someone else's words to him: it scored "permissions"
# on a message whose instruction was "clear the backlog", because the words sat
# in a 41 KB meeting transcript underneath (MSG-35336).
#
# The boundary is the first blank line, applied ONLY to long messages. Checked
# against every long message in the window it was written from:
#
#   MSG-35336  41 KB  instruction, blank line, then a meeting transcript
#   MSG-35490  22 KB  instruction, blank line, then a peer's pasted answer
#   MSG-35301  1.8 KB instruction, blank line, then a customer's question list
#   MSG-35488  612 B  a real TWO-PARAGRAPH instruction, no paste at all
#
# That last one is why the rule is length-gated rather than always-first-
# paragraph: his genuine instructions do span blank lines, and truncating them
# would drop half of what he asked for. A long message with no blank line at
# all falls back to the character cap, which under-reads a long instruction and
# never invents one, which is the safe direction for a signal that files work.
LONG_MSG_CHARS = int(os.environ.get("FRICTION_LONG_MSG_CHARS", "1200"))


# Text AMUX APPENDS to a human's message. Must be removed before any phrase
# comparison, or the harness's own words count as the human repeating himself.
#
# Measured 2026-09-04: `cross-lane-repeat` read n=14, and 4 of the 14 were this
# footer. Two of them, MSG-40511 and MSG-42067, share 194 identical characters
# and NOTHING ELSE: one asks to add public datasets to a table, the other asks
# for MVS throughput metrics. The signal reported them as the same instruction
# sent to two lanes.
#
# It matters more than 29% suggests, because `cross-lane-repeat` is the only
# evidence under the theme "Ethan is the fleet's status poller", whose entire
# claim is that he has to repeat himself. An instrument that counts amux's own
# text as his repetition is arguing the theme from the harness's voice.
AMUX_APPENDED = re.compile(r"\n*\[amux: ", re.I)


def strip_amux_appended(text: str) -> str:
    """The human's own words, with anything amux added removed."""
    return AMUX_APPENDED.split(text or "", 1)[0]


def instruction_of(text: str) -> str:
    t = strip_amux_appended(text)
    if len(t) <= LONG_MSG_CHARS:
        return t
    return t.split("\n\n", 1)[0][:LONG_MSG_CHARS]


def canon_area(text: str) -> str:
    t = (text or "").lower()
    for pat, name in AREA_CANON:
        if re.search(pat, t):
            return name
    return "unclassified"


# A success report contradicted by an empty result. Deliberately NOT part of
# AREA_CANON (AF-394).
#
# THE DEFECT. AREA_CANON is first-match-wins over the whole title, and a Mixpeek
# title leads with its subsystem, so "Engine/batches (a batch reports COMPLETED,
# 100%, failed_objects [] and writes ZERO documents)" is `engine`. The theme this
# belongs to is "Instruments that lie", whose signal is
# `ledger-cluster:instruments`, and that signal read QUIET on 2026-09-01 while
# three fresh instances of the class sat in the same scan output.
#
# MEASURED, and it corrects the card that filed this: reordering AREA_CANON would
# have fixed NOTHING. 16 open entries across both ledgers describe this shape and
# only 3 of them contain any word from the instruments arm
# (instrument|measur|probe|metric|observab|log). The failure is vocabulary, not
# ordering. Those 16 are scattered over NINE clusters: api-contract, deploy-ci,
# engine, ui, board-gates, cli, instruments.
#
# WHY THIS IS AN EXTRA LABEL AND NOT A REORDER OR A FULL MULTI-LABEL CANON. Both
# alternatives were measured before being rejected. Reordering steals entries from
# the subsystem clusters, which moves the trailing baselines the sweep compares
# against. Letting every AREA_CANON arm apply independently is worse: the patterns
# were written for first-match-wins and are far too loose to stand alone. Measured
# over the same 1131 entries it gives 2.08 labels each, with `doc` matching every
# entry that says "documents" (8 -> 156) and `index|cluster|pipeline` inflating
# engine 134 -> 472. So this adds ONE precise membership and takes nothing away:
# instruments 103 -> 116, every other cluster unchanged.
# WIDENED 2026-09-04, and the widening is the sweep's own finding. The arms above
# were written from three specimens that all said "reports COMPLETED", so the
# membership matched the WORDING of those three rather than the class. Measured
# over the same 1206 open entries: the shipped arms caught 32, and 63 more open
# entries describe the identical shape in different words. Sampled at 4 per arm,
# every one is the class: "documents/list through the SHARED host returns a silent
# 200-empty", "`objects/batch` silently drops any blob whose URL its fetcher cannot
# reach", "setting an App `is_active: false` does NOT take it offline",
# "`post_filters` is typed, documented, autocompleted, and never applied".
#
# THE BASELINE MOVES AND THE NEXT RUN MUST NOT READ IT AS A SURGE. instruments
# goes 32 -> 95 memberships on the same corpus, with no entry leaving its
# subsystem cluster. That is an instrument change, not new friction, and it is
# recorded in docs/friction-themes.md against the theme it feeds.
#
# EACH ARM IS ITS OWN PHRASE, not a general "success" pattern. A bare \bok\b or
# \bgreen\b was measured and rejected: they match ordinary prose and would let
# this membership absorb the ledger, which is the failure the negative cells in
# scripts/test-friction-themes.sh exist to catch.
GREEN_BUT_EMPTY = re.compile(
    r"reports? (?:completed|success|green|ok)\b"
    r"|completes? green|complet\w* green"
    r"|(?:wrote|writes|written|produces?|produced) (?:zero|no) (?:documents|rows|results|files|bytes|objects)"
    r"|zero (?:bytes|documents|rows|results|files|objects)"
    r"|(?:reported|reports|says) (?:it )?(?:worked|succeeded|done)\b"
    r"|silent (?:write )?loss"
    r"|nothing (?:says so|can search)"
    # a 2xx carrying nothing, said the several ways this fleet says it
    r"|silent(?:ly)? (?:200|202)|(?:200|202)[- ]empty"
    r"|HTTP 200 (?:with|and) (?:zero|no|an empty)"
    # a success status contradicted in the same sentence, EITHER SIDE of it. The
    # first version only looked forward and missed "is cancelled, and reports
    # HTTP 200 / status: completed", where the contradiction is stated first;
    # the test cell caught that on its own specimen.
    r"|status:\s*(?:completed|success|ok)\b[^.\n]{0,80}"
    r"(?:cancel|fail|never|still running|zero|empty)"
    r"|(?:cancel\w*|fail\w*|times? out|timed out|never \w+)[^.\n]{0,80}"
    r"(?:reports?|returns?)[^.\n]{0,30}(?:200|202|status:\s*(?:completed|success|ok))"
    # the operation discarding work while answering normally
    r"|silently (?:drops?|discards?|ignores?|skips?|reversible|overwrit\w+|swallow\w*)"
    # a control that answers and does not act
    r"|does NOT (?:take|apply|remove|delete|disable|stop)\b"
    r"|plumbed[^.\n]{0,60}(?:no[- ]op|inert|nothing|never)"
    r"|documented[^.\n]{0,40}and (?:inert|ignored|never)"
    # the defining clause of the class, stated directly
    r"|and (?:nothing|no one|nobody) (?:anywhere )?(?:says|warns|reports|notices)",
    re.I,
)


def extra_areas(text: str) -> list:
    """Cross-cutting memberships an entry carries BESIDES its subsystem area.

    Returns at most one today. Kept as a list because the next cross-cutting
    class (a gate with no honest exit, say) belongs here rather than in
    AREA_CANON, for the same reason this one does: it is a property of the
    FAILURE, and AREA_CANON answers a question about the SUBSYSTEM.
    """
    return ["instruments"] if GREEN_BUT_EMPTY.search(text or "") else []


class Signal:
    """One measured signal. `measured` and `n_considered` are not optional.

    A signal that could not be computed says so and says why; it never
    degrades into a zero, because a zero and an unread source look identical
    downstream and that confusion is 41 of this fleet's 83 ledger entries.
    """

    def __init__(self, key, repo_scope, headline, measured=True,
                 n_considered=0, why_unmeasured=None, baseline_label="/day"):
        self.key = key
        self.repo_scope = repo_scope      # amux | mixpeek | both | other
        self.headline = headline
        self.measured = measured
        self.n_considered = n_considered
        self.why_unmeasured = why_unmeasured
        self.value = 0
        self.baseline = None              # trailing comparison, if computable
        # What the baseline number MEANS. A rate and a count print identically
        # and read completely differently; "192/day" against a 265-card pile is
        # a nonsense number a reader would have to go and disprove.
        self.baseline_label = baseline_label
        self.active = False
        self.evidence = []
        self.detail = {}

    def to_dict(self):
        return {
            "key": self.key,
            "repo_scope": self.repo_scope,
            "headline": self.headline,
            "measured": self.measured,
            "n_considered": self.n_considered,
            "why_unmeasured": self.why_unmeasured,
            "value": self.value,
            "baseline": self.baseline,
            "baseline_means": self.baseline_label,
            "active": self.active,
            "evidence": self.evidence[:MAX_PER_SIGNAL],
            "detail": self.detail,
        }


def db_connect():
    try:
        con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True, timeout=10)
        con.row_factory = sqlite3.Row
        return con, None
    except Exception as e:  # pragma: no cover - environment failure
        return None, f"{type(e).__name__}: {e}"


def ts_unit_warning(con):
    """Re-check the two timestamp units this file depends on, every run.

    `cmd_history.ts` is milliseconds and `issues.created`/`closed_at` are
    seconds. Every window here is computed from that, so if a future write
    lands in the other unit the whole report goes wrong QUIETLY: cards read as
    56,000 days old and every lane reads as having closed nothing, both of
    which look exactly like findings. Ethos rule 4: say what should appear
    beside the answer, and check for THAT.
    """
    if con is None:
        return None
    ms_cut = 10_000_000_000  # a seconds timestamp is below this until y2286
    bad = []
    row = con.execute(
        "SELECT SUM(ts < ?) a, COUNT(*) n FROM cmd_history", (ms_cut,)).fetchone()
    if row["n"] and row["a"]:
        bad.append(f"cmd_history.ts: {row['a']}/{row['n']} rows look like SECONDS")
    row = con.execute(
        "SELECT SUM(created > ?) a, COUNT(*) n FROM issues", (ms_cut,)).fetchone()
    if row["n"] and row["a"]:
        bad.append(f"issues.created: {row['a']}/{row['n']} rows look like MILLISECONDS")
    row = con.execute(
        "SELECT SUM(closed_at > ?) a, COUNT(*) n FROM issues "
        "WHERE closed_at IS NOT NULL", (ms_cut,)).fetchone()
    if row["n"] and row["a"]:
        bad.append(f"issues.closed_at: {row['a']}/{row['n']} rows look like MILLISECONDS")
    return "; ".join(bad) if bad else None


def read_text(path):
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            return fh.read(), None
    except Exception as e:
        return None, f"{type(e).__name__}: {e}"


# ---------------------------------------------------------------------------
# Signal 1: rule restatement. Ethan re-dictating a rule that already exists
# ---------------------------------------------------------------------------
def signal_rule_restatement(con, now_ms, prompts_doc):
    win_ms = int(DAYS * 86400_000)
    base_ms = int(BASELINE_DAYS * 86400_000)
    out = []
    if con is None:
        return [Signal("rule-restatement", "both", "Ethan re-dictating standing rules",
                       measured=False, why_unmeasured="amux.db unreadable")]
    rows = [r for r in con.execute(
        "SELECT id, text, session, ts FROM cmd_history "
        "WHERE type='user' AND origin='' AND ts >= ?",
        (now_ms - base_ms,),
    ).fetchall() if not is_bare_drive(r["text"])]
    if not rows:
        return [Signal("rule-restatement", "both", "Ethan re-dictating standing rules",
                       measured=True, n_considered=0)]

    for key, pat, doc_probe in RULE_CLASSES:
        rx = re.compile(pat, re.I)
        in_win, in_base = [], 0
        repos = Counter()
        for r in rows:
            if not rx.search(instruction_of(r["text"])):
                continue
            if r["ts"] >= now_ms - win_ms:
                in_win.append(r)
                repos[lane_repo(r["session"])] += 1
            else:
                in_base += 1
        if not in_win:
            continue
        baseline_rate = in_base / max(BASELINE_DAYS - DAYS, 1e-9)
        scope = "both" if repos.get("amux") and repos.get("mixpeek") else (
            "amux" if repos.get("amux") else "mixpeek" if repos.get("mixpeek") else "other")
        s = Signal(f"rule-restatement:{key}", scope,
                   f"Ethan restated the '{key}' rule", n_considered=len(rows))
        s.value = len(in_win)
        s.baseline = round(baseline_rate, 2)
        already_written = sorted(
            name for name, body in prompts_doc.items()
            if body and re.search(doc_probe, body, re.I))
        s.detail = {
            "already_written_in": already_written,
            "prose_exists": bool(already_written),
            "per_repo": dict(repos),
            "doc_probe": doc_probe,
        }
        # A class is active when today outruns its own trailing rate, or when a
        # rule that IS ALREADY WRITTEN got restated more than once. The second
        # arm is the point of the whole signal: prose losing to mechanism. It
        # needs n>=2 because a single mention of an already-documented topic is
        # ordinary conversation, and at n>=1 every class fired every day.
        s.active = (s.value > max(2.0, baseline_rate * 1.5)) or (
            already_written and s.value >= 2)
        s.evidence = [
            {"msg": f"MSG-{r['id']}", "session": r["session"],
             "ts": time.strftime("%Y-%m-%d %H:%M", time.localtime(r["ts"] / 1000)),
             "text": (r["text"] or "")[:220]}
            for r in in_win[:MAX_PER_SIGNAL]
        ]
        out.append(s)
    return out


# ---------------------------------------------------------------------------
# Signal 2: the same ask reaching two or more DIFFERENT lanes
#
# One lane getting the same instruction twice is that lane's problem. Two lanes
# getting it in the same window is a fleet default that is missing, which is
# exactly the class that belongs in a shared prompt.
# ---------------------------------------------------------------------------
STOP = set("the a an and or to of in for on is it that this with be as at by from "
           "we you i our your please can do does did make sure not no yes now "
           "then also just so if what why how when".split())


def shingle(text, n=5):
    words = [w for w in re.findall(r"[a-z']+", (text or "").lower()) if w not in STOP]
    return {" ".join(words[i:i + n]) for i in range(max(len(words) - n + 1, 0))}
    # n=5 on stopword-stripped text. At n=4 this collided on ordinary
    # English ("files reach them via") and reported unrelated messages as
    # the same instruction sent to five lanes.


def signal_cross_lane_repeat(con, now_ms):
    if con is None:
        return [Signal("cross-lane-repeat", "both", "Same ask sent to multiple lanes",
                       measured=False, why_unmeasured="amux.db unreadable")]
    win_ms = int(DAYS * 86400_000)
    rows = con.execute(
        "SELECT id, text, session, ts FROM cmd_history "
        "WHERE type='user' AND origin='' AND ts >= ? ORDER BY ts",
        (now_ms - win_ms,),
    ).fetchall()
    s = Signal("cross-lane-repeat", "both",
               "The same instruction sent to two or more different lanes",
               n_considered=len(rows))
    shingles = defaultdict(list)
    for r in rows:
        instr = instruction_of(r["text"])
        if len(instr) < 40:
            continue
        for sh in shingle(instr):
            shingles[sh].append(r)
    seen_pairs = set()
    for sh, hits in shingles.items():
        lanes = {h["session"] for h in hits}
        if len(lanes) < 2:
            continue
        key = tuple(sorted(h["id"] for h in hits))
        if key in seen_pairs:
            continue
        seen_pairs.add(key)
        repos = {lane_repo(h["session"]) for h in hits}
        s.value += 1
        s.evidence.append({
            "phrase": sh,
            "lanes": sorted(lanes),
            "repos": sorted(repos),
            "cross_repo": len(repos - {"other"}) > 1,
            "msgs": [f"MSG-{h['id']}" for h in hits][:4],
            "text": (hits[0]["text"] or "")[:200],
        })
    s.evidence.sort(key=lambda e: (not e["cross_repo"], -len(e["lanes"])))
    s.active = s.value > 0
    s.detail = {"cross_repo_instances": sum(1 for e in s.evidence if e["cross_repo"])}
    return [s]


# ---------------------------------------------------------------------------
# Signal 3: ledger clustering across BOTH repos, on one vocabulary
#
# frustrations.md's own rule: "one frustration is a complaint and a cluster is
# an argument". The cluster only forms if both ledgers are counted together,
# which no existing sweep does. amux's runs on amux's file, mixpeek's on
# mixpeek's, and a class present in both at n=2 each is invisible to both.
# ---------------------------------------------------------------------------
def parse_amux_ledger(text):
    """Field-per-line format. Entries are at column 0; the indented template in
    the header is deliberately NOT at column 0 so it cannot count itself."""
    entries = []
    cur = None
    for line in text.splitlines():
        if line.startswith("## "):
            if cur:
                entries.append(cur)
            cur = {"title": line[3:].strip()}
            continue
        if cur is None:
            continue
        m = re.match(r"^([A-Z]+): ?(.*)$", line)
        if m:
            cur[m.group(1).lower()] = m.group(2).strip()
    if cur:
        entries.append(cur)
    return [e for e in entries if "status" in e or "area" in e]


def parse_mixpeek_ledger(text):
    """Checkbox format: `- [ ] **YYYY-MM-DD | Area (title)** *(session)*: body`."""
    entries = []
    for line in text.splitlines():
        m = re.match(r"^- \[( |x)\] \*\*(\d{4}-\d{2}-\d{2}) \| (.+?)\*\*(.*)$", line)
        if not m:
            continue
        done, date, head, rest = m.groups()
        sess = re.search(r"\*\(([^)]+)\)\*", rest)
        entries.append({
            "status": "fixed" if done == "x" else "open",
            "date": date,
            "title": head.strip(),
            "session": sess.group(1) if sess else "",
            "body": rest[:400],
        })
    return entries


def signal_ledger_clusters(now_ms):
    amux_path = os.path.join(AMUX_REPO, "frustrations.md")
    mx_path = os.path.join(MIXPEEK_REPO, "FRUSTRATIONS.md")
    a_txt, a_err = read_text(amux_path)
    m_txt, m_err = read_text(mx_path)

    if a_txt is None and m_txt is None:
        return [Signal("ledger-cluster", "both", "Open friction classes across both ledgers",
                       measured=False,
                       why_unmeasured=f"amux: {a_err}; mixpeek: {m_err}")]

    # The standing size of a class is background: `instruments` has held ~99
    # open entries for weeks and reporting it daily says nothing new. What is
    # worth a look is the class that GREW in this window, and especially one
    # that grew on BOTH sides. That is a friction reaching two codebases at
    # once, which is the shape that belongs in the global prompt.
    fresh_cut = time.strftime("%Y-%m-%d",
                              time.localtime(now_ms / 1000 - DAYS * 86400))
    per_area = defaultdict(lambda: {"amux": [], "mixpeek": [],
                                    "fresh_amux": [], "fresh_mixpeek": []})
    n = 0

    def add(side, area, rec):
        per_area[area][side].append(rec)
        if rec["date"] and rec["date"] >= fresh_cut:
            per_area[area]["fresh_" + side].append(rec)

    # `n` counts ENTRIES and increments once each; an entry that carries a
    # cross-cutting membership as well as its subsystem one lands in two buckets
    # (AF-394). So the buckets can sum to more than `n`, which is correct and is
    # the whole point: "a batch reports COMPLETED and wrote zero" is genuinely an
    # engine defect AND an instrument that lied.
    if a_txt is not None:
        for e in parse_amux_ledger(a_txt):
            if (e.get("status") or "open").lower() != "open":
                continue
            n += 1
            title = e.get("title", "")
            area = e.get("area") or canon_area(title)
            rec = {"title": title[:120], "date": e.get("date", ""),
                   "card": e.get("card", ""), "session": e.get("session", "")}
            primary = canon_area(area)
            add("amux", primary, rec)
            for x in extra_areas(title):
                if x != primary:
                    add("amux", x, rec)
    if m_txt is not None:
        for e in parse_mixpeek_ledger(m_txt):
            if e["status"] != "open":
                continue
            n += 1
            rec = {"title": e["title"][:120], "date": e["date"],
                   "card": "", "session": e.get("session", "")}
            primary = canon_area(e["title"])
            add("mixpeek", primary, rec)
            for x in extra_areas(e["title"]):
                if x != primary:
                    add("mixpeek", x, rec)

    out = []
    for area, sides in sorted(per_area.items(),
                              key=lambda kv: -(len(kv[1]["fresh_amux"])
                                               + len(kv[1]["fresh_mixpeek"]))):
        a, m = len(sides["amux"]), len(sides["mixpeek"])
        fa, fm = len(sides["fresh_amux"]), len(sides["fresh_mixpeek"])
        total, fresh = a + m, fa + fm
        both = a > 0 and m > 0
        scope = "both" if both else ("amux" if a else "mixpeek")
        s = Signal(f"ledger-cluster:{area}", scope,
                   f"'{area}' gained {fresh} open entries in {DAYS}d "
                   f"({fa} amux / {fm} mixpeek); {total} open in total",
                   n_considered=n)
        s.value = fresh
        s.detail = {"standing_open": total, "standing_amux": a,
                    "standing_mixpeek": m, "new_in_window": fresh,
                    "spans_repos": both, "since": fresh_cut}
        # Two new entries in one class in one window is a live cluster. One new
        # entry on EACH side is the cross-repo case that no single-repo sweep
        # can see, and it is the one worth a global prompt change.
        s.active = fresh >= 2 or (fa >= 1 and fm >= 1)
        s.evidence = (sides["fresh_amux"] + sides["fresh_mixpeek"]
                      or sides["amux"] + sides["mixpeek"])[:MAX_PER_SIGNAL]
        if a_txt is None or m_txt is None:
            s.measured = True
            s.why_unmeasured = (
                f"PARTIAL: only one ledger readable "
                f"(amux: {'ok' if a_txt is not None else a_err}; "
                f"mixpeek: {'ok' if m_txt is not None else m_err})")
        out.append(s)
    return out


# ---------------------------------------------------------------------------
# Signal 4: nudge pressure that does not move a queue
#
# A lane that received many machine nudges and closed nothing is a loop with no
# negative feedback term. Per repo, because the two halves of the fleet run on
# different cadences and averaging them hides both.
# ---------------------------------------------------------------------------
def signal_nudge_without_movement(con, now_ms):
    if con is None:
        return [Signal("nudge-no-movement", "both", "Nudges sent to lanes whose queue did not move",
                       measured=False, why_unmeasured="amux.db unreadable")]
    win_ms = int(DAYS * 86400_000)
    since = now_ms - win_ms
    msgs = con.execute(
        "SELECT session, type, origin, COUNT(*) c FROM cmd_history "
        "WHERE ts >= ? GROUP BY session, type, origin", (since,)).fetchall()
    # issues.created / closed_at are SECONDS; cmd_history.ts is MILLISECONDS.
    # Mixing them turns every lane into "closed nothing" and every card into
    # "56,000 days old", both of which look like findings. ts_unit_warning()
    # re-checks the assumption each run so a future write in the other unit
    # surfaces as a warning instead of as a fleet-wide alarm.
    closed = dict(con.execute(
        "SELECT session, COUNT(*) c FROM issues "
        "WHERE closed_at IS NOT NULL AND closed_at >= ? GROUP BY session",
        (since // 1000,)).fetchall())

    per_lane = defaultdict(lambda: {"machine": 0, "human": 0})
    for r in msgs:
        bucket = "human" if (r["type"] == "user" and not r["origin"]) else "machine"
        per_lane[r["session"]][bucket] += r["c"]

    s = Signal("nudge-no-movement", "both",
               "Lanes nudged repeatedly whose board queue did not move",
               n_considered=sum(v["machine"] + v["human"] for v in per_lane.values()))
    for lane, counts in sorted(per_lane.items(), key=lambda kv: -kv[1]["machine"]):
        if counts["machine"] < 10:
            continue
        moved = closed.get(lane, 0)
        if moved > 0:
            continue
        s.value += 1
        s.evidence.append({
            "lane": lane, "repo": lane_repo(lane),
            "machine_msgs": counts["machine"], "human_msgs": counts["human"],
            "cards_closed_in_window": moved,
        })
    s.active = s.value > 0
    s.detail = {
        "lanes_over_threshold": s.value,
        "threshold": "10+ machine messages and 0 cards closed",
        "total_machine_msgs": sum(v["machine"] for v in per_lane.values()),
        "total_human_msgs": sum(v["human"] for v in per_lane.values()),
    }
    return [s]


# ---------------------------------------------------------------------------
# Signal 5: where cards rest, per repo
#
# The structural one. It has no daily baseline to beat, being a standing count
# and it is active whenever a status holds a queue older than its own purpose.
# ---------------------------------------------------------------------------
TREND_DAYS = float(os.environ.get("FRICTION_TREND_DAYS", "7"))


def signal_board_resting(con, now_ms):
    """Where cards rest, per repo, AND WHETHER IT IS GETTING WORSE.

    The standing count is not the signal. `todo` has held ~300 month-old cards
    for weeks; printing that daily is the log this file is supposed to prevent.
    The signal is the DELTA against the same measurement a week ago, which is
    reconstructible from `created`/`closed_at` with no state file to drift: a
    card was open at time T if it was created by T and not closed until after
    it. A pile that is shrinking needs no attention even at 300 cards; one
    growing off a small base does.
    """
    if con is None:
        return [Signal("board-resting", "both", "Statuses holding old cards",
                       measured=False, why_unmeasured="amux.db unreadable")]
    now_s = now_ms / 1000
    then_s = now_s - TREND_DAYS * 86400
    rows = con.execute(
        "SELECT id, status, session, created, closed_at FROM issues "
        "WHERE archived=0 AND deleted IS NULL").fetchall()
    open_now = defaultdict(list)
    open_then = Counter()
    resting = {"todo", "backlog", "needsyou", "review", "blocked", "doing", "armed"}
    for r in rows:
        prefix = (r["id"] or "").split("-")[0]
        repo = PREFIX_REPO.get(prefix, lane_repo(r["session"]))
        created, closed = r["created"], r["closed_at"]
        if r["status"] in resting:
            open_now[(repo, r["status"])].append((now_s - created) / 86400)
        # A card open a week ago: created by then, and either still open or
        # closed after that point. Status is today's status, so this counts the
        # pile's SIZE over time, not per-status migration, which is the
        # question being asked (is the pile growing) and not a stronger one.
        if created <= then_s and (closed is None or closed > then_s):
            open_then[(repo, r["status"])] += 1

    considered = len(rows)
    out = []
    for (repo, status), ages in sorted(open_now.items(), key=lambda kv: -len(kv[1])):
        ages.sort()
        n = len(ages)
        median = ages[n // 2]
        over7 = sum(1 for a in ages if a > 7)
        prev = open_then.get((repo, status), 0)
        delta = n - prev
        s = Signal(f"board-resting:{repo}:{status}", repo,
                   f"'{status}' in {repo}: {n} cards, median age {median:.1f}d, "
                   f"{delta:+d} vs {TREND_DAYS:.0f}d ago", n_considered=considered,
                   baseline_label=f" cards open {TREND_DAYS:.0f}d ago")
        s.value = n
        s.baseline = prev
        s.detail = {"median_age_days": round(median, 1),
                    "over_7_days": over7,
                    "pct_over_7_days": round(100 * over7 / n) if n else 0,
                    "oldest_days": round(ages[-1], 1),
                    "open_7d_ago": prev,
                    "delta_7d": delta,
                    "growing": delta > 0}
        # Growing, and already big enough and old enough that growth is not
        # just a busy week. A shrinking pile is the system working.
        s.active = delta > 0 and n >= 20 and median > 14
        out.append(s)
    return out


# ---------------------------------------------------------------------------
def load_prompt_docs():
    """The files a theme can be baked INTO. Reading them is how we answer
    'is this rule already written down', which decides prose-vs-mechanism."""
    candidates = {
        "global CLAUDE.md": os.path.expanduser("~/.claude/CLAUDE.md"),
        "amux CLAUDE.md": os.path.join(AMUX_REPO, "CLAUDE.md"),
        "amux ethos": os.path.join(AMUX_REPO, ".claude/rules/ethos.md"),
        "amux frustrations rule": os.path.join(AMUX_REPO, ".claude/rules/frustrations.md"),
        "mixpeek CLAUDE.md": os.path.join(MIXPEEK_REPO, "CLAUDE.md"),
        "mixpeek CONVENTIONS.md": os.path.join(MIXPEEK_REPO, "CONVENTIONS.md"),
        "mixpeek AGENTS.md": os.path.join(MIXPEEK_REPO, "AGENTS.md"),
    }
    out = {}
    for name, path in candidates.items():
        body, err = read_text(path)
        out[name] = body  # None means unreadable, and stays None on purpose
    return out


def main():
    as_json = "--json" in sys.argv
    show_all = "--all" in sys.argv
    now_ms = int(time.time() * 1000)
    con, db_err = db_connect()
    prompts = load_prompt_docs()

    signals = []
    signals += signal_rule_restatement(con, now_ms, prompts)
    signals += signal_cross_lane_repeat(con, now_ms)
    signals += signal_ledger_clusters(now_ms)
    signals += signal_nudge_without_movement(con, now_ms)
    signals += signal_board_resting(con, now_ms)

    sources = {
        "amux.db": {"readable": con is not None, "error": db_err, "path": DB},
        "amux frustrations.md": {
            "readable": os.path.exists(os.path.join(AMUX_REPO, "frustrations.md")),
            "path": os.path.join(AMUX_REPO, "frustrations.md")},
        "mixpeek FRUSTRATIONS.md": {
            "readable": os.path.exists(os.path.join(MIXPEEK_REPO, "FRUSTRATIONS.md")),
            "path": os.path.join(MIXPEEK_REPO, "FRUSTRATIONS.md")},
        "prompt docs": {name: (body is not None) for name, body in prompts.items()},
    }
    unread = [k for k, v in sources.items()
              if isinstance(v, dict) and v.get("readable") is False]
    ts_warning = ts_unit_warning(con)

    active = [s for s in signals if s.active and s.measured]
    unmeasured = [s for s in signals if not s.measured]
    payload = {
        "generated_at": time.strftime("%Y-%m-%d %H:%M:%S"),
        "window_days": DAYS,
        "baseline_days": BASELINE_DAYS,
        "sources": sources,
        "unreadable_sources": unread,
        "timestamp_unit_warning": ts_warning,
        "n_signals_computed": len(signals),
        "n_signals_active": len(active),
        "n_signals_unmeasured": len(unmeasured),
        "active": [s.to_dict() for s in active],
        "unmeasured": [s.to_dict() for s in unmeasured],
        "all": [s.to_dict() for s in signals] if show_all else None,
    }

    if as_json:
        print(json.dumps(payload, indent=2))
        return 0

    print(f"# Fleet friction themes: {payload['generated_at']}")
    print(f"window {DAYS}d, baseline {BASELINE_DAYS}d, "
          f"{len(signals)} signals computed, {len(active)} active, "
          f"{len(unmeasured)} unmeasured")
    if unread:
        print(f"UNREADABLE SOURCES: {', '.join(unread)} "
              f"(treat their signals as unknown, not clean)")
    if ts_warning:
        print(f"TIMESTAMP UNIT WARNING: {ts_warning}")
        print("  Every window below is computed from those units. Do not file")
        print("  anything from this run until the unit is settled.")
    print()
    if unmeasured:
        print("## Could not measure")
        for s in unmeasured:
            print(f"  - {s.key}: {s.why_unmeasured}")
        print()
    if not active:
        print("## No active themes")
        print("  Every signal is at or below its trailing baseline and no")
        print("  structural threshold is crossed. That is a real result.")
        return 0

    print("## Active themes")
    for s in sorted(active, key=lambda x: (x.repo_scope != "both", -x.value)):
        scope = "BOTH REPOS" if s.repo_scope == "both" else s.repo_scope.upper()
        print(f"\n### [{scope}] {s.key}  (n={s.value}"
              + (f", baseline {s.baseline}{s.baseline_label}"
                 if s.baseline is not None else "")
              + f", considered {s.n_considered})")
        print(f"  {s.headline}")
        if s.detail:
            print(f"  detail: {json.dumps(s.detail)}")
        for e in s.evidence[:MAX_PER_SIGNAL]:
            print(f"    - {json.dumps(e)[:300]}")
    print("\nSignals are candidates, not findings. Name the theme, decide which")
    print("file absorbs it, and say so on the card.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
