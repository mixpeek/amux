#!/usr/bin/env python3
"""Send a message to a Codex chat by its name: codex-send.py <chat> <message|->.

Resolves <chat> against the thread names Codex keeps in ~/.codex/state_5.sqlite
(falls back to ~/.codex/session_index.jsonl), then runs `codex queue`.

<chat> may be a thread UUID, an exact name ("#mxp onboarding flow"), or a
fragment that matches exactly one live thread name ("mxp onboarding"). Matching
ignores case, a leading '#', and punctuation. Archived threads are skipped.
An ambiguous or missing name exits 2 and lists candidates; nothing is sent.

Only thread NAMES and ids are printed. Titles and first messages are never
read into output: they can carry pasted secrets.

The message is prefixed with the sending worker ($AMUX_SESSION) unless
--no-origin is given. That prefix is a claim, not a verified stamp: Codex does
not authenticate it.
"""
import argparse, json, os, re, sqlite3, subprocess, sys
from pathlib import Path

CODEX = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex"))
UUID = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$", re.I)
LOG = Path.home() / ".amux" / "logs" / "codex-send.log"
# PATH is thin under launchd and the scheduler, so look where codex installs too.
CODEX_BIN_FALLBACKS = ["/usr/local/bin/codex", "/opt/homebrew/bin/codex",
                       str(Path.home() / ".local/bin/codex")]


def codex_bin():
    """The first codex that actually runs. A broken npm install on PATH (a missing platform
    package: "spawn .../vendor/x86_64-apple-darwin/codex/codex ENOENT", seen 2026-10-17) must not
    hide the working binaries the Codex app and its daemon ship."""
    import glob, shutil
    cands = [str(Path.home() / ".codex/packages/app-server-daemon/current/bin/codex"),
             "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
             "/Applications/Codex.app/Contents/Resources/codex"]
    found = shutil.which("codex")
    if found:
        cands.append(found)
    cands += sorted(glob.glob(str(Path.home() / ".nvm/versions/node/*/bin/codex")))[::-1] + CODEX_BIN_FALLBACKS
    for c in dict.fromkeys(cands):
        if os.access(c, os.X_OK):
            try:
                if subprocess.run([c, "--version"], capture_output=True, timeout=20).returncode == 0:
                    return c
            except (OSError, subprocess.TimeoutExpired):
                pass
            log(verdict="codex_bin_broken", path=c)
    return None


def log(**rec):
    LOG.parent.mkdir(parents=True, exist_ok=True)
    rec["sender"] = os.environ.get("AMUX_SESSION", "")
    with LOG.open("a") as f:
        f.write(json.dumps(rec) + "\n")


def norm(s):
    s = s.strip().lower().lstrip("#")
    return " ".join(re.sub(r"[^\w\s]", " ", s).split())


def threads():
    """[(id, name, updated)] for live, named threads. Newest first."""
    db = CODEX / "state_5.sqlite"
    if db.exists():
        try:
            con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5)
            rows = con.execute(
                "select id, name, recency_at from threads "
                "where name is not null and trim(name) != '' and archived = 0 "
                "order by recency_at desc").fetchall()
            return rows, "state_5.sqlite"
        except sqlite3.Error as e:
            print(f"codex-send: state db unreadable ({e}); using session_index.jsonl", file=sys.stderr)
    latest = {}
    idx = CODEX / "session_index.jsonl"
    for line in idx.read_text().splitlines() if idx.exists() else []:
        try:
            r = json.loads(line)
        except ValueError:
            continue
        if r.get("thread_name"):
            latest[r["id"]] = (r["id"], r["thread_name"], r.get("updated_at", ""))
    return sorted(latest.values(), key=lambda r: r[2], reverse=True), "session_index.jsonl"


def resolve(query):
    rows, source = threads()
    if UUID.match(query.strip()):
        hit = [r for r in rows if r[0] == query.strip()]
        return (hit[0] if hit else (query.strip(), "(unnamed or archived)", 0)), [], source, len(rows)
    q = norm(query)
    exact = [r for r in rows if norm(r[1]) == q]
    if len(exact) == 1:
        return exact[0], [], source, len(rows)
    if exact:
        return None, exact, source, len(rows)
    toks = q.split()
    partial = [r for r in rows if all(t in norm(r[1]).split() or t in norm(r[1]) for t in toks)]
    if len(partial) == 1:
        return partial[0], [], source, len(rows)
    return None, partial, source, len(rows)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("chat")
    ap.add_argument("message", help="message text, or - to read stdin")
    ap.add_argument("--dry-run", action="store_true", help="resolve only, send nothing")
    ap.add_argument("--no-origin", action="store_true", help="do not prefix the sender")
    a = ap.parse_args()

    hit, cands, source, n = resolve(a.chat)
    if not hit:
        why = "matches several threads" if cands else "matches no live named thread"
        print(f"codex-send: '{a.chat}' {why} (searched {n} named threads in {source}). Nothing sent.", file=sys.stderr)
        for r in cands[:15]:
            print(f"  {r[1]}  [{r[0]}]", file=sys.stderr)
        if not cands:
            print("  list them: codex-send.py --list", file=sys.stderr)
        log(chat=a.chat, verdict="not_sent_ambiguous" if cands else "not_sent_no_match",
            candidates=len(cands))
        sys.exit(2)
    tid, name = hit[0], hit[1]
    print(f"codex-send: '{a.chat}' -> {name} [{tid}] (from {source}, {n} named threads)")

    msg = sys.stdin.read() if a.message == "-" else a.message
    if not msg.strip():
        print("codex-send: empty message. Nothing sent.", file=sys.stderr)
        log(chat=a.chat, thread=tid, name=name, verdict="not_sent_empty")
        sys.exit(2)
    # An unsigned message lands in a chat the owner types into, so it reads as the
    # owner's own words. 2026-10-24: $AMUX_SESSION was empty in amux-helper's shell,
    # five relays went out with no prefix, and the orchestrator adopted one as a
    # direct owner instruction. So fall back to the tmux session name, and refuse
    # rather than send unsigned unless --no-origin says that is intended.
    sender, origin_source = os.environ.get("AMUX_SESSION", "").strip(), "env"
    if not sender and os.environ.get("TMUX"):
        try:
            sender = subprocess.run(["tmux", "display-message", "-p", "#S"], capture_output=True,
                                    text=True, timeout=5).stdout.strip()
            origin_source = "tmux"
        except (OSError, subprocess.SubprocessError):
            sender = ""
        # amux names a worker's tmux session amux-<worker>.
        if sender.startswith("amux-") and (Path.home() / ".amux" / "sessions"
                                           / f"{sender[5:]}.env").is_file():
            sender = sender[5:]
    if not a.no_origin:
        if not sender:
            print("codex-send: no sender identity ($AMUX_SESSION is empty and no tmux session "
                  "name), so the message would arrive unsigned and read as the owner's. Set "
                  "AMUX_SESSION, or pass --no-origin if unsigned is intended. Nothing sent.",
                  file=sys.stderr)
            log(chat=a.chat, thread=tid, name=name, verdict="not_sent_no_origin")
            sys.exit(2)
        if origin_source != "env":
            print(f"codex-send: WARN $AMUX_SESSION empty; signing as '{sender}' from the tmux "
                  "session name", file=sys.stderr)
        msg = f"From amux worker {sender}:\n\n{msg}"
    if a.dry_run:
        print(f"codex-send: dry run, {len(msg)} chars not sent")
        return
    exe = codex_bin()
    if not exe:
        print("codex-send: the codex CLI was not found on PATH or in its usual install "
              "paths. Nothing sent.", file=sys.stderr)
        log(chat=a.chat, thread=tid, name=name, verdict="not_sent_no_codex_cli")
        sys.exit(3)
    try:
        p = subprocess.run([exe, "queue", "--thread", tid, "--message", msg],
                           capture_output=True, text=True, cwd=str(Path.home()), timeout=120)
    except subprocess.TimeoutExpired:
        print("codex-send: codex queue timed out after 120s; delivery unknown.", file=sys.stderr)
        log(chat=a.chat, thread=tid, name=name, verdict="unknown_timeout")
        sys.exit(4)
    out = (p.stdout + p.stderr).strip()
    print(out)
    queued = "Queued message" in out
    log(chat=a.chat, thread=tid, name=name, chars=len(msg), exit=p.returncode, queued=queued,
        verdict="queued" if queued and p.returncode == 0 else "not_queued", out=out[-300:])
    if p.returncode != 0 or not queued:
        print("codex-send: codex queue did not confirm the message was queued", file=sys.stderr)
        sys.exit(p.returncode or 1)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--list"]:
        rows, source = threads()
        for r in rows:
            print(f"{r[1]}  [{r[0]}]")
        print(f"({len(rows)} live named threads, {source})", file=sys.stderr)
        sys.exit(0)
    main()
