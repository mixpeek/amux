#!/usr/bin/env python3
"""GitHub stargazers of a repository into the amux CRM (owner 2026-11-24: "do
github stars of amux"). A star is the warmest outbound signal amux has: the
person found the repo and said so.

    scripts/crm-github-stars.py mixpeek/amux [--enroll SEQ-1] [--limit N] [--dry-run]

Each stargazer becomes a person (or is matched to one: by GitHub login in
`external.github`, then by public email), with a github channel on their
profile, tags github-star and github-star:<repo>, source github-star:<repo>,
and one activity line "starred <repo>" dated when they starred it (repeat runs
add nothing). Public profile fields fill what is empty: name, company, location,
email, X handle, blog. With --enroll, people new to the CRM are enrolled in that
sequence, so step 1 is planned and announced as crm.touch.due when it is due.

Run it from the amux scheduler (kind shell) to pick up new stars daily. GitHub
reads use the App token (~/.amux/github-app/get-token.sh) when present.
Prints one JSON summary line: measured, n_considered, created, matched, enrolled.
"""
import json
import os
import subprocess
import sys
import urllib.request
import ssl

def amux_url():
    try:
        return subprocess.run(["amux", "url"], capture_output=True, text=True, timeout=10).stdout.strip() or os.environ.get("AMUX_URL", "")
    except Exception:
        return os.environ.get("AMUX_URL", "")

API = amux_url()
CTX = ssl._create_unverified_context()
WHO = os.environ.get("AMUX_SESSION", "") or "crm-github-stars"


def crm(method, path, body=None):
    req = urllib.request.Request(API + "/api/crm" + path, method=method, data=json.dumps(body).encode() if body is not None else None,
                                 headers={"Content-Type": "application/json", "X-Amux-Session": WHO})
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=30) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def ext_of(v):
    if isinstance(v, dict):
        return v
    try:
        return json.loads(v or "{}") or {}
    except (ValueError, TypeError):
        return {}


def gh_env():
    env = dict(os.environ)
    tok = os.path.expanduser("~/.amux/github-app/get-token.sh")
    if os.path.exists(tok):
        out = subprocess.run([tok, "--min-remaining", "1200", "--raw"], capture_output=True, text=True).stdout.strip()
        if out:
            env["GH_TOKEN"] = out
    return env


def gh(path, env, accept=None):
    cmd = ["gh", "api", "--paginate", path] + (["-H", f"Accept: {accept}"] if accept else [])
    out = subprocess.run(cmd, capture_output=True, text=True, env=env)
    if out.returncode:
        raise SystemExit(json.dumps({"measured": False, "why": f"gh api {path}: {out.stderr.strip()[:200]}"}))
    # --paginate concatenates JSON arrays as ][
    return json.loads("[" + out.stdout.strip().replace("]\n[", ",").replace("][", ",").strip("[]") + "]") if out.stdout.strip().startswith("[") else json.loads(out.stdout)


def main():
    args = sys.argv[1:]
    if not args or args[0].startswith("-"):
        sys.exit(__doc__)
    repo = args[0]
    seq = args[args.index("--enroll") + 1] if "--enroll" in args else ""
    limit = int(args[args.index("--limit") + 1]) if "--limit" in args else 0
    dry = "--dry-run" in args
    env = gh_env()
    stars = gh(f"repos/{repo}/stargazers?per_page=100", env, accept="application/vnd.github.star+json")
    stars.sort(key=lambda s: s.get("starred_at", ""), reverse=True)
    if limit:
        stars = stars[:limit]
    # Who is already here: by email from the list, and by GitHub login from the
    # people a previous run tagged (the list carries no `external`, one GET each).
    _, people = crm("GET", "/contacts?limit=5000")
    by_login, by_email = {}, {}
    for p in people if isinstance(people, list) else []:
        if p.get("email"):
            by_email[p["email"].lower()] = p["id"]
        if "github-star" in (p.get("tags") or []):
            _, full = crm("GET", f"/contacts/{p['id']}")
            gl = ext_of((full or {}).get("external")).get("github", "")
            if gl:
                by_login[gl.lower()] = p["id"]
    created, matched, enrolled, failed = [], [], [], []
    tag = f"github-star:{repo}"
    for s in stars:
        login = s["user"]["login"]
        u = gh(f"users/{login}", env)
        email = (u.get("email") or "").strip().lower()
        pid = by_login.get(login.lower()) or (by_email.get(email) if email else None)
        fields = {
            "name": u.get("name") or login,
            "company": (u.get("company") or "").lstrip("@").strip(),
            "location": u.get("location") or "",
            "email": email,
            "twitter": ("@" + u["twitter_username"]) if u.get("twitter_username") else "",
            "notes": "\n".join(x for x in [u.get("bio") or "", u.get("blog") or "", f"https://github.com/{login} ({u.get('followers', 0)} followers, {u.get('public_repos', 0)} public repos)"] if x),
        }
        if dry:
            print(json.dumps({"login": login, "match": pid, **{k: v for k, v in fields.items() if k != "notes"}}))
            continue
        if pid:
            matched.append(pid)
        else:
            st, r = crm("POST", "/contacts", {**fields, "source": f"github-star:{repo}", "tags": ["github-star", tag],
                                               "external": {"github": login}})
            if st == 409 and r.get("existing"):
                pid = r["existing"]; matched.append(pid)
            elif st == 201:
                pid = r["id"]; created.append(pid)
            else:
                failed.append({"login": login, "status": st, "error": r.get("error")}); continue
        if pid in matched:
            # Fill only what is empty; never overwrite what someone wrote.
            _, cur = crm("GET", f"/contacts/{pid}")
            patch = {k: v for k, v in fields.items() if v and not (cur or {}).get(k) and k != "name"}
            ext = ext_of((cur or {}).get("external"))
            if not ext.get("github"):
                patch["external"] = {**ext, "github": login}
            tags = sorted(set(((cur or {}).get("tags") or [])) | {"github-star", tag})
            patch["tags"] = tags
            crm("PATCH", f"/contacts/{pid}", patch)
        _, chans = crm("GET", f"/channels?owner_kind=contact&owner_id={pid}")
        if not any(c.get("kind") == "github" for c in (chans if isinstance(chans, list) else [])):
            crm("POST", "/channels", {"owner_kind": "contact", "owner_id": pid, "kind": "github", "url": f"https://github.com/{login}"})
        crm("POST", f"/contacts/{pid}/interactions", {"type": "other", "direction": "in", "subject": f"starred {repo}",
             "date": (s.get("starred_at") or "")[:10], "notes": f"starred https://github.com/{repo}", "message_id": f"star:{repo}:{login}"})
        if seq and pid in created:
            st, r = crm("POST", f"/sequences/{seq}/enroll", {"contact_ids": [pid]})
            if st == 200 and r.get("enrolled"):
                enrolled.append(pid)
    print(json.dumps({"measured": True, "repo": repo, "n_considered": len(stars), "created": len(created), "matched": len(matched),
                      "enrolled": len(enrolled), "failed": failed[:10], "n_failed": len(failed), "dry_run": dry}))


if __name__ == "__main__":
    main()
