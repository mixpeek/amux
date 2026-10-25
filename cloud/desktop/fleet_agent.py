#!/usr/bin/env python3
"""amux Cloud fleet agent: runs on every customer server (docs/design/cloud-self-service.md, 3.1, J7, J14).

Everything it does is outbound, so it keeps working when the customer turns on
tailnet-only access.

  first boot   make this host's key and CSR, trade the CSR for a Cloudflare Origin
               CA certificate in the first heartbeat, run bootstrap.sh in fleet mode,
               write the backup config, take the first backup, then report serving
  every minute heartbeat: version, last backup, disk; apply what the reply says
               (backup credentials once, a release to install, tailnet-only)
  backups      litestream streams amux.db to this server's own R2 bucket every
               60 seconds; restic snapshots the home directory nightly
  releases     download, verify with `ssh-keygen -Y verify` against the pinned
               release key, swap by rename, health-check 5 minutes, roll back alone

Standard library plus tools already on the image (openssl, ssh-keygen,
litestream, restic, systemctl). State lives in /var/lib/amux-fleet.

  fleet_agent.py run        the service loop
  fleet_agent.py once       one heartbeat, for debugging
"""
import hashlib, json, os, shutil, subprocess, sys, time, urllib.error, urllib.request

FLEET_ENV = os.environ.get("FLEET_ENV", "/etc/amux/fleet.env")
STATE = os.environ.get("FLEET_STATE", "/var/lib/amux-fleet")
ETC = os.environ.get("FLEET_ETC", "/etc/amux")
BOOTSTRAP = os.environ.get("FLEET_BOOTSTRAP", "/opt/amux/bootstrap.sh")   # installed by AMUX_IMAGE_BUILD
# bootstrap.sh runs amux-server as the amux user's systemd user unit from ~/.local/bin.
SERVER_BIN = os.environ.get("FLEET_SERVER_BIN", "/home/amux/.local/bin/amux-server-rs")
RESTART_CMD = os.environ.get("FLEET_RESTART_CMD",
                             'runuser -u amux -- env XDG_RUNTIME_DIR=/run/user/$(id -u amux) '
                             'systemctl --user restart amux-server.service')
HEALTH = os.environ.get("FLEET_HEALTH", "https://127.0.0.1:8824/health")
RELEASE_SIGNERS = os.environ.get("FLEET_RELEASE_SIGNERS", "/etc/amux/release_signers")
INTERVAL = float(os.environ.get("FLEET_INTERVAL", "60"))
HEALTH_SOAK = float(os.environ.get("FLEET_HEALTH_SOAK", "300"))


def log(**kv):
    kv["ts"] = round(time.time(), 3)
    print(json.dumps(kv, sort_keys=True), flush=True)


def env():
    out = {}
    with open(FLEET_ENV) as f:
        for line in f:
            k, _, v = line.strip().partition("=")
            if k:
                out[k] = json.loads(v) if v.startswith('"') else v
    return out


def sh(*cmd, check=True, timeout=600, **kw):
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, **kw)
    if check and r.returncode != 0:
        # The cause is at the end (set -e stops on the failing line); the start is apt and install noise.
        raise RuntimeError(f"{cmd[0]} exited {r.returncode}: {r.stderr.strip()[-600:]}")
    return r


def state(name, value=None):
    os.makedirs(STATE, exist_ok=True)
    p = os.path.join(STATE, name)
    if value is None:
        try:
            with open(p) as f:
                return json.load(f)
        except (FileNotFoundError, ValueError):
            return None
    tmp = p + ".tmp"
    with open(tmp, "w") as f:
        json.dump(value, f)
    os.replace(tmp, p)
    return value


# ------------------------------------------------------------------ control plane
def post(cfg, verb, body, timeout=30):
    url = f"{cfg['AMUX_CONTROL_URL'].rstrip('/')}/api/fleet/{cfg['AMUX_SERVER_ID']}/{verb}"
    req = urllib.request.Request(url, data=json.dumps(body).encode(), method="POST",
                                 headers={"Authorization": f"Bearer {cfg['AMUX_FLEET_SECRET']}",
                                          "Content-Type": "application/json", "User-Agent": "amux-fleet/1"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


# ------------------------------------------------------------------ certificate
def ensure_csr():
    key, csr = os.path.join(ETC, "origin.key"), os.path.join(STATE, "origin.csr")
    os.makedirs(STATE, exist_ok=True)
    if not os.path.exists(key):
        sh("openssl", "ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", key)
        os.chmod(key, 0o600)
    if not os.path.exists(csr):
        sh("openssl", "req", "-new", "-key", key, "-subj", "/CN=amux-cloud", "-out", csr)
    with open(csr) as f:
        return f.read()


def have_cert():
    return os.path.exists(os.path.join(ETC, "origin.crt")) and os.path.getsize(os.path.join(ETC, "origin.crt")) > 0


# ------------------------------------------------------------------ backups
def litestream_last():
    """End time of the newest replicated WAL segment, from `litestream generations` (last column, ISO time)."""
    cfgp = os.path.join(ETC, "litestream.yml")
    if not os.path.exists(cfgp):
        return None
    r = sh("litestream", "generations", "-config", cfgp, "/home/amux/.amux/amux.db", check=False, timeout=30)
    newest = None
    for line in r.stdout.splitlines()[1:]:
        parts = line.split()
        if parts:
            try:
                t = time.mktime(time.strptime(parts[-1].replace("Z", ""), "%Y-%m-%dT%H:%M:%S"))
                newest = max(newest or 0, t - time.timezone)
            except ValueError:
                continue
    return newest


def last_backup_at():
    """The database's last replicated write (continuous) once litestream runs, else the last file snapshot.
    The control plane needs this under an hour old before it may delete anything (plan 3.2 rule 6)."""
    files = (state("backup") or {}).get("last_ok")
    db = litestream_last()
    return db if db and files else files


def write_backup_config(creds):
    """creds: {endpoint, bucket, access_key_id, secret_access_key, restic_password} for this server only."""
    with open(os.path.join(ETC, "litestream.yml"), "w") as f:
        f.write(f"""# written by fleet_agent.py; this server's own bucket
dbs:
  - path: /home/amux/.amux/amux.db
    replicas:
      - type: s3
        endpoint: {creds['endpoint']}
        bucket: {creds['bucket']}
        path: litestream/amux.db
        access-key-id: {creds['access_key_id']}
        secret-access-key: {creds['secret_access_key']}
        sync-interval: 60s
        snapshot-interval: 24h
        retention: 168h
""")
    os.chmod(os.path.join(ETC, "litestream.yml"), 0o600)
    with open(os.path.join(ETC, "restic.env"), "w") as f:
        f.write(f"RESTIC_REPOSITORY=s3:{creds['endpoint']}/{creds['bucket']}/restic\n"
                f"AWS_ACCESS_KEY_ID={creds['access_key_id']}\nAWS_SECRET_ACCESS_KEY={creds['secret_access_key']}\n"
                f"RESTIC_PASSWORD={creds['restic_password']}\n")
    os.chmod(os.path.join(ETC, "restic.env"), 0o600)
    state("backup_configured", {"at": time.time(), "bucket": creds["bucket"]})


def run_backup(full=False):
    """Snapshot files with restic (and the database through litestream's own snapshot). Records success time."""
    envf = os.path.join(ETC, "restic.env")
    if not os.path.exists(envf):
        return False
    e = dict(os.environ)
    with open(envf) as f:
        e.update(dict(l.strip().split("=", 1) for l in f if "=" in l))
    if sh("restic", "cat", "config", env=e, check=False).returncode != 0:
        sh("restic", "init", env=e)
    sh("restic", "backup", "--exclude", ".cache", "--exclude", "node_modules", "--exclude", "target",
       "/home/amux", env=e, timeout=3600)
    if full:
        sh("litestream", "snapshots", "-config", os.path.join(ETC, "litestream.yml"), "/home/amux/.amux/amux.db", check=False)
    state("backup", {"last_ok": time.time()})
    try:
        state("backup_fingerprint", fingerprint("/home/amux/.amux/amux.db", e))
    except Exception as ex:
        log(event="backup", verdict="fleet_fingerprint_failed", error=str(ex)[:200])
    log(event="backup", verdict="fleet_backup_ok")
    return True


def fingerprint(db_path, restic_env, snapshot="latest", tree_root=None):
    """Rows per table plus a hash of the file list (path and size). The live server computes it from
    restic's listing of its newest snapshot; a restore drill computes the same from what it restored."""
    import sqlite3
    tables = {}
    if os.path.exists(db_path):
        c = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        for (t,) in c.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"):
            tables[t] = c.execute(f'SELECT COUNT(*) FROM "{t}"').fetchone()[0]
        c.close()
    entries = []
    if tree_root:
        for dp, _, names in os.walk(tree_root):
            for n in names:
                full = os.path.join(dp, n)
                entries.append(f"{os.path.relpath(full, tree_root)}\t{os.path.getsize(full)}")
    else:
        r = sh("restic", "ls", "--json", snapshot, env=restic_env, timeout=600)
        for line in r.stdout.splitlines():
            try:
                o = json.loads(line)
            except ValueError:
                continue
            if o.get("type") == "file":
                entries.append(f"{o['path'].lstrip('/').removeprefix('home/amux/')}\t{o.get('size', 0)}")
    entries.sort()
    return {"tables": tables, "files": len(entries),
            "files_sha256": hashlib.sha256("\n".join(entries).encode()).hexdigest()}


def restic_env():
    e = dict(os.environ)
    with open(os.path.join(ETC, "restic.env")) as f:
        e.update(dict(l.strip().split("=", 1) for l in f if "=" in l))
    return e


def run_drill(cfg):
    """AMUX_MODE=drill: restore the live server's backups here, fingerprint them, report, done."""
    reply = post(cfg, "heartbeat", {})
    creds = reply.get("restore_from")
    if not creds:
        return False
    write_backup_config(creds)
    work = os.path.join(STATE, "drill")
    os.makedirs(work, exist_ok=True)
    db = os.path.join(work, "amux.db")
    sh("litestream", "restore", "-config", os.path.join(ETC, "litestream.yml"), "-o", db, "/home/amux/.amux/amux.db",
       timeout=1800)
    sh("restic", "restore", "latest", "--target", os.path.join(work, "files"), env=restic_env(), timeout=3600)
    root = os.path.join(work, "files", "home", "amux")
    got = fingerprint(db, None, tree_root=root if os.path.isdir(root) else os.path.join(work, "files"))
    post(cfg, "heartbeat", {"drill_result": got})
    log(event="drill", verdict="fleet_drill_reported", tables=len(got["tables"]), files=got["files"])
    return True


# ------------------------------------------------------------------ releases
def server_version():
    try:
        import ssl
        ctx = ssl.create_default_context(); ctx.check_hostname, ctx.verify_mode = False, ssl.CERT_NONE
        with urllib.request.urlopen(HEALTH, timeout=5, context=ctx) as r:
            d = json.load(r)
        return d.get("version") or d.get("commit")
    except Exception:
        return None


def healthy_for(seconds):
    deadline = time.time() + seconds
    while time.time() < deadline:
        if server_version() is None:
            return False
        time.sleep(min(15, max(1, seconds / 20)))
    return True


def restart_server():
    sh("sh", "-c", RESTART_CMD)


def install_release(rel):
    """rel: {version, url, sha256, sig_url}. Verified before anything changes; old binary kept for rollback."""
    stage = os.path.join(STATE, "release")
    os.makedirs(stage, exist_ok=True)
    binp, sigp = os.path.join(stage, "amux-server.new"), os.path.join(stage, "amux-server.new.sig")
    urllib.request.urlretrieve(rel["url"], binp)
    urllib.request.urlretrieve(rel["sig_url"], sigp)
    with open(binp, "rb") as f:
        digest = hashlib.sha256(f.read()).hexdigest()
    if digest != rel["sha256"]:
        raise RuntimeError(f"release {rel['version']}: sha256 {digest} does not match {rel['sha256']}")
    with open(binp, "rb") as data:
        r = subprocess.run(["ssh-keygen", "-Y", "verify", "-f", RELEASE_SIGNERS, "-I", "release@amux.io",
                            "-n", "amux-release", "-s", sigp], stdin=data, capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"release {rel['version']}: signature did not verify: {r.stderr.strip()[:200]}")
    os.chmod(binp, 0o755)
    prev = SERVER_BIN + ".prev"
    shutil.copy2(SERVER_BIN, prev)                      # rollback target
    os.replace(binp, SERVER_BIN)                        # rename: a running process keeps its old image
    restart_server()
    if healthy_for(HEALTH_SOAK):
        state("release_result", {"version": rel["version"], "result": "installed", "at": time.time()})
        log(event="release", version=rel["version"], verdict="fleet_release_installed")
        return "installed"
    os.replace(prev, SERVER_BIN)
    restart_server()
    state("release_result", {"version": rel["version"], "result": "rolled_back", "at": time.time()})
    log(event="release", version=rel["version"], verdict="fleet_release_rolled_back")
    return "rolled_back"


# ------------------------------------------------------------------ the loop
def first_boot(cfg):
    """Idempotent: each step records itself in STATE and is skipped once done."""
    tls_mode = (state("tls") or {}).get("mode", "origin")
    if not have_cert() and tls_mode == "origin":
        reply = post(cfg, "heartbeat", {"version": None, "csr": ensure_csr(), "serving": False})
        if reply.get("origin_certificate"):
            with open(os.path.join(ETC, "origin.crt"), "w") as f:
                f.write(reply["origin_certificate"])
            log(event="first_boot", step="certificate", verdict="fleet_cert_ok")
        elif reply.get("tls_mode") == "acme":
            # The control plane could not issue an Origin CA certificate and made this host DNS-only:
            # Caddy gets its own Let's Encrypt certificate instead.
            tls_mode = "acme"
            state("tls", {"mode": "acme", "at": time.time()})
            log(event="first_boot", step="certificate", verdict="fleet_tls_acme_fallback")
        apply_reply(cfg, reply)
        if not have_cert() and tls_mode == "origin":
            return False
    if not state("bootstrapped"):
        sh("bash", BOOTSTRAP, env=dict(os.environ, AMUX_SKIP_PACKAGES="1", AMUX_TLS_MODE=tls_mode), timeout=1800)
        state("bootstrapped", {"at": time.time()})
        log(event="first_boot", step="bootstrap", verdict="fleet_bootstrap_ok")
    if not state("backup_configured"):
        apply_reply(cfg, post(cfg, "heartbeat", {"version": server_version(), "serving": False, "need_backup_creds": True}))
        if not state("backup_configured"):
            return False
        sh("systemctl", "enable", "--now", "litestream.service", check=False)
    if not last_backup_at():
        run_backup(full=True)
    return True


def apply_reply(cfg, reply):
    if reply.get("backup_credentials") and not state("backup_configured"):
        write_backup_config(reply["backup_credentials"])
    if reply.get("release") and reply["release"].get("version") != server_version():
        try:
            outcome = install_release(reply["release"])
        except Exception as e:
            outcome = f"refused: {e}"
            log(event="release", verdict="fleet_release_refused", error=str(e)[:200])
        state("release_report", {"version": reply["release"].get("version"), "outcome": outcome})
    if "stop_workers" in (reply.get("commands") or []) and not (state("workers_stopped") or {}).get("on"):
        # M17 isolation: pause every worker; the customer restarts them after a successful appeal.
        sh("runuser", "-u", "amux", "--", "bash", "-lc", "amux stop-all", check=False, timeout=120)
        state("workers_stopped", {"on": True, "at": time.time()})
        log(event="abuse", verdict="fleet_workers_stopped")
    elif "stop_workers" not in (reply.get("commands") or []) and (state("workers_stopped") or {}).get("on"):
        state("workers_stopped", {"on": False, "at": time.time()})
    want_tailnet_only = bool(reply.get("tailnet_only"))
    if (state("tailnet_only") or {}).get("on") != want_tailnet_only:
        apply_tailnet_only(want_tailnet_only)


def tailnet_url():
    r = sh("tailscale", "status", "--json", check=False, timeout=10) if shutil.which("tailscale") else None
    try:
        st = json.loads(r.stdout) if r and r.returncode == 0 else {}
    except ValueError:
        return None
    dns = ((st.get("Self") or {}).get("DNSName") or "").rstrip(".")
    return f"https://{dns}" if st.get("BackendState") == "Running" and dns.endswith(".ts.net") else None


def apply_tailnet_only(on):
    """Plan 8.5: public 443 closed, tailnet open. Refuses to close the public door with no tailnet to use."""
    if on and not tailnet_url():
        log(event="tailnet_only", verdict="fleet_tailnet_only_refused", why="server is not on a tailnet")
        return False
    if on:
        sh("ufw", "allow", "in", "on", "tailscale0", check=False)
        sh("ufw", "delete", "allow", "443/tcp", check=False)
        sh("ufw", "delete", "allow", "80/tcp", check=False)
    else:
        sh("ufw", "allow", "443/tcp", check=False)
    state("tailnet_only", {"on": on, "at": time.time()})
    log(event="tailnet_only", on=on, verdict="fleet_tailnet_only_applied")
    return True


def abuse_signals():
    """M17 inputs: names of running processes and the six-hour average CPU, from load samples kept here."""
    try:
        load1 = os.getloadavg()[0] / (os.cpu_count() or 1) * 100
    except OSError:
        return None
    samples = [x for x in (state("cpu_samples") or []) if x[0] > time.time() - 6 * 3600] + [[time.time(), load1]]
    state("cpu_samples", samples[-400:])
    procs = sh("ps", "-eo", "comm=", check=False, timeout=10).stdout.split()
    full_window = samples and samples[0][0] < time.time() - 5.5 * 3600
    return {"cpu_pct_6h": round(sum(x[1] for x in samples) / len(samples), 1) if full_window else None,
            "processes": sorted(set(os.path.basename(p) for p in procs))[:400]}


def usage():
    """M6, M7, M10 inputs, counts only: is any model connected, how many messages people have sent to
    workers (a running total), and CPU and memory use now. Never message content."""
    import sqlite3, ssl
    out = {"model_connected": None, "turns_total": None, "cpu_pct": None, "mem_pct": None}
    try:
        ctx = ssl.create_default_context(); ctx.check_hostname, ctx.verify_mode = False, ssl.CERT_NONE
        with urllib.request.urlopen(HEALTH.replace("/health", "/api/identity"), timeout=5, context=ctx) as r:
            ident = json.load(r)
        ma = ident.get("model_auth") or {}
        out["model_connected"] = bool(ident.get("has_api_key")) or any(
            any(bool(v) for v in (prov or {}).values()) for prov in ma.values())
    except Exception:
        pass
    db = "/home/amux/.amux/amux.db"
    if os.path.exists(db):
        try:
            c = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=2)
            out["turns_total"] = c.execute("SELECT COUNT(*) FROM cmd_history WHERE type='user'").fetchone()[0]
            c.close()
        except sqlite3.Error:
            pass
    try:
        out["cpu_pct"] = round(os.getloadavg()[0] / (os.cpu_count() or 1) * 100, 1)
        mem = dict(l.split(":", 1) for l in open("/proc/meminfo"))
        total, avail = int(mem["MemTotal"].split()[0]), int(mem["MemAvailable"].split()[0])
        out["mem_pct"] = round(100 * (total - avail) / total, 1)
    except (OSError, KeyError, ValueError):
        pass
    return out


def relieve_disk(du_pct):
    """M3: above 85%, prune only what regenerates (logs, package and build caches). Never customer data."""
    if du_pct < 85:
        return None
    before = shutil.disk_usage("/").used
    for cmd in (["journalctl", "--vacuum-size=200M"], ["apt-get", "clean"], ["docker", "system", "prune", "-f"]):
        sh(*cmd, check=False, timeout=300)
    cache = "/home/amux/.cache"
    if os.path.isdir(cache):
        for name in os.listdir(cache):
            shutil.rmtree(os.path.join(cache, name), ignore_errors=True)
    freed = before - shutil.disk_usage("/").used
    log(event="disk", verdict="fleet_disk_pruned", freed_mb=round(freed / 1e6))
    return {"freed_bytes": max(0, freed)}


def heartbeat(cfg):
    du = shutil.disk_usage("/")
    pruned = relieve_disk(100 * du.used / du.total)
    if pruned:
        du = shutil.disk_usage("/")
    body = {"version": server_version(), "last_backup_at": last_backup_at(),
            "disk_used_pct": round(100 * du.used / du.total, 1), "serving": server_version() is not None,
            "release_report": state("release_report"), "tailnet_url": tailnet_url(),
            "backup_fingerprint": state("backup_fingerprint"), "abuse_signals": abuse_signals(), "usage": usage(),
            "disk_pruned": pruned}
    reply = post(cfg, "heartbeat", body)
    apply_reply(cfg, reply)
    return reply


def run():
    cfg = env()
    log(event="start", server=cfg.get("AMUX_SERVER_ID"), verdict="fleet_agent_started")
    if cfg.get("AMUX_MODE") == "drill":
        while True:                                   # the control plane deletes this server after the report
            try:
                if run_drill(cfg):
                    return
            except Exception as e:
                log(event="drill", verdict="fleet_drill_error", error=str(e)[:300])
            time.sleep(INTERVAL)
    while True:
        try:
            if first_boot(cfg):
                heartbeat(cfg)
                last = (state("backup") or {}).get("last_ok") or 0
                if time.time() - last > 86400:
                    run_backup()
        except urllib.error.URLError as e:
            log(event="heartbeat", verdict="fleet_control_unreachable", error=str(e)[:200])
        except Exception as e:
            log(event="loop", verdict="fleet_loop_error", error=str(e)[:300])
        time.sleep(INTERVAL)


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "run":
        run()
    elif cmd == "once":
        print(json.dumps(heartbeat(env()), indent=1))
    else:
        sys.exit(__doc__)
