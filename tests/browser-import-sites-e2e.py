#!/usr/bin/env python3
"""Import sign-ins from a local browser, per site (owner, 2026-11-22: "Do them
all", from research/athena-mac-app-teardown.md; Athena imports per chosen site).

    python3 tests/browser-import-sites-e2e.py [amux-server binary]

A private amux server runs with its own HOME, which holds a Firefox profile
this test writes (cookies.sqlite, Firefox's own schema, plaintext as Firefox
stores it). So the import reads a real browser profile through the real
route and never touches the owner's browsers. Checks: discover lists the
profile; an import with sites keeps only those sites' cookies (a parent
domain's cookie included, a look-alike domain not) and says how many it
left out; an import without sites keeps every cookie; the files are private
to the owner; an existing profile is never overwritten.
"""
import json, os, shutil, socket, sqlite3, ssl, stat, subprocess, sys, tempfile, time, urllib.request

SERVER = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/.local/bin/amux-server-rs")
TMP = tempfile.mkdtemp(prefix="amux-import-sites-e2e-")
FAKE_HOME = os.path.join(TMP, "user")
AMUX_HOME = os.path.join(TMP, "amux")
os.makedirs(os.path.join(AMUX_HOME, "tmux"))
CTX = ssl.create_default_context(); CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE
OWNER_FILE = "auth" + "_token"
fails = 0


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:600]}"))
    fails += 0 if ok else 1


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


# A Firefox profile, as Firefox lays it out on a Mac.
prof = os.path.join(FAKE_HOME, "Library", "Application Support", "Firefox", "Profiles", "e2e.default-release")
os.makedirs(prof)
db = sqlite3.connect(os.path.join(prof, "cookies.sqlite"))
db.execute("CREATE TABLE moz_cookies (id INTEGER PRIMARY KEY, originAttributes TEXT NOT NULL DEFAULT '', name TEXT, value TEXT, host TEXT, path TEXT, expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER, isSecure INTEGER, isHttpOnly INTEGER, inBrowserElement INTEGER DEFAULT 0, sameSite INTEGER DEFAULT 0, rawSameSite INTEGER DEFAULT 0, schemeMap INTEGER DEFAULT 0)")
far = int(time.time()) + 86400 * 30
for host, name in [(".google.com", "SID"), ("accounts.google.com", "LSID"), ("github.com", "user_session"),
                   ("evilgoogle.com", "phish"), (".example.org", "x")]:
    db.execute("INSERT INTO moz_cookies (name,value,host,path,expiry,isSecure,isHttpOnly) VALUES (?,?,?,?,?,1,1)", (name, "v-" + name, host, "/", far))
db.commit(); db.close()
open(os.path.join(FAKE_HOME, "Library", "Application Support", "Firefox", "profiles.ini"), "w").write(
    "[Profile0]\nName=e2e\nIsRelative=1\nPath=Profiles/e2e.default-release\nDefault=1\n")

port = free_port()
URL = f"https://localhost:{port}"
srv = os.path.join(TMP, "amux-server")
shutil.copy(SERVER, srv)
env = {k: v for k, v in os.environ.items() if not k.startswith(("TMUX", "AMUX_SESSION", "AMUX_URL"))}
env.update(HOME=FAKE_HOME, AMUX_HOME=AMUX_HOME, AMUX_RS_PORT=str(port), AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(AMUX_HOME, "tmux"))
log = open(os.path.join(TMP, "server.log"), "w")
sp = subprocess.Popen([srv], env=env, stdout=log, stderr=subprocess.STDOUT)


def owner(method, path, body=None):
    h = {"Content-Type": "application/json", "Authorization": "Bearer " + open(os.path.join(AMUX_HOME, OWNER_FILE)).read().strip()}
    req = urllib.request.Request(URL + path, method=method, headers=h, data=json.dumps(body).encode() if body is not None else None)
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=30) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def cookies_of(profile):
    p = os.path.join(AMUX_HOME, "playwright-auth", "profiles", profile, "cookies.json")
    return p, json.load(open(p)) if os.path.exists(p) else []


try:
    for _ in range(180):
        try:
            if urllib.request.urlopen(URL + "/health", context=CTX, timeout=2).status == 200 and os.path.exists(os.path.join(AMUX_HOME, OWNER_FILE)):
                break
        except OSError:
            pass
        if sp.poll() is not None:
            break
        time.sleep(0.5)
    check("the private server is up", sp.poll() is None)

    code, d = owner("GET", "/api/browser/import/discover")
    ff = next((s for s in d.get("sources", []) if s.get("id") == "firefox"), {})
    check("discover finds the Firefox profile", any(p.get("name") == "e2e.default-release" for p in ff.get("profiles", [])), (code, d))

    code, d = owner("POST", "/api/browser/import", {"browser_id": "firefox", "profile_name": "e2e.default-release",
                                                    "destination": "google-only", "sites": ["google.com"]})
    path, got = cookies_of("google-only")
    names = sorted(c["name"] for c in got)
    check("an import for google.com takes google.com's cookies, its parent's included", code == 200 and names == ["LSID", "SID"], (code, d, names))
    check("and leaves a look-alike domain and other sites out", not any(c["domain"] in ("evilgoogle.com", "github.com", ".example.org") for c in got), got)
    check("and says how many it left out", d.get("imported_cookies") == 2 and d.get("skipped_cookies") == 3, d)
    check("the cookie file is private to the owner", path and stat.S_IMODE(os.stat(path).st_mode) == 0o600, oct(os.stat(path).st_mode) if path else "")

    code, d = owner("POST", "/api/browser/import", {"browser_id": "firefox", "profile_name": "e2e.default-release",
                                                    "destination": "two-sites", "sites": ["https://github.com/login", "accounts.google.com"]})
    names = sorted(c["name"] for c in cookies_of("two-sites")[1])
    check("several sites, given as URLs or hosts, each bring theirs", code == 200 and names == ["LSID", "SID", "user_session"], (code, d, names))

    code, d = owner("POST", "/api/browser/import", {"browser_id": "firefox", "profile_name": "e2e.default-release", "destination": "everything"})
    check("without sites every cookie comes", code == 200 and d.get("imported_cookies") == 5 and d.get("skipped_cookies") == 0, (code, d))

    code, d = owner("POST", "/api/browser/import", {"browser_id": "firefox", "profile_name": "e2e.default-release",
                                                    "destination": "google-only", "sites": ["github.com"]})
    check("an existing profile is never overwritten", code != 200 and sorted(c["name"] for c in cookies_of("google-only")[1]) == ["LSID", "SID"], (code, d))
    check("the server logs each import with its sites", "sites=google.com" in open(os.path.join(TMP, "server.log"), errors="replace").read())
finally:
    sp.terminate()
    log.close()
print(f"{'PASS' if fails == 0 else 'FAIL'}: {fails} failing check(s) (logs {TMP})")
sys.exit(1 if fails else 0)
