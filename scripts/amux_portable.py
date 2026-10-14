#!/usr/bin/env python3
"""Move an amux server's setup between machines: local to Cloud, Cloud to local, Cloud to Cloud
(docs/design/cloud-self-service.md 7.1).

Everything goes through amux's public HTTP API, never the database file: export
reads what the API lists, import replays it through the same create calls on the
target. That keeps secrets that live only in the database out of the bundle, works
across server versions, and runs every write through the target's own permission
checks. Import is idempotent: each kind is matched on its natural key, and a
second import of the same bundle changes nothing and says so.

  amux_portable.py export --api URL --out NAME.amuxmove [--files PATH ...]
  amux_portable.py import --api URL NAME.amuxmove [--apply]   (dry run unless --apply)
  amux_portable.py cloud-login                                 device sign-in to cloud.amux.io
  amux_portable.py cloud-move [--plan solo] [--files PATH ...] local amux to a Cloud server
  amux_portable.py cloud-pull SLUG [--files PATH ...]          a Cloud server to local amux
  amux_portable.py cloud-apply amux.yaml [--confirm-billing]   provision or update a server from YAML

Env: AMUX_API (default https://localhost:8824), AMUX_AUTH_TOKEN (bearer for the source/target
when it needs one), AMUX_CLOUD_URL (default https://cloud.amux.io).
"""
import argparse, base64, getpass, io, json, os, re, ssl, sys, tarfile, time, urllib.error, urllib.parse, urllib.request
import webbrowser

VERSION = 1
CLOUD = os.environ.get("AMUX_CLOUD_URL", "https://cloud.amux.io").rstrip("/")
TOKEN_FILE = os.path.expanduser(os.environ.get("AMUX_CLOUD_TOKEN_FILE", "~/.amux/cloud/token"))
SECRETISH = re.compile(r"(sk-ant-[A-Za-z0-9_-]{8,}|sk-[A-Za-z0-9]{20,}|ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}|"
                       r"xox[abp]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|tskey-[A-Za-z0-9-]{10,}|-----BEGIN [A-Z ]*PRIVATE KEY-----)")
SCHEDULE_FIELDS = {"title", "session", "command", "script_path", "kind", "sched_type", "recurrence", "run_at",
                   "enabled", "schedule_expr", "watch_timeout", "worktree", "by"}
BOARD_FIELDS = {"title", "desc", "status", "session", "type", "tags", "due", "due_time", "acceptance_criteria", "next_action"}


# ------------------------------------------------------------------ HTTP
class Api:
    def __init__(self, base, headers=None):
        self.base = base.rstrip("/")
        self.headers = dict(headers or {})
        tok = os.environ.get("AMUX_AUTH_TOKEN")
        if tok and "Cookie" not in self.headers:
            self.headers["Authorization"] = f"Bearer {tok}"
        self.ctx = ssl.create_default_context()
        if urllib.parse.urlparse(self.base).hostname in ("localhost", "127.0.0.1", "::1"):
            self.ctx.check_hostname, self.ctx.verify_mode = False, ssl.CERT_NONE   # a local server's own certificate

    def call(self, method, path, body=None, raw=None, ctype=None, ok=(200, 201, 202, 204)):
        data, headers = None, dict(self.headers)
        if body is not None:
            data, headers["Content-Type"] = json.dumps(body).encode(), "application/json"
        elif raw is not None:
            data, headers["Content-Type"] = raw, ctype or "application/octet-stream"
        req = urllib.request.Request(self.base + path, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=60, context=self.ctx) as r:
                payload = r.read()
                status = r.status
        except urllib.error.HTTPError as e:
            payload, status = e.read(), e.code
        if status not in ok and status not in (409,):
            raise RuntimeError(f"{method} {path} -> HTTP {status}: {payload[:300].decode(errors='replace')}")
        try:
            return status, json.loads(payload) if payload else None
        except ValueError:
            return status, payload

    def get(self, path):
        return self.call("GET", path)[1]


def rows(x, key):
    if isinstance(x, list):
        return x
    return (x or {}).get(key) or []


# ------------------------------------------------------------------ export
def export(api, out, file_roots=()):
    data, files, warnings = {}, {}, []
    data["config"] = api.get("/api/config/export")
    workers = [w for w in rows(api.get("/api/sessions"), "sessions") if not w.get("archived")]
    data["workers"] = [{k: w.get(k) for k in ("name", "dir", "desc", "model", "provider", "flags", "tags", "worker_type")}
                       for w in workers]
    data["groups"] = [{k: g.get(k) for k in ("name", "department", "goal", "kpis", "human_cost")}
                      for g in rows(api.get("/api/groups"), "groups")]
    data["schedules"] = [{k: v for k, v in s.items() if k in SCHEDULE_FIELDS} for s in rows(api.get("/api/schedules"), "schedules")]
    mems = rows(api.get("/api/memories"), "items")
    for w in workers:
        st, body = api.call("GET", "/api/memories?worker=" + urllib.parse.quote(w["name"]), ok=(200, 404))
        if st == 200:                     # 404: a legacy worker with no worker-scoped memory store
            mems += rows(body, "items")
    seen, data["memories"] = set(), []
    for m in mems:
        key = (json.dumps(m.get("scope"), sort_keys=True), m.get("name"))
        if key in seen:
            continue
        seen.add(key)
        data["memories"].append({k: m.get(k) for k in ("scope", "name", "content", "memory_type", "expires_at")})
    data["board"] = [dict({k: c.get(k) for k in BOARD_FIELDS if c.get(k) is not None}, old_id=c.get("id"),
                          archived=bool(c.get("archived")), depends_on=c.get("depends_on"))
                     for c in rows(api.get("/api/board?all=1&full=1"), "items")]
    contacts = []
    for c in rows(api.get("/api/crm/contacts"), "contacts"):
        full = api.get(f"/api/crm/contacts/{c['id']}") or c
        contacts.append({k: full.get(k) for k in ("name", "company", "role", "email", "linkedin", "twitter", "phone",
                                                    "notes", "tags", "interactions")})
    data["crm"] = contacts
    data["skills"] = [{"name": s["name"], "content": (api.get(f"/api/skills/{urllib.parse.quote(s['name'])}") or {}).get("content")}
                      for s in rows(api.get("/api/skills"), "skills")]
    data["saved_messages"] = [{k: m.get(k) for k in ("session", "label", "text")} for m in rows(api.get("/api/saved-messages"), "items")]
    data["journal"] = [{k: j.get(k) for k in ("date", "text", "tags", "place_name", "starred", "lat", "lng")}
                       for j in rows(api.get("/api/journal"), "items")]
    data["repos"] = repos_of(api, data["workers"], warnings)
    for root in file_roots:
        collect_files(api, root.strip("/"), files, warnings)
    data["env_names"] = env_names(api)
    blob = json.dumps(data)
    if SECRETISH.search(blob):
        hits = sorted(set(m.group(0)[:12] + "…" for m in SECRETISH.finditer(blob)))
        raise SystemExit(f"refusing to write a bundle that contains secret-shaped values: {', '.join(hits)}. "
                         "Remove them from the cards, memories or files they sit in, then export again.")
    manifest = {"format": "amuxmove", "version": VERSION, "exported_at": int(time.time()), "source": api.base,
                "counts": {k: len(v) for k, v in data.items() if isinstance(v, list)}, "files": len(files),
                "warnings": warnings}
    with tarfile.open(out, "w:gz") as tar:
        def add(name, b):
            ti = tarfile.TarInfo(name); ti.size = len(b); ti.mtime = int(time.time()); tar.addfile(ti, io.BytesIO(b))
        add("manifest.json", json.dumps(manifest, indent=1).encode())
        add("data.json", blob.encode())
        for rel, b in files.items():
            add("files/" + rel, b)
    return manifest


def repos_of(api, workers, warnings):
    out = []
    for w in workers:
        d = (w.get("dir") or "").rstrip("/")
        if not d:
            continue
        r = api.call("GET", "/api/fs/read?path=" + urllib.parse.quote(d + "/.git/config"), ok=(200, 400, 403, 404))
        cfg = r[1].get("content") if r[0] == 200 and isinstance(r[1], dict) else None
        if not cfg:
            continue
        m = re.search(r'\[remote "origin"\][^\[]*?url\s*=\s*(\S+)', cfg)
        if not m:
            warnings.append(f"{w['name']}: {d} is a git checkout with no origin remote; its files are not moved")
            continue
        head = api.call("GET", "/api/fs/read?path=" + urllib.parse.quote(d + "/.git/HEAD"), ok=(200, 404))[1] or {}
        branch = (re.search(r"ref: refs/heads/(\S+)", head.get("content") or "") or [None, "main"])[1]
        out.append({"worker": w["name"], "dir": d, "url": m.group(1), "branch": branch})
        if os.path.isdir(d):                                  # exporting from this machine: warn about unpushed work
            st = os.popen(f"git -C {sh_quote(d)} status --porcelain 2>/dev/null | head -5").read().strip()
            ahead = os.popen(f"git -C {sh_quote(d)} rev-list --count @{{u}}..HEAD 2>/dev/null").read().strip()
            if st or (ahead and ahead != "0"):
                warnings.append(f"{w['name']}: {d} has uncommitted or unpushed changes that will not move; push them first")
    return out


def sh_quote(s):
    return "'" + s.replace("'", "'\\''") + "'"


def collect_files(api, root, files, warnings, depth=0):
    listing = api.get("/api/files?path=" + urllib.parse.quote(root)) or {}
    for e in listing.get("entries", []):
        rel = f"{root}/{e['name']}" if root else e["name"]
        if e.get("kind") == "dir" or e.get("dir"):
            if depth < 6 and e["name"] not in (".git", "node_modules", ".venv", "target", ".cache"):
                collect_files(api, rel, files, warnings, depth + 1)
            continue
        if (e.get("size") or 0) > 50 * 1024 * 1024:
            warnings.append(f"{rel}: over 50 MB, not moved")
            continue
        status, body = api.call("GET", "/api/files/download?path=" + urllib.parse.quote(rel))
        files[rel] = body if isinstance(body, (bytes, bytearray)) else json.dumps(body).encode()


def env_names(api):
    try:
        env = api.get("/api/settings/env") or {}
    except RuntimeError:
        return []
    keys = env.keys() if isinstance(env, dict) else []
    return sorted(k for k in keys if re.fullmatch(r"[A-Z][A-Z0-9_]*", k) and re.search(r"KEY|TOKEN|SECRET", k))


# ------------------------------------------------------------------ import
def read_bundle(path):
    with tarfile.open(path, "r:gz") as tar:
        names = tar.getnames()
        if "manifest.json" not in names or "data.json" not in names:
            raise SystemExit(f"{path} is not an amux move bundle")
        manifest = json.load(tar.extractfile("manifest.json"))
        if manifest.get("format") != "amuxmove" or manifest.get("version", 0) > VERSION:
            raise SystemExit(f"{path} has an unsupported format or a newer version ({manifest.get('version')})")
        data = json.load(tar.extractfile("data.json"))
        files = {}
        for m in tar.getmembers():
            if m.isfile() and m.name.startswith("files/"):
                rel = m.name[len("files/"):]
                if rel.startswith("/") or ".." in rel.split("/"):
                    raise SystemExit(f"refusing a bundle file outside home: {rel}")
                files[rel] = tar.extractfile(m).read()
    if SECRETISH.search(json.dumps(data)):
        raise SystemExit("refusing a bundle that contains secret-shaped values")
    return manifest, data, files


def plan_import(api, data, files):
    """What would change, per kind: (item label, action, reason). Reads the target, writes nothing."""
    have_workers = {w["name"] for w in rows(api.get("/api/sessions"), "sessions")}
    have_sched = {(s.get("title"), s.get("session"), s.get("schedule_expr")) for s in rows(api.get("/api/schedules"), "schedules")}
    have_mem = {(json.dumps(m.get("scope"), sort_keys=True), m.get("name")) for m in rows(api.get("/api/memories"), "items")}
    have_cards = {(c.get("title"), c.get("session")) for c in rows(api.get("/api/board?all=1"), "items")}
    have_crm = {((c.get("name") or "").lower(), (c.get("email") or "").lower()) for c in rows(api.get("/api/crm/contacts"), "contacts")}
    have_skills = {s["name"] for s in rows(api.get("/api/skills"), "skills")}
    have_saved = {(m.get("session"), m.get("text")) for m in rows(api.get("/api/saved-messages"), "items")}
    have_journal = {(j.get("date"), j.get("text")) for j in rows(api.get("/api/journal"), "items")}
    plan = []
    add = lambda kind, label, exists: plan.append((kind, label, "skip" if exists else "create",
                                                   "already on the target" if exists else ""))
    for w in data.get("workers", []):
        add("worker", w["name"], w["name"] in have_workers)
    for g in data.get("groups", []):
        plan.append(("group", g["name"], "update", "group settings are set by name"))
    for s in data.get("skills", []):
        add("skill", s["name"], s["name"] in have_skills)
    for m in data.get("memories", []):
        add("memory", m["name"], (json.dumps(m.get("scope"), sort_keys=True), m["name"]) in have_mem)
    for s in data.get("schedules", []):
        add("schedule", s["title"], (s.get("title"), s.get("session"), s.get("schedule_expr")) in have_sched)
    for c in data.get("board", []):
        add("card", c["title"], (c["title"], c.get("session")) in have_cards)
    for c in data.get("crm", []):
        add("contact", c["name"], ((c.get("name") or "").lower(), (c.get("email") or "").lower()) in have_crm)
    for m in data.get("saved_messages", []):
        add("saved message", (m.get("label") or m["text"][:40]), (m.get("session"), m.get("text")) in have_saved)
    for j in data.get("journal", []):
        add("journal entry", j.get("date") or "", (j.get("date"), j.get("text")) in have_journal)
    for rel in files:
        plan.append(("file", rel, "write", "overwrites a file at the same path"))
    for r in data.get("repos", []):
        plan.append(("repository", f"{r['url']} -> {r['dir']}", "clone", "skipped if the folder already exists"))
    return plan


def apply_import(api, data, files, plan):
    todo = {(k, label) for k, label, action, _ in plan if action != "skip"}
    result = []
    def done(kind, label, outcome):
        result.append((kind, label, outcome))
    if data.get("config"):
        api.call("PUT", "/api/config/apply", data["config"])
        done("config", "skins, board columns, worker declarations", "applied")
    for w in data.get("workers", []):
        if ("worker", w["name"]) in todo:
            body = {k: v for k, v in w.items() if v not in (None, "", [])}
            body["start"] = False                                     # declared, never started by an import
            status, _ = api.call("POST", "/api/sessions", body)
            done("worker", w["name"], "exists" if status == 409 else "declared, not started")
    for g in data.get("groups", []):
        cfg = {k: v for k, v in g.items() if k != "name" and v not in (None, "")}
        if cfg:
            api.call("PATCH", f"/api/groups/{urllib.parse.quote(g['name'])}/config", cfg)
            done("group", g["name"], "settings applied")
    for s in data.get("skills", []):
        if ("skill", s["name"]) in todo and s.get("content"):
            api.call("POST", f"/api/skills/{urllib.parse.quote(s['name'])}", {"content": s["content"]})
            done("skill", s["name"], "created")
    for m in data.get("memories", []):
        if ("memory", m["name"]) in todo:
            body = {k: v for k, v in m.items() if v is not None}
            body["provenance"] = {"kind": "imported", "source": "amux move bundle"}
            status, _ = api.call("POST", "/api/memories", body)
            done("memory", m["name"], "exists" if status == 409 else "created")
    for s in data.get("schedules", []):
        if ("schedule", s["title"]) in todo:
            body = dict(s, enabled=0)                                 # both machines must not fire the same schedule
            api.call("POST", "/api/schedules", body)
            done("schedule", s["title"], "created disabled: enable it once the old machine stops")
    id_map = {}
    for c in data.get("board", []):
        if ("card", c["title"]) not in todo:
            continue
        body = {k: v for k, v in c.items() if k in BOARD_FIELDS and v is not None}
        body["status"] = "todo" if body.get("status") in ("done", "verified") else body.get("status", "todo")
        status, resp = api.call("POST", "/api/board", body)
        new = (resp or {}).get("id") if isinstance(resp, dict) else None
        if status == 200 and isinstance(resp, dict) and not resp.get("card_created", True):
            done("card", c["title"], f"folded into existing card {new} by the target's intake")
            continue
        if new and c.get("old_id"):
            id_map[c["old_id"]] = new
        if new and c.get("status") in ("done", "verified"):
            api.call("PATCH", f"/api/board/{new}", {"status": "done", "evidence": "none: imported from an amux move bundle"},
                     ok=(200, 201, 409))
        if new and c.get("archived"):
            api.call("POST", f"/api/board/{new}/archive", {}, ok=(200, 204, 409))
        done("card", c["title"], f"created as {new}")
    for c in data.get("board", []):
        deps = [id_map.get(d) for d in (c.get("depends_on") or []) if id_map.get(d)]
        if deps and id_map.get(c.get("old_id")):
            api.call("PATCH", f"/api/board/{id_map[c['old_id']]}", {"depends_on": deps}, ok=(200, 409))
    for c in data.get("crm", []):
        if ("contact", c["name"]) in todo:
            body = {k: v for k, v in c.items() if k != "interactions" and v not in (None, "", [])}
            _, resp = api.call("POST", "/api/crm/contacts", body)
            cid = (resp or {}).get("id")
            for i in c.get("interactions") or []:
                api.call("POST", f"/api/crm/contacts/{cid}/interactions",
                         {k: i.get(k) for k in ("date", "type", "notes", "follow_up_date", "follow_up_note") if i.get(k)})
            done("contact", c["name"], "created")
    for m in data.get("saved_messages", []):
        if ("saved message", m.get("label") or m["text"][:40]) in todo:
            api.call("POST", "/api/saved-messages", {k: v for k, v in m.items() if v})
            done("saved message", m.get("label") or m["text"][:40], "created")
    for j in data.get("journal", []):
        if ("journal entry", j.get("date") or "") in todo:
            api.call("POST", "/api/journal", {k: v for k, v in j.items() if v not in (None, "")})
            done("journal entry", j.get("date"), "created")
    for rel, b in files.items():
        api.call("POST", "/api/files/upload?path=" + urllib.parse.quote(rel), raw=b)
        done("file", rel, "written")
    for r in data.get("repos", []):
        done("repository", r["url"], clone_repo(api, r))
    return result


def clone_repo(api, r):
    """Clone on the target through its terminal; skipped when the folder exists."""
    exists = api.call("GET", "/api/fs/list?path=" + urllib.parse.quote(r["dir"]), ok=(200, 400, 404))[0] == 200
    if exists:
        return "folder already exists, not cloned"
    _, t = api.call("POST", "/api/terminal/create", {"cols": 200, "rows": 20})
    tid = (t or {}).get("id")
    if not tid:
        return "could not open a terminal on the target to clone"
    cmd = f"git clone --branch {sh_quote(r['branch'])} {sh_quote(r['url'])} {sh_quote(r['dir'])}; exit\r"
    api.call("POST", f"/api/terminal/{tid}/input", {"data": base64.b64encode(cmd.encode()).decode()})
    out = b""
    for _ in range(40):
        _, d = api.call("GET", f"/api/terminal/{tid}/output?wait=10")
        if isinstance(d, dict) and d.get("data"):
            out += base64.b64decode(d["data"])
        if isinstance(d, dict) and d.get("alive") is False:
            break
    ok = api.call("GET", "/api/fs/list?path=" + urllib.parse.quote(r["dir"] + "/.git"), ok=(200, 400, 404))[0] == 200
    return "cloned" if ok else "clone failed (a private repository needs the target's GitHub sign-in first)"


def print_plan(plan, manifest=None):
    if manifest:
        print(f"bundle from {manifest['source']}, exported {time.strftime('%Y-%m-%d %H:%M', time.localtime(manifest['exported_at']))}")
        for w in manifest.get("warnings", []):
            print(f"  warning: {w}")
    counts = {}
    for kind, label, action, reason in plan:
        counts.setdefault(kind, {}).setdefault(action, 0)
        counts[kind][action] += 1
    for kind, c in counts.items():
        print(f"  {kind}: " + ", ".join(f"{n} {a}" for a, n in sorted(c.items())))
    creates = sum(1 for p in plan if p[2] != "skip")
    print(f"{creates} change(s), {len(plan) - creates} already present" if plan else "nothing to import")


# ------------------------------------------------------------------ cloud
def cloud_token():
    try:
        with open(TOKEN_FILE) as f:
            return f.read().strip()
    except FileNotFoundError:
        raise SystemExit("not signed in to amux Cloud: run `amux cloud login`")


def cloud_api():
    return Api(CLOUD, {"Authorization": f"Bearer {cloud_token()}"})


def cloud_login():
    api = Api(CLOUD)
    _, d = api.call("POST", "/api/device/start", {})
    print(f"Open {d['verify_url']} and enter the code {d['user_code']}")
    try:
        webbrowser.open(d["verify_url"])
    except Exception:
        pass
    deadline = time.time() + d.get("expires_in", 600)
    while time.time() < deadline:
        time.sleep(d.get("interval", 3))
        status, r = api.call("POST", "/api/device/poll", {"device_code": d["device_code"]}, ok=(200, 202, 403, 410))
        if status == 200 and r.get("token"):
            os.makedirs(os.path.dirname(TOKEN_FILE), exist_ok=True)
            with open(TOKEN_FILE, "w") as f:
                f.write(r["token"])
            os.chmod(TOKEN_FILE, 0o600)
            print(f"signed in to amux Cloud as {r.get('email')}")
            return
        if status in (403, 410):
            raise SystemExit("sign-in was denied or the code expired")
    raise SystemExit("sign-in timed out")


def cloud_server_api(slug_or_id):
    """An Api pointed at a Cloud server, signed in through its door with a fresh one-minute link."""
    me = cloud_api().get("/api/me")
    srv = next((s for s in me.get("servers", []) if s["slug"] == slug_or_id or s["id"] == slug_or_id), None)
    if not srv:
        raise SystemExit(f"no server {slug_or_id} on this account (have: {', '.join(s['slug'] for s in me.get('servers', [])) or 'none'})")
    entry = cloud_api().get(f"/api/go-url/{srv['id']}")["url"]

    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *a, **k):
            return None
    opener = urllib.request.build_opener(NoRedirect)
    try:
        opener.open(entry, timeout=30)
        raise SystemExit("the server's door did not answer with a sign-in")
    except urllib.error.HTTPError as e:
        cookies = [c.split(";")[0] for c in e.headers.get_all("Set-Cookie") or []]
    door = [c for c in cookies if c.startswith("amux_door=")]
    if not door:
        raise SystemExit("the server's door refused the sign-in link")
    return Api(srv["url"], {"Cookie": "; ".join(door)}), srv


def wait_ready(timeout=900):
    deadline = time.time() + timeout
    while time.time() < deadline:
        me = cloud_api().get("/api/me")
        ready = [s for s in me.get("servers", []) if s["status"] == "ready"]
        if ready:
            return ready[0]
        time.sleep(10)
    raise SystemExit("the server was not ready within 15 minutes; check cloud.amux.io")


def cloud_move(plan, file_roots):
    local = Api(os.environ.get("AMUX_API", "https://localhost:8824"))
    bundle = os.path.expanduser(f"~/amux-move-{time.strftime('%Y%m%d-%H%M%S')}.amuxmove")
    m = export(local, bundle, file_roots)
    print(f"exported {sum(m['counts'].values())} items and {m['files']} files to {bundle}")
    me = cloud_api().get("/api/me")
    if not [s for s in me.get("servers", []) if s["status"] in ("ready", "provisioning")]:
        url = f"{CLOUD}/start?plan={urllib.parse.quote(plan)}&source=cli"
        print(f"Finish checkout in your browser: {url}")
        webbrowser.open(url)
    srv = wait_ready()
    target, _ = cloud_server_api(srv["id"])
    _, data, files = read_bundle(bundle)
    p = plan_import(target, data, files)
    print_plan(p)
    for kind, label, outcome in apply_import(target, data, files, p):
        print(f"  {kind}: {label}: {outcome}")
    print(f"done. Your server: {srv['url']}  (nothing on this machine was changed)")


def cloud_pull(slug, file_roots):
    source, srv = cloud_server_api(slug)
    bundle = os.path.expanduser(f"~/amux-{srv['slug']}-{time.strftime('%Y%m%d-%H%M%S')}.amuxmove")
    m = export(source, bundle, file_roots)
    print(f"exported {sum(m['counts'].values())} items and {m['files']} files from {srv['url']} to {bundle}")
    local = Api(os.environ.get("AMUX_API", "https://localhost:8824"))
    _, data, files = read_bundle(bundle)
    p = plan_import(local, data, files)
    print_plan(p)
    for kind, label, outcome in apply_import(local, data, files, p):
        print(f"  {kind}: {label}: {outcome}")


# ------------------------------------------------------------------ YAML
def load_yaml(path):
    import yaml
    with open(path) as f:
        doc = yaml.safe_load(f) or {}
    if doc.get("version") != 1:
        raise SystemExit("amux.yaml needs version: 1")
    if SECRETISH.search(json.dumps(doc)):
        raise SystemExit("amux.yaml contains a secret-shaped value; name it under env: with from: prompt instead")
    return doc


def yaml_to_bundle(doc, base_dir):
    """The YAML is a hand-written bundle: same importer, same idempotence."""
    data = {"workers": [], "schedules": [], "skills": [], "repos": [], "groups": [{"name": g} for g in doc.get("groups") or []]}
    for w in doc.get("workers") or []:
        data["workers"].append({"name": w["name"], "dir": w.get("dir"), "model": w.get("model"), "desc": w.get("desc"),
                                "tags": w.get("groups") or w.get("tags"), "provider": w.get("provider")})
    for s in doc.get("schedules") or []:
        data["schedules"].append({"title": s["title"], "session": s.get("worker"), "schedule_expr": s.get("expr"),
                                  "kind": s.get("kind", "tmux"), "command": s.get("command") or s["title"]})
    for p in doc.get("skills") or []:
        fp = os.path.join(base_dir, p)
        data["skills"].append({"name": os.path.splitext(os.path.basename(fp))[0], "content": open(fp).read()})
    for r in doc.get("repos") or []:
        data["repos"].append({"worker": None, "dir": r["dir"], "url": r["url"], "branch": r.get("branch", "main")})
    files = {}
    for f in doc.get("files") or []:
        src = os.path.join(base_dir, f["from"])
        to = f["to"].lstrip("~/").strip("/")
        for dp, _, names in os.walk(src) if os.path.isdir(src) else [(os.path.dirname(src), [], [os.path.basename(src)])]:
            for n in names:
                full = os.path.join(dp, n)
                rel = os.path.relpath(full, src) if os.path.isdir(src) else n
                files[f"{to}/{rel}"] = open(full, "rb").read()
    if doc.get("skin"):
        data["config"] = {"skins": {"global": doc["skin"]}}
    return data, files


def cloud_apply(path, confirm_billing):
    doc = load_yaml(path)
    want = (doc.get("cloud") or {}).get("plan", "solo")
    me = cloud_api().get("/api/me")
    live = [s for s in me.get("servers", []) if s["status"] in ("ready", "provisioning")]
    name = (doc.get("cloud") or {}).get("server")
    target = next((s for s in live if s["slug"] == name), None) if name else (live[0] if len(live) == 1 else None)
    if not target:
        if live and not confirm_billing:
            raise SystemExit(f"this would add a new {want} server to a paid account; run again with --confirm-billing")
        if live:
            ans = input(f"Add a {want} server to your account, billed monthly? Type yes: ")
            if ans.strip().lower() != "yes":
                raise SystemExit("nothing changed")
        url = f"{CLOUD}/start?plan={urllib.parse.quote(want)}&source=yaml"
        print(f"Finish checkout in your browser: {url}")
        webbrowser.open(url)
        target = wait_ready()
    api, srv = cloud_server_api(target["id"])
    data, files = yaml_to_bundle(doc, os.path.dirname(os.path.abspath(path)))
    for k, spec in (doc.get("env") or {}).items():
        src = (spec or {}).get("from", "prompt")
        val = os.environ.get(k) if src == "local-env" else (getpass.getpass(f"{k}: ") if src == "prompt" else None)
        if val:
            api.call("PATCH", "/api/settings/env", {k: val})
            print(f"  env: {k}: set on the server only")
    p = plan_import(api, data, files)
    print_plan(p)
    for kind, label, outcome in apply_import(api, data, files, p):
        print(f"  {kind}: {label}: {outcome}")
    for w in doc.get("workers") or []:
        if w.get("start"):
            api.call("POST", f"/api/sessions/{urllib.parse.quote(w['name'])}/start", {}, ok=(200, 202, 204, 409))
            print(f"  worker: {w['name']}: started (start: true)")
    for m in doc.get("members") or []:
        scope = m.get("scope", "global")
        body = {"email": m["email"], "scope_level": "global"} if scope == "global" else \
            {"email": m["email"], "scope_level": scope.split(":")[0], "scope_name": scope.split(":", 1)[1]}
        api.call("POST", "/api/org/invites", body, ok=(200, 201, 409))
        print(f"  member: {m['email']}: invited")
    print(f"done: {srv['url']}")


# ------------------------------------------------------------------ main
def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("export"); e.add_argument("--api", default=os.environ.get("AMUX_API", "https://localhost:8824"))
    e.add_argument("--out", required=True); e.add_argument("--files", nargs="*", default=[])
    i = sub.add_parser("import"); i.add_argument("bundle"); i.add_argument("--api", default=os.environ.get("AMUX_API", "https://localhost:8824"))
    i.add_argument("--apply", action="store_true")
    sub.add_parser("cloud-login")
    mv = sub.add_parser("cloud-move"); mv.add_argument("--plan", default="solo"); mv.add_argument("--files", nargs="*", default=[])
    pl = sub.add_parser("cloud-pull"); pl.add_argument("slug"); pl.add_argument("--files", nargs="*", default=[])
    ya = sub.add_parser("cloud-apply"); ya.add_argument("yaml"); ya.add_argument("--confirm-billing", action="store_true")
    a = ap.parse_args()
    if a.cmd == "export":
        m = export(Api(a.api), a.out, a.files)
        print(json.dumps(m, indent=1))
    elif a.cmd == "import":
        api = Api(a.api)
        manifest, data, files = read_bundle(a.bundle)
        p = plan_import(api, data, files)
        print_plan(p, manifest)
        if a.apply:
            for kind, label, outcome in apply_import(api, data, files, p):
                print(f"  {kind}: {label}: {outcome}")
        else:
            print("dry run: nothing changed. Run again with --apply to import.")
    elif a.cmd == "cloud-login":
        cloud_login()
    elif a.cmd == "cloud-move":
        cloud_move(a.plan, a.files)
    elif a.cmd == "cloud-pull":
        cloud_pull(a.slug, a.files)
    elif a.cmd == "cloud-apply":
        cloud_apply(a.yaml, a.confirm_billing)


if __name__ == "__main__":
    main()
