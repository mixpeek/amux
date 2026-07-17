"""Config + persistent state for scheduled sweeps.

Config lives at ~/.procwarden/config.json and is the ONLY place the automated
sweep decides what to act on. State (strike counts for sustained-threshold
detection) lives at ~/.procwarden/state.json.

Design choices for safety:
  * `enforce` defaults to FALSE -> sweeps only LOG what they *would* do.
    You flip it to true once you trust the rules.
  * Sustained thresholds (`for_sweeps`) avoid acting on a single noisy sample.
  * The kill gate (safety.py) still applies on top of every rule, so a rule can
    never kill a FATAL process even if it matches.
"""
from __future__ import annotations

import json
import os

CONFIG_DIR = os.path.expanduser("~/.procwarden")
CONFIG_PATH = os.path.join(CONFIG_DIR, "config.json")
STATE_PATH = os.path.join(CONFIG_DIR, "state.json")
DEFAULT_LOG = os.path.join(CONFIG_DIR, "sweep.log")


def default_config():
    return {
        "_README": "procwarden sweep config. enforce=false means DRY RUN "
                    "(log only, nothing killed). Set true to act. The safety "
                    "gate still protects system/FATAL processes regardless.",
        "enforce": False,
        "log_file": DEFAULT_LOG,
        "exclude_names": ["^ssh$", "^sshd$", "^tmux", "^procwarden"],
        "exclude_users": [],
        "maintenance": {
            "_README": "Nightly `procwarden maintain` pass. purge/spotlight/"
                       "health are non-destructive and run whenever their flag "
                       "is on. The process reap obeys the top-level `enforce` "
                       "flag (dry-run until you trust it).",
            "enabled": True,
            "purge_memory": True,          # reclaim inactive RAM (relieves swap)
            "run_sweep": True,             # also run the config sweep each night
            "spotlight_exclude": [],       # dirs to keep de-indexed (starves the
                                           # fseventsd/mds storm). e.g.
                                           # ["~/.amux", "/path/to/vm-images"]
            "watch_daemons": ["fseventsd", "ecosystemd", "ecosystemanalyti",
                              "mds", "mds_stores", "mdworker", "mdworker_shared"],
            "health": {
                "mem_used_pct_above": 90,
                "swap_used_mb_above": 6000,
                "daemon_cpu_above": 80,
            },
            "alert": {
                "post_board": True,          # POST amux board on NEW concerns
                "urgent_owner_alert": False,  # push+iMessage; reserve for severe
                "amux_base": "https://localhost:8822",
                "session": "procwarden-maintain",
            },
        },
        "rules": [
            {
                "name": "zombies",
                "enabled": True,
                "match": {"status": "zombie", "owner": "any"},
                "when": {},
                "for_sweeps": 1,
                "action": "report",
            },
            {
                "name": "runaway-cpu",
                "enabled": True,
                "match": {"name_regex": ".*", "owner": "me"},
                "when": {"cpu_above": 90},
                "for_sweeps": 3,
                "action": "terminate",
                "signal": "TERM",
                "force": False,
            },
            {
                "name": "memory-hog (report only)",
                "enabled": True,
                "match": {"name_regex": ".*", "owner": "me"},
                "when": {"mem_above_mb": 8000},
                "for_sweeps": 2,
                "action": "report",
            },
            {
                "name": "example: kill stale updater after 24h idle (disabled)",
                "enabled": False,
                "match": {"name_regex": "(?i)updater|autoupdate", "owner": "me"},
                "when": {"uptime_above_hours": 24, "cpu_below": 1},
                "for_sweeps": 1,
                "action": "terminate",
                "signal": "TERM",
                "force": False,
            },
            {
                "name": "stale-idle-claude-session (opt-in: enable + set enforce)",
                "_README": "Reaps YOUR claude sessions idle >48h at ~0% CPU to "
                           "free RAM. Disabled by default — an idle session may "
                           "just be waiting for input; killing loses its state "
                           "(recoverable via `claude --resume`). Enable only if "
                           "you know these are abandoned.",
                "enabled": False,
                "match": {"cmdline_regex": "claude --model", "owner": "me"},
                "when": {"uptime_above_hours": 48, "cpu_below": 1},
                "for_sweeps": 1,
                "action": "terminate",
                "signal": "TERM",
                "force": False,
            },
        ],
    }


def _deep_merge(base, over):
    """Recursively merge dict `over` onto `base` (mutates and returns base).
    Used so a partial user `maintenance` block still gets default sub-keys —
    otherwise a shallow update would drop e.g. alert.amux_base and crash."""
    for k, v in over.items():
        if isinstance(v, dict) and isinstance(base.get(k), dict):
            _deep_merge(base[k], v)
        else:
            base[k] = v
    return base


def load_config(path=CONFIG_PATH):
    """Load config, merging onto defaults. Missing file -> defaults."""
    cfg = default_config()
    if os.path.exists(path):
        with open(path) as f:
            user = json.load(f)
        # nested blocks are deep-merged so partial user config keeps defaults;
        # flat keys (incl. `rules`, which the user owns wholesale) are replaced.
        maint = user.pop("maintenance", None)
        cfg.update(user)
        if maint is not None:
            _deep_merge(cfg["maintenance"], maint)
    cfg["log_file"] = os.path.expanduser(cfg.get("log_file") or DEFAULT_LOG)
    return cfg


def write_default(path=CONFIG_PATH, overwrite=False):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    if os.path.exists(path) and not overwrite:
        return False
    with open(path, "w") as f:
        json.dump(default_config(), f, indent=2)
    return True


def load_state(path=STATE_PATH):
    if os.path.exists(path):
        try:
            with open(path) as f:
                return json.load(f)
        except (ValueError, OSError):
            return {}
    return {}


def save_state(state, path=STATE_PATH):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(state, f)
    os.replace(tmp, path)
