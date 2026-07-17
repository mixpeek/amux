"""procwarden menu bar app (rumps).

A click-to-open panel showing the heaviest processes by CPU / memory / network,
with inspect + kill. Safety-first: the menu bar will only kill processes the
safety engine marks `killable` (your own apps / auto-relaunching agents).
System / root / FATAL processes show as locked — to override those you must use
the CLI with --force, on purpose.
"""
from __future__ import annotations

import threading

import rumps

from . import collect
from .actions import kill_pid
from .config import load_config
from .safety import FATAL, HIGH, LOW, MEDIUM, NONE
from .sweep import run_sweep
from .util import human_bytes, human_duration

LEVEL_ICON = {NONE: "⚪", LOW: "🟢", MEDIUM: "🟡", HIGH: "🟠", FATAL: "🔴"}
SORTS = [("cpu", "CPU"), ("mem", "Memory"), ("net", "Network")]


class ProcWardenApp(rumps.App):
    def __init__(self):
        super().__init__("procwarden", title="🛡", quit_button="Quit procwarden")
        self.sort = "cpu"
        self.row_count = 12
        self._busy = False
        self.menu = ["(loading…)"]
        self.timer = rumps.Timer(self.refresh, 6)
        self.timer.start()
        # first paint shortly after launch
        rumps.Timer(self._first, 0.5).start()

    # -- refresh ----------------------------------------------------------- #
    def _first(self, sender):
        sender.stop()
        self.refresh(None)

    def refresh(self, _sender):
        if self._busy:
            return
        self._busy = True
        try:
            want_net = self.sort == "net"
            procs = collect.collect(sample_interval=0.3, want_net=want_net)
            procs.sort(key=collect.SORT_KEYS[self.sort], reverse=True)
            top = procs[: self.row_count]
            ov = collect.system_overview()
        finally:
            self._busy = False
        self._render(top, ov)

    def _render(self, procs, ov):
        self.menu.clear()
        header = rumps.MenuItem(
            f"CPU {ov['cpu_percent']:.0f}%   "
            f"MEM {human_bytes(ov['mem_used'])}/{human_bytes(ov['mem_total'])} "
            f"({ov['mem_percent']:.0f}%)")
        header.set_callback(None)
        self.menu.add(header)
        self.menu.add(rumps.separator)

        for p in procs:
            self.menu.add(self._proc_item(p))

        self.menu.add(rumps.separator)
        self.menu.add(self._sort_menu())
        self.menu.add(rumps.MenuItem("Run cleanup sweep now",
                                     callback=self._sweep))
        self.menu.add(rumps.MenuItem("Refresh now", callback=self.refresh))

    # -- per-process item -------------------------------------------------- #
    def _proc_item(self, p):
        icon = LEVEL_ICON.get(p.safety.level if p.safety else NONE, "⚪")
        title = f"{icon} {p.cpu:5.1f}%  {human_bytes(p.mem_rss):>7}  {p.name}"
        item = rumps.MenuItem(title)

        for line in (f"pid {p.pid}   user {p.username}",
                     f"uptime {human_duration(p.uptime)}   threads {p.threads}",
                     f"risk {p.safety.level}: {p.safety.reason}" if p.safety else ""):
            if line:
                sub = rumps.MenuItem(line)
                sub.set_callback(None)
                item.add(sub)
        item.add(rumps.separator)
        item.add(rumps.MenuItem("Inspect…", callback=self._make_inspect(p.pid)))

        if p.safety and p.safety.killable:
            item.add(rumps.MenuItem("Kill", callback=self._make_kill(p.pid, p.name)))
        else:
            lock = rumps.MenuItem("🔒 Protected (use CLI --force)")
            lock.set_callback(None)
            item.add(lock)
        return item

    def _make_inspect(self, pid):
        def cb(_sender):
            p = collect.get_one(pid)
            if not p:
                rumps.alert("procwarden", f"pid {pid} is gone.")
                return
            msg = (
                f"{p.name}  (pid {p.pid})\n\n"
                f"owner:    {p.username}\n"
                f"author:   {p.author}\n"
                f"status:   {p.status}\n"
                f"cpu:      {p.cpu:.1f}%\n"
                f"memory:   {human_bytes(p.mem_rss)} ({p.mem_percent:.1f}%)\n"
                f"threads:  {p.threads}\n"
                f"uptime:   {human_duration(p.uptime)}\n"
                f"network:  in {human_bytes(p.net_in)} / out {human_bytes(p.net_out)}\n"
                f"exe:      {p.exe or '(n/a)'}\n\n"
                f"RISK {p.safety.level}: {p.safety.reason}")
            rumps.alert("Inspect", msg)
        return cb

    def _make_kill(self, pid, name):
        def cb(_sender):
            if rumps.alert("Kill process?",
                           f"Send SIGTERM to {name} (pid {pid})?",
                           ok="Kill", cancel="Cancel") != 1:
                return
            ok, msg = kill_pid(pid, escalate=True)
            rumps.notification("procwarden", "Kill" if ok else "Kill failed", msg)
            self.refresh(None)
        return cb

    # -- sort + sweep ------------------------------------------------------ #
    def _sort_menu(self):
        m = rumps.MenuItem(f"Sort by: {dict(SORTS)[self.sort]}")
        for key, label in SORTS:
            mark = "✓ " if key == self.sort else "   "
            m.add(rumps.MenuItem(mark + label, callback=self._make_sort(key)))
        return m

    def _make_sort(self, key):
        def cb(_sender):
            self.sort = key
            self.refresh(None)
        return cb

    def _sweep(self, _sender):
        cfg = load_config()
        summary = run_sweep(cfg)
        mode = summary["mode"]
        n = summary["fired"]
        rumps.notification(
            "procwarden sweep",
            f"{mode}: {n} rule hit(s)",
            f"scanned {summary['scanned']} procs. See log for details.")
        self.refresh(None)


def main():
    # Menu-bar accessory, not a Dock app: without this, running under plain
    # python shows a rocket Dock icon for the warden itself.
    try:
        from AppKit import NSApplication, NSApplicationActivationPolicyAccessory
        NSApplication.sharedApplication().setActivationPolicy_(
            NSApplicationActivationPolicyAccessory)
    except Exception:
        pass
    ProcWardenApp().run()
