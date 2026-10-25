#!/usr/bin/env python3
"""Start a NAMED, interactive Codex thread: codex-new.py <name> [-] [--fork <thread>] [--cwd DIR].

A thread made with `codex exec` is headless: it has a session id and no name, so the Codex app
does not list it and `/codex <name>` cannot reach it. This script drives `codex app-server` (the
JSON-RPC server behind the app) over stdio instead:

  1. thread/start, or thread/fork of an existing thread (keeps its whole history),
  2. thread/name/set, so the app lists it and codex-send.py finds it by name,
  3. turn/start with the message from stdin (optional). A detached child keeps that app-server
     alive until the turn completes (log: ~/.amux/logs/codex-new-<thread>.log), so this script
     returns at once. Later messages go through codex-send.py like any other thread.

Prints `Started thread <id> named "<name>"`. Exit 2 if the name is already taken by a live
thread (send to it instead), 1 on any other failure. Every start is logged to
~/.amux/logs/codex-send.log next to the sends.

  --full-access   sandbox danger-full-access and approval never (an unattended tester or
                  builder); default is the user's config.toml
"""
import argparse, json, os, subprocess, sys, time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import importlib.util
_spec = importlib.util.spec_from_file_location("codex_send", Path(__file__).resolve().parent / "codex-send.py")
cs = importlib.util.module_from_spec(_spec); _spec.loader.exec_module(cs)


class Rpc:
    def __init__(self, exe):
        self.p = subprocess.Popen([exe, "app-server"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, text=True, bufsize=1, start_new_session=True,
                                  cwd=str(Path.home()))
        self.n = 0

    def call(self, method, params, timeout=60):
        self.n += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        end = time.time() + timeout
        while time.time() < end:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError(f"app-server closed during {method}")
            msg = json.loads(line)
            if msg.get("id") == self.n:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg.get("result") or {}
        raise RuntimeError(f"{method}: no answer in {timeout}s")

    def notify(self, method, params=None):
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method, "params": params or {}}) + "\n")
        self.p.stdin.flush()

    def close(self):
        self.p.stdin.close(); self.p.wait(timeout=10)


def follow(rpc, tid, hours=12):
    """Detached: hold the app-server open until the thread's turn completes, then close it."""
    if os.fork():
        return
    os.setsid()
    # Let go of the caller's stdio. Holding it kept `$(codex-new.py ...)` and any pipe blocked until
    # the turn finished, so a caller that watched the thread only ever saw it idle (2026-10-23).
    null = os.open(os.devnull, os.O_RDWR)
    for fd in (0, 1, 2):
        os.dup2(null, fd)
    logf = Path.home() / ".amux" / "logs" / f"codex-new-{tid}.log"
    end = time.time() + hours * 3600
    with logf.open("a") as out:
        while time.time() < end:
            line = rpc.p.stdout.readline()
            if not line:
                break
            try:
                m = json.loads(line)
            except ValueError:
                continue
            meth = m.get("method", "")
            if meth.startswith(("turn/", "thread/status", "error")):
                out.write(json.dumps({"ts": int(time.time()), "method": meth}) + "\n"); out.flush()
            if "id" in m and meth:   # a request from the server (approval etc.): decline, never hang
                rpc.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "unattended"}}) + "\n")
                rpc.p.stdin.flush()
            if meth == "turn/completed":
                break
    try: rpc.close()
    except Exception: pass
    os._exit(0)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("name")
    ap.add_argument("message", nargs="?", default="", help="- reads the first message from stdin")
    ap.add_argument("--fork", default="", help="thread id or name to fork (history carries over)")
    ap.add_argument("--cwd", default=os.getcwd())
    ap.add_argument("--full-access", action="store_true")
    a = ap.parse_args()
    msg = sys.stdin.read() if a.message == "-" else a.message

    t0 = time.time()
    rows, _ = cs.threads()
    if any(cs.norm(n) == cs.norm(a.name) for _, n, _ in rows):
        print(f'codex-new: a live thread is already named "{a.name}"; send to it with codex-send.py', file=sys.stderr)
        return 2
    exe = cs.codex_bin()
    if not exe:
        print("codex-new: no codex binary found", file=sys.stderr); return 1
    rpc = Rpc(exe)
    started = False
    try:
        rpc.call("initialize", {"clientInfo": {"name": "amux-codex-skill", "title": "amux", "version": "1"}})
        rpc.notify("initialized")
        opts = {"cwd": a.cwd}
        if a.full_access:
            opts.update(sandbox="danger-full-access", approvalPolicy="never")
        if a.fork:
            hit = cs.resolve(a.fork)[0]
            if not hit:
                raise RuntimeError(f"--fork {a.fork!r} matches no single thread")
            src = hit[0]
            res = rpc.call("thread/fork", {"threadId": src, **opts})
        else:
            res = rpc.call("thread/start", opts)
        tid = (res.get("thread") or {}).get("id") or res.get("threadId")
        rpc.call("thread/name/set", {"threadId": tid, "name": a.name})
        if msg.strip():
            rpc.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text": msg}]})
            started = True
    except Exception as e:
        cs.log(chat=a.name, verdict="new_failed", error=str(e)[:300])
        print(f"codex-new: {e}", file=sys.stderr)
        try: rpc.close()
        except Exception: pass
        return 1
    if started:
        follow(rpc, tid)
    else:
        rpc.close()
    cs.log(chat=a.name, thread=tid, name=a.name, verdict="started", forked_from=a.fork or None, chars=len(msg),
           returned_after_s=round(time.time() - t0, 1))
    print(f'Started thread {tid} named "{a.name}"')
    return 0


if __name__ == "__main__":
    sys.exit(main())
