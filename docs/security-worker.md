# The `security` worker

General security for this Mac and every repo under `~/Dev`: watch for
vulnerabilities, work out which ones apply here, and land the patches.
Mixpeek product security (auth surfaces, tenant isolation, key handling in the
Mixpeek app) stays with `mixpeek-security`. When a finding is in Mixpeek's
product code, hand it to that worker with the evidence.

Created 2026-10-26 after the Tailscale "Tailworm" disclosure and the eMudhra
certificate compromise (TS-2026-013).

## What wakes it

| Schedule | Kind | What it does |
|---|---|---|
| Daily 06:30, security vulnerability watch | shell | Runs `scripts/security-watch.py` from `origin/main`. Costs no model call. Messages this worker only when there is a new finding or a source failed to run |
| Weekly Monday 09:00, security news review | tmux | A full turn: read security news for software this box runs, since feeds miss findings with no CVE yet (Tailworm had none) |

`scripts/security-watch.py` checks:

- CISA's Known Exploited Vulnerabilities list, matched against the software
  this box runs (`SECURITY_WATCH_INVENTORY`)
- Tailscale's security bulletins, and the installed client against the newest
  stable release
- `cargo audit`, `npm audit` and `pip-audit` over every lockfile in
  `SECURITY_WATCH_REPOS`, read from each repo's `origin/main`: the shared
  checkouts under `~/Dev` can sit far behind main, so their working trees
  are not what ships. npm findings carry `scope`: `runtime` (still flagged
  with `--omit=dev`) or `dev-only` (build and test tooling)
- macOS software updates

Each run writes `~/.amux/security-watch/<date>.json`. `state.json` there holds
the ids already reported, so each finding is sent once. Each source reports
whether it ran: a source that could not run is listed as unmeasured, never as
clean. Run it by hand with `python3 scripts/security-watch.py --dry-run`. The
scheduler stops a shell run at 600s, so checks run in parallel inside
`SECURITY_WATCH_BUDGET_S` (default 480); one still running at the deadline is
reported unmeasured.

## How to work a finding

1. Decide whether it applies: the affected version is installed, the
   vulnerable feature is in use, the code path is reachable. Write down which
   of these you checked. "Not affected" needs a reason.
2. Patch in a detached worktree at `origin/main`, run the repo's gates, and
   land it on main (each repo's CLAUDE.md says how). Prefer the smallest
   compatible update. A major-version bump gets a test of the behaviour it
   touches.
3. Anything that needs Ethan goes to a `needsyou` card with the question and
   what unblocks it: installs that need his admin password (the Tailscale app,
   macOS updates), paid tiers, anything sent outside the company.
4. Keep one board card per finding or per repo batch, closed with the command
   and result that shows it fixed (for example `cargo audit` with 0
   vulnerabilities).

## Boundaries

Ethan's global boundary applies: no spending, nothing sent outside the
company, no deleting or migrating customer or production data, and no
restarts of shared services without warning. Reading anything is fine.
Never paste a credential value into a card, commit or message.
