"""procwarden — inspect and selectively kill macOS processes, safely.

Subcommands:
  list     table of processes by a chosen dimension (default)
  top      live-refreshing list
  inspect  full detail on one pid
  kill     terminate pid(s) with the safety gate enforced
  menubar  launch the menu bar app
"""
from __future__ import annotations

import argparse
import json
import os
import signal
import sys
import time

from . import collect
from .actions import gate, kill_pid
from .safety import FATAL, LEVEL_ORDER
from .util import c, human_bytes, human_duration, level_colored, set_color


# --------------------------------------------------------------------------- #
# rendering
# --------------------------------------------------------------------------- #
def _overview_line():
    o = collect.system_overview()
    load = ", ".join(f"{x:.2f}" for x in o["load"])
    return (
        c("system  ", "bold")
        + f"cpu {o['cpu_percent']:.0f}% ({o['cpu_count']} cores)  "
        + f"load {load}  "
        + f"mem {human_bytes(o['mem_used'])}/{human_bytes(o['mem_total'])} "
        + f"({o['mem_percent']:.0f}%)  "
        + f"disk {human_bytes(o['disk_used'])}/{human_bytes(o['disk_total'])} "
        + f"({o['disk_percent']:.0f}% used)"
    )


def render_table(procs, show_net, show_author):
    headers = ["PID", "USER", "CPU%", "MEM", "%MEM"]
    if show_net:
        headers += ["NET-IN", "NET-OUT"]
    headers += ["UPTIME", "THR", "RISK", "NAME"]
    if show_author:
        headers += ["AUTHOR"]

    rows = []
    for p in procs:
        row = [
            str(p.pid),
            p.username[:12],
            f"{p.cpu:.1f}",
            human_bytes(p.mem_rss),
            f"{p.mem_percent:.1f}",
        ]
        if show_net:
            row += [human_bytes(p.net_in), human_bytes(p.net_out)]
        row += [
            human_duration(p.uptime),
            str(p.threads),
            p.safety.level if p.safety else "?",
            p.name[:28],
        ]
        if show_author:
            row += [(p.author or "")[:24]]
        rows.append((row, p))

    widths = [len(h) for h in headers]
    for row, _ in rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(cell))

    risk_idx = headers.index("RISK")
    out = ["  ".join(c(h.ljust(widths[i]), "bold") for i, h in enumerate(headers))]
    for row, p in rows:
        cells = []
        for i, cell in enumerate(row):
            text = cell.ljust(widths[i])
            if i == risk_idx and p.safety:
                text = level_colored(cell.ljust(widths[i]))
            cells.append(text)
        out.append("  ".join(cells))
    return "\n".join(out)


# --------------------------------------------------------------------------- #
# commands
# --------------------------------------------------------------------------- #
def cmd_list(args):
    procs = collect.collect(want_net=args.net, want_author=args.author,
                            mine_only=args.user)
    key = collect.SORT_KEYS[args.sort]
    procs.sort(key=key, reverse=True)
    if args.killable:
        procs = [p for p in procs if p.safety and p.safety.killable]
    procs = procs[: args.limit]

    if args.json:
        print(json.dumps([_proc_dict(p) for p in procs], indent=2))
        return

    print(_overview_line())
    print()
    print(render_table(procs, args.net, args.author))
    print()
    print(c(f"showing {len(procs)} of all processes, sorted by {args.sort}. "
            "RISK: " + c("LOW", "green") + "=safe  "
            + c("MEDIUM", "yellow") + "=verify  "
            + c("HIGH", "magenta") + "=system  "
            + c("FATAL", "red") + "=never", "dim"))


def cmd_top(args):
    try:
        while True:
            procs = collect.collect(sample_interval=0.6, want_net=args.net,
                                    mine_only=args.user)
            procs.sort(key=collect.SORT_KEYS[args.sort], reverse=True)
            procs = procs[: args.limit]
            os.system("clear")
            print(_overview_line())
            print(c(f"  (refreshing every ~{args.interval}s, sort={args.sort}, "
                    "Ctrl-C to quit)", "dim"))
            print()
            print(render_table(procs, args.net, False))
            time.sleep(max(0.0, args.interval - 0.6))
    except KeyboardInterrupt:
        print("\nbye")


def cmd_inspect(args):
    p = collect.get_one(args.pid)
    if not p:
        print(c(f"no such process: {args.pid}", "red"))
        sys.exit(1)
    s = p.safety
    print(c(f"{p.name}  (pid {p.pid})", "bold"))
    print(f"  status      {p.status}")
    print(f"  owner       {p.username}")
    print(f"  parent      pid {p.ppid}")
    print(f"  author      {p.author}")
    print(f"  exe         {p.exe or '(unavailable)'}")
    print(f"  cmdline     {p.cmdline}")
    print(f"  uptime      {human_duration(p.uptime)} "
          f"(started {time.strftime('%Y-%m-%d %H:%M', time.localtime(p.create_time))})")
    print(f"  cpu         {p.cpu:.1f}%")
    print(f"  memory      {human_bytes(p.mem_rss)} ({p.mem_percent:.1f}% of RAM)")
    print(f"  threads     {p.threads}")
    print(f"  network     in {human_bytes(p.net_in)} / out {human_bytes(p.net_out)} "
          + c("(cumulative; nettop)", "dim"))
    print(f"  RISK        {level_colored(s.level)} — {s.reason}")
    verdict = (c("safe to kill with `procwarden kill %d`" % p.pid, "green")
               if s.killable else
               c("protected — needs --force" if s.level != FATAL
                 else "PROTECTED — tool will not kill this", "yellow"))
    print(f"  verdict     {verdict}")


def cmd_kill(args):
    sig = getattr(signal, "SIG" + args.signal.upper(), signal.SIGTERM)
    for pid in args.pids:
        p = collect.get_one(pid, want_net=False, want_author=False)
        if not p:
            print(c(f"pid {pid}: no such process", "yellow"))
            continue
        decision = gate(p.safety, args.force)
        label = f"{p.name} (pid {pid}) [{p.safety.level}]"
        if not decision.allowed:
            print(c(f"SKIP {label}: {decision.note}", "red"))
            continue
        if not args.yes:
            extra = c(" *** OVERRIDE ***", "magenta") if decision.needs_override else ""
            ans = input(f"kill {label}{extra}? [y/N] ").strip().lower()
            if ans not in ("y", "yes"):
                print("  skipped")
                continue
        ok, msg = kill_pid(pid, sig=sig, escalate=args.escalate)
        print(("  " + c("✓ ", "green") + msg) if ok else ("  " + c("✗ ", "red") + msg))


def cmd_menubar(args):
    from . import menubar
    menubar.main()


def cmd_sweep(args):
    from . import config as cfgmod
    from . import sweep as sweepmod
    cfg = cfgmod.load_config(args.config) if args.config else cfgmod.load_config()
    print(c(f"sweep starting (mode: "
            + ("DRY-RUN" if (args.dry_run or not cfg.get("enforce")) else "ENFORCE")
            + f", config: {args.config or cfgmod.CONFIG_PATH})", "bold"))
    summary = sweepmod.run_sweep(
        cfg, dry_run_override=True if args.dry_run else None,
        logger=(None if args.quiet else lambda m: print(m)))
    print()
    print(c(f"scanned {summary['scanned']} procs, {summary['fired']} rule hit(s), "
            f"mode={summary['mode']}", "bold"))
    if not summary["fired"]:
        print(c("  nothing matched — machine looks clean", "green"))
    print(c(f"  log: {cfg.get('log_file')}", "dim"))


def cmd_maintain(args):
    from . import config as cfgmod
    from . import maintain as mnt
    if args.print_sudoers:
        print(mnt.privilege_dropin())
        return
    cfg = cfgmod.load_config(args.config) if args.config else cfgmod.load_config()
    mode = "DRY-RUN" if args.dry_run else "LIVE"
    if not args.quiet and not args.json:
        print(c(f"maintain starting (mode: {mode})", "bold"))
    summary = mnt.run_maintain(
        cfg, dry_run_override=True if args.dry_run else None,
        logger=(None if (args.quiet or args.json) else lambda m: print(m)))
    if args.json:
        print(json.dumps(summary, indent=2))
        return
    b, a = summary["before"], summary["after"]
    freed = sum(x.get("freed_bytes", 0) for x in summary["actions"])
    print()
    print(c(f"done: mem {b['mem_used_pct']:.0f}% -> {a['mem_used_pct']:.0f}% used, "
            f"{human_bytes(freed)} reclaimed, "
            f"{len(summary['concerns'])} concern(s), "
            f"{len(summary['alert_channels'])} alert(s)", "bold"))
    print(c(f"  log: {mnt.MAINT_LOG}", "dim"))


def cmd_config(args):
    from . import config as cfgmod
    if args.action == "init":
        wrote = cfgmod.write_default(overwrite=args.force)
        if wrote:
            print(c(f"wrote default config -> {cfgmod.CONFIG_PATH}", "green"))
            print(c("  it is in DRY-RUN mode (enforce=false). Edit rules, then "
                    "set enforce=true to act.", "dim"))
        else:
            print(c(f"config already exists at {cfgmod.CONFIG_PATH} "
                    "(use --force to overwrite)", "yellow"))
    elif args.action == "path":
        print(cfgmod.CONFIG_PATH)
    elif args.action == "show":
        cfg = cfgmod.load_config()
        print(json.dumps(cfg, indent=2))


def cmd_autostart(args):
    from . import autostart as au
    if args.action == "install":
        path = au.install()
        print(c(f"installed menu bar login item -> {path}", "green"))
        print(c("  🛡 launches now and at every login; relaunches if it quits.",
                "dim"))
        print(c(f"  logs: {au.STDERR_LOG}", "dim"))
    elif args.action == "uninstall":
        au.uninstall()
        print(c("removed menu bar login item (the 🛡 will quit)", "green"))
    elif args.action == "status":
        print(au.status())


def cmd_schedule(args):
    from . import schedule as sch
    if args.action == "install":
        path = sch.install(interval_minutes=args.interval)
        print(c(f"installed LaunchAgent -> {path}", "green"))
        print(c(f"  runs `procwarden sweep` every {args.interval} min, "
                "survives reboot.", "dim"))
        print(c(f"  logs: {sch.STDOUT_LOG}", "dim"))
    elif args.action == "nightly":
        try:
            hh, mm = (int(x) for x in args.at.split(":"))
        except ValueError:
            print(c(f"bad --at {args.at!r}; want HH:MM (e.g. 03:00)", "red"))
            sys.exit(1)
        path = sch.install_nightly(hour=hh, minute=mm)
        print(c(f"installed nightly LaunchAgent -> {path}", "green"))
        print(c(f"  runs `procwarden maintain` daily at {hh:02d}:{mm:02d}, "
                "survives reboot.", "dim"))
        print(c(f"  logs: {sch.MAINT_OUT_LOG}", "dim"))
    elif args.action == "uninstall":
        sch.uninstall(maintain=args.maintain)
        print(c(f"uninstalled {'nightly maintain' if args.maintain else 'sweep'} "
                "LaunchAgent", "green"))
    elif args.action == "status":
        print(sch.status())


def _proc_dict(p):
    return {
        "pid": p.pid, "ppid": p.ppid, "name": p.name, "user": p.username,
        "status": p.status, "cpu": round(p.cpu, 1), "mem_rss": p.mem_rss,
        "mem_percent": round(p.mem_percent, 2), "threads": p.threads,
        "uptime_sec": int(p.uptime), "net_in": p.net_in, "net_out": p.net_out,
        "author": p.author, "exe": p.exe, "cmdline": p.cmdline,
        "risk": p.safety.level if p.safety else None,
        "killable": p.safety.killable if p.safety else None,
        "risk_reason": p.safety.reason if p.safety else None,
    }


# --------------------------------------------------------------------------- #
# argparse
# --------------------------------------------------------------------------- #
def build_parser():
    # shared flag usable before OR after the subcommand
    common_global = argparse.ArgumentParser(add_help=False)
    common_global.add_argument("--no-color", action="store_true",
                               help="disable ANSI color")

    ap = argparse.ArgumentParser(
        prog="procwarden", parents=[common_global],
        description="Inspect and selectively kill macOS processes — safely.")
    sub = ap.add_subparsers(dest="cmd")

    def add_common(sp):
        sp.add_argument("-n", "--limit", type=int, default=25,
                        help="rows to show (default 25)")
        sp.add_argument("-s", "--sort", choices=list(collect.SORT_KEYS),
                        default="cpu", help="sort dimension (default cpu)")
        sp.add_argument("--user", action="store_true",
                        help="only your processes")
        sp.add_argument("--net", action="store_true",
                        help="include per-process network (slower; runs nettop)")

    pl = sub.add_parser("list", help="one-shot process table (default)",
                        parents=[common_global])
    add_common(pl)
    pl.add_argument("--author", action="store_true",
                    help="resolve code-sign author (slower)")
    pl.add_argument("--killable", action="store_true",
                    help="only processes safe to kill")
    pl.add_argument("--json", action="store_true", help="machine-readable output")
    pl.set_defaults(func=cmd_list)

    pt = sub.add_parser("top", help="live-refreshing table",
                        parents=[common_global])
    add_common(pt)
    pt.add_argument("-i", "--interval", type=float, default=2.0,
                    help="refresh seconds (default 2)")
    pt.set_defaults(func=cmd_top)

    pi = sub.add_parser("inspect", help="full detail for one pid",
                        parents=[common_global])
    pi.add_argument("pid", type=int)
    pi.set_defaults(func=cmd_inspect)

    pk = sub.add_parser("kill", help="terminate pid(s) with safety gate",
                        parents=[common_global])
    pk.add_argument("pids", type=int, nargs="+")
    pk.add_argument("--signal", default="TERM",
                    help="signal name w/o SIG- prefix (TERM, KILL, INT, HUP...)")
    pk.add_argument("--escalate", action="store_true",
                    help="follow SIGTERM with SIGKILL if it survives 2s")
    pk.add_argument("-f", "--force", action="store_true",
                    help="override the gate for HIGH/MEDIUM risk (never FATAL)")
    pk.add_argument("-y", "--yes", action="store_true",
                    help="skip the confirmation prompt")
    pk.set_defaults(func=cmd_kill)

    pm = sub.add_parser("menubar", help="launch the menu bar app",
                        parents=[common_global])
    pm.set_defaults(func=cmd_menubar)

    ps = sub.add_parser("sweep", help="run config-driven cleanup once",
                        parents=[common_global])
    ps.add_argument("--config", help="config path (default ~/.procwarden/config.json)")
    ps.add_argument("--dry-run", action="store_true",
                    help="force dry-run even if config has enforce=true")
    ps.add_argument("-q", "--quiet", action="store_true",
                    help="only print the summary")
    ps.set_defaults(func=cmd_sweep)

    pmn = sub.add_parser("maintain",
                         help="nightly maintenance pass (purge + spotlight + "
                              "health + reap)", parents=[common_global])
    pmn.add_argument("--config", help="config path (default ~/.procwarden/config.json)")
    pmn.add_argument("--dry-run", action="store_true",
                     help="force every action to log-only (no purge/kill)")
    pmn.add_argument("-q", "--quiet", action="store_true",
                     help="only print the summary (used by launchd)")
    pmn.add_argument("--json", action="store_true", help="machine-readable output")
    pmn.add_argument("--print-sudoers", action="store_true",
                     help="print the /etc/sudoers.d/procwarden drop-in and exit")
    pmn.set_defaults(func=cmd_maintain)

    pc = sub.add_parser("config", help="manage the sweep config",
                        parents=[common_global])
    pc.add_argument("action", choices=["init", "path", "show"])
    pc.add_argument("-f", "--force", action="store_true",
                    help="overwrite existing config on init")
    pc.set_defaults(func=cmd_config)

    psch = sub.add_parser("schedule",
                          help="install/remove the recurring sweep & nightly maintain",
                          parents=[common_global])
    psch.add_argument("action",
                      choices=["install", "nightly", "uninstall", "status"])
    psch.add_argument("-i", "--interval", type=int, default=30,
                      help="minutes between sweeps (install; default 30)")
    psch.add_argument("--at", default="03:00",
                      help="HH:MM for the nightly maintain (default 03:00)")
    psch.add_argument("--maintain", action="store_true",
                      help="target the nightly maintain agent on uninstall")
    psch.set_defaults(func=cmd_schedule)

    pas = sub.add_parser("autostart",
                         help="run the menu bar app at login (install/uninstall/status)",
                         parents=[common_global])
    pas.add_argument("action", choices=["install", "uninstall", "status"])
    pas.set_defaults(func=cmd_autostart)
    return ap


def main(argv=None):
    ap = build_parser()
    raw = argv if argv is not None else sys.argv[1:]
    args = ap.parse_args(raw)
    if not getattr(args, "cmd", None):
        # no subcommand -> default to `list`, preserving global flags
        args = ap.parse_args(["list"] + [a for a in raw if a == "--no-color"])
    if getattr(args, "no_color", False):
        set_color(False)
    args.func(args)


if __name__ == "__main__":
    main()
