#!/usr/bin/env python3
"""MC-1203: Post-push SHA verify — MECHANISM, not convention.

PostToolUse hook on Bash. After a `git push` that updated origin/main, verify
that the commit the push SENT is reachable from origin/main. If it is not, the
push was overtaken or rewritten and the work is not on main.

Four shared-tree incidents in one day (2026-07-29), ALL caught by luck:
  1. Manifest edits swept into a concurrent session's commit by file-level git add
  2. Push failed on sibling race; rebase-retry landed it but only manual verify confirmed
  3. git commit lost a race to another session's index.lock
  4. Uncommitted file misattributed across sessions

WHICH COMMIT (mxp-gs12 review, 2026-10-18). This hook used to compare the
checkout's local HEAD with origin/main. `amux land`, graft pushes and temp-index
pushes (`git push origin <sha>:main`) never move local HEAD, so on the Mixpeek
checkout HEAD sat at one commit for six hours while lanes landed dozens, and
the hook raised "POST-PUSH SHA VERIFY FAILED" 59 times, every one false, each
telling the lane to `git pull --rebase` on a shared checkout. The pushed commit
now comes from the push's own output (`<old>..<new>  <src> -> main`), then
from an explicit refspec, and the hook says plainly when it cannot tell
instead of guessing HEAD. A rejected push is reported as rejected, not as
overtaken. The advice no longer rebases a shared checkout.

Every run appends one JSON line to ~/.amux/logs/post-push-verify.log with the
verdict (confirmed, not_on_origin, rejected, up_to_date, unmeasured) and where
the sha came from (output, refspec), so a sweep can count false alarms.

FAIL-OPEN: any error silently exits 0. This mechanism must never block work.
"""
import json
import os
import re
import subprocess
import sys
import time


def _run(cmd, cwd, timeout=5):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, cwd=cwd)


def _log(rec):
    try:
        d = os.path.join(os.path.expanduser("~"), ".amux", "logs")
        os.makedirs(d, exist_ok=True)
        rec = {"ts": round(time.time(), 3), "session": os.environ.get("AMUX_SESSION", ""), **rec}
        with open(os.path.join(d, "post-push-verify.log"), "a") as f:
            f.write(json.dumps(rec) + "\n")
    except Exception:
        pass


def _ci_started(repo_root, head):
    """Did the push actually start ANY workflow run? (AC-267)

    The SHA landing on origin/main is not the same as the push having an
    effect. On 2026-08-06 two pushes to main (27 commits) produced ZERO workflow
    runs, a GitHub-side delivery failure with Actions enabled.

    Bounded to ~15s and fail-open. A skip announces itself rather than passing
    silently, which is the AC-261 lesson: a quiet skip reads as a clean check.
    """
    import time as _t
    for _attempt in range(3):
        try:
            r = subprocess.run(
                ["gh", "run", "list", "--limit", "25", "--json", "headSha,name,status"],
                cwd=repo_root, capture_output=True, text=True, timeout=15)
        except FileNotFoundError:
            print("post-push-verify: CI NOT CHECKED — `gh` not on PATH.")
            return
        except Exception as e:
            print("post-push-verify: CI NOT CHECKED — %s." % type(e).__name__)
            return
        if r.returncode != 0:
            print("post-push-verify: CI NOT CHECKED — gh exited %d." % r.returncode)
            return
        try:
            runs = [x for x in json.loads(r.stdout or "[]")
                    if (x.get("headSha") or "").startswith(head[:12])]
        except Exception:
            print("post-push-verify: CI NOT CHECKED — unreadable gh output.")
            return
        if runs:
            names = ", ".join(sorted({x.get("name", "?")[:24] for x in runs})[:3])
            print("post-push-verify: %d CI run(s) started for %s (%s)"
                  % (len(runs), head[:10], names))
            return
        if _attempt < 2:
            _t.sleep(5)
    print(
        "\n" + "=" * 64 + "\n"
        "  NO CI RUN STARTED FOR THIS PUSH (AC-267)\n"
        + "=" * 64 + "\n"
        "  Pushed SHA: " + head[:10] + "\n\n"
        "  The SHA is on origin/main, but GitHub started NO workflow run for\n"
        "  it after ~15s. Anything you expected to deploy did NOT deploy.\n\n"
        "  Do not report this work as shipped. Check and re-trigger:\n"
        "    gh run list --limit 5\n"
        "  Then verify against PROD, not against the green check.\n"
        + "=" * 64)


# `<old>..<new>  <src> -> main`, `+ <old>...<new> <src> -> main (forced update)`
_UPDATED = re.compile(r"^\s*[+* ]?\s*([0-9a-f]{7,40})\.\.\.?([0-9a-f]{7,40})\s+(\S+)\s+->\s+(?:refs/heads/)?main\b", re.M)
_NEW = re.compile(r"^\s*\*\s+\[new branch\]\s+(\S+)\s+->\s+(?:refs/heads/)?main\b", re.M)
_REJECTED = re.compile(r"^\s*!\s+\[(?:remote )?rejected\]\s+\S+\s+->\s+(?:refs/heads/)?main\b", re.M)
_REFSPEC = re.compile(r"\bgit\s+(?:-C\s+\S+\s+)?push\b[^\n;&|]*?\s(?:origin)\s+\+?([^\s:]+):(?:refs/heads/)?main\b")
_PLAIN = re.compile(r"\bgit\s+(?:-C\s+\S+\s+)?push\b[^\n;&|]*?\sorigin\s+(main|HEAD)\s*(?:$|[;&|\n])")


def pushed_commit(cmd, output, repo_root):
    """(verdict, sha, source). verdict in updated, rejected, up_to_date, unknown."""
    m = list(_UPDATED.finditer(output))
    if m:
        return "updated", m[-1].group(2), "output"
    n = list(_NEW.finditer(output))
    if n:
        r = _run(["git", "rev-parse", "-q", "--verify", n[-1].group(1) + "^{commit}"], repo_root)
        if r.returncode == 0:
            return "updated", r.stdout.strip(), "output"
    if _REJECTED.search(output):
        return "rejected", None, "output"
    if "Everything up-to-date" in output:
        return "up_to_date", None, "output"
    # No ref-update lines (output redirected or not captured): an explicit
    # refspec names the commit. `git push origin main` pushes the local branch
    # main, never HEAD, and HEAD only when it is named.
    s = _REFSPEC.search(cmd)
    src = s.group(1) if s else None
    if not src:
        p = _PLAIN.search(cmd)
        src = ("refs/heads/main" if p and p.group(1) == "main" else "HEAD") if p else None
    if src and not src.startswith("$"):
        r = _run(["git", "rev-parse", "-q", "--verify", src + "^{commit}"], repo_root)
        if r.returncode == 0 and len(r.stdout.strip()) == 40:
            return "updated", r.stdout.strip(), "refspec"
    return "unknown", None, None


def main():
    try:
        d = json.load(sys.stdin)
    except Exception:
        return
    cmd = (d.get("tool_input") or {}).get("command", "") or ""
    if not re.search(r"\bgit\s+(?:-C\s+\S+\s+)?push\b", cmd) or "origin" not in cmd or "main" not in cmd:
        return
    resp = d.get("tool_response")
    if isinstance(resp, dict):
        output = "\n".join(str(resp.get(k) or "") for k in ("stdout", "stderr", "output"))
    else:
        output = str(resp or "")
    try:
        repo_root = _run(["git", "rev-parse", "--show-toplevel"], cwd=d.get("cwd") or ".").stdout.strip()
        if not repo_root:
            return
    except Exception:
        return
    try:
        verdict, sha, source = pushed_commit(cmd, output, repo_root)
    except Exception:
        return
    if verdict == "rejected":
        print("post-push-verify: the push to main was REJECTED in its own output; nothing landed from it, so there is nothing to verify.")
        _log({"verdict": "rejected", "measured": True, "n_considered": 1, "repo": repo_root})
        return
    if verdict == "up_to_date":
        _log({"verdict": "up_to_date", "measured": True, "n_considered": 1, "repo": repo_root})
        return
    if verdict == "unknown":
        print("post-push-verify: NOT VERIFIED — could not tell which commit this push sent to main "
              "(no ref update in its output and no explicit refspec). Check with: "
              "git fetch origin main && git branch -r --contains <sha>")
        _log({"verdict": "unmeasured", "measured": False, "why_unmeasured": "no pushed sha in output or refspec",
              "n_considered": 0, "repo": repo_root})
        return
    try:
        token_script = os.path.expanduser("~/.amux/github-app/get-token.sh")
        if os.path.exists(token_script):
            subprocess.run(["bash", "-c", f'eval "$({token_script})" && git fetch origin main --quiet'],
                           capture_output=True, timeout=15, cwd=repo_root)
        else:
            _run(["git", "fetch", "origin", "main", "--quiet"], cwd=repo_root, timeout=15)
    except Exception:
        return
    try:
        short = sha[:10]
        if _run(["git", "merge-base", "--is-ancestor", sha, "origin/main"], cwd=repo_root).returncode == 0:
            print(f"post-push-verify: {short} confirmed on origin/main ✓ (sha from the push {source})")
            _log({"verdict": "confirmed", "sha": sha, "source": source, "measured": True, "n_considered": 1, "repo": repo_root})
            _ci_started(repo_root, sha)
            return
        remote_head = _run(["git", "rev-parse", "origin/main"], cwd=repo_root).stdout.strip()[:10]
        _log({"verdict": "not_on_origin", "sha": sha, "source": source, "origin_main": remote_head,
              "measured": True, "n_considered": 1, "repo": repo_root})
        print(
            f"\n{'=' * 64}\n"
            f"  POST-PUSH SHA VERIFY FAILED (MC-1203)\n"
            f"{'=' * 64}\n"
            f"  Pushed commit: {short} (from the push {source})\n"
            f"  origin/main:   {remote_head}\n"
            f"\n"
            f"  The commit this push sent to main is NOT reachable from origin/main,\n"
            f"  so main was rewritten or moved past it after the push.\n"
            f"\n"
            f"  Check: git branch -r --contains {short}\n"
            f"  If it is really missing, land it again without touching a shared\n"
            f"  checkout's HEAD:  amux land --sha {short}\n"
            f"  (Do not `git pull --rebase` in a checkout other lanes share.)\n"
            f"{'=' * 64}\n"
        )
    except Exception:
        return


if __name__ == "__main__":
    main()
