# Remote support & maintenance for customer deployments

**Status:** design proposal (2026-07-03)
**Context:** amux Concierge ($5k onboarding + $5k/mo) promises we support, maintain, and enhance each customer's deployment. But amux runs on hardware the customer owns, under their logins, and often handles PHI (dental, skilled nursing). So remote support must work **without their data leaving their machine**: customer-initiated sharing, redaction everywhere, opt-in anonymized health signals, and a full audit trail for BAA compliance.

Design north star: maximize our ability to diagnose, update, and fix remotely while keeping the "your machine, your data" guarantee intact. Done well, the support tooling is itself a selling point.

Most of this composes with primitives amux already has: session logs + `?plain=1` clean export, the Gmail email API, the watchdog/self-healing + crash-dump, the amux Cloud gateway, Litestream→R2 backup, and the shared board.

---

## A. Support bundle & log export  *(anchor / quick win)*

1. **Export support bundle** — one click on a Support tab zips: recent `server.log`, ANSI-stripped session logs (reuse `?plain=1`), redacted config/env, version + git commit, a system snapshot (OS, disk, mem, uptime), active sessions, and recent errors. Download or send.
2. **Email logs to support** — sends that bundle through the existing amux email API to `support@amux.io`, prefilled with the deployment id and a free-text "what happened" field. (Exactly the flow you described.)
3. **Automatic redaction on every export path** — strip API keys, tokens, bearer/cookies, and configurable PHI patterns (names, MRNs, SSNs, DOBs) before anything leaves the machine. Non-negotiable for HIPAA; show the customer a preview of what's being sent.
4. **Per-item share** — on the logs/peek tab, "Share this session's log with support" and "Copy diagnostics" (version + env summary + health) for quick paste.

## B. Health dashboard & self-diagnostics  *(quick–medium)*

5. **Health tab** — uptime, version/commit, session counts, error rate, watchdog restarts, disk/mem/CPU, provider reachability, cron/scheduler status, cert expiry.
6. **`amux doctor`** (CLI + in-dashboard) — runs a checklist (tmux present, disk space, ports free, DB integrity, providers reachable, Gmail token valid, cert healthy) and returns pass/fail with fix hints. Deflects a large share of tickets.
7. **Structured error panel** — capture exceptions/tracebacks with dedupe, counts, and last-seen; a "recent incidents" list instead of grepping raw logs.
8. **Staleness badge** — warn when this deployment's `amux-server.py` is behind the latest release.

## C. Opt-in remote monitoring  *(medium)*

9. **Heartbeat to amux Cloud** — periodic anonymized health POST (version, session/error counts, restart count; no logs, no PHI). Customer toggles it on. Lets us spot trouble before they report it.
10. **Support fleet console** (cloud side) — every consenting deployment with health, version, last-seen; sort/alert on down, unhealthy, or stale.
11. **Proactive alerts** — notify support and the customer on repeated crashes, disk near full, expired provider auth (e.g. Gmail token), or cert expiry.
12. **Weekly value + health report** — emailed to the customer and us: "X workflows run, ~Y hours saved, 0 incidents, on version Z." Doubles as retention/ROI proof.

## D. Safe remote updates & backup  *(medium–high)*

13. **Update channel** — "Check for updates" then "Apply update": pulls a signed new `amux-server.py`, snapshots first, restarts (the mtime auto-restart already handles this), runs a post-update health check, and auto-rolls-back on failure. Stable/beta channels.
14. **Pre-update DB snapshot + one-click restore** of `~/.amux/amux.db` (board, sessions, notes, CRM). Local snapshots on every update; cloud deployments already replicate via Litestream→R2.
15. **Apply support patch** — we propose a config/hotfix; the customer sees a diff and approves; audit-logged. No silent changes.

## E. Consent-based remote assist  *(medium–high, privacy-critical)*

16. **Request support session** — customer clicks to mint a time-boxed, scoped, expiring token that gives us a read-only (or approval-gated) view of their dashboard, relayed through the amux Cloud gateway so no inbound ports are needed. Auto-expires; every action is audit-logged and visible to them.
17. **Support-scoped peek** — share one session's live output with us behind a scoped, redacted, expiring link.
18. **Support-to-customer messaging** — push a dashboard banner ("maintenance tonight", "fix deployed"), or with consent send a message into a session.

## F. Audit & compliance  *(required for BAA/HIPAA)*

19. **Support-access audit log** — who accessed what, when, from where; exportable; visible to the customer. Required to stand behind the BAA.
20. **Data-access transparency page** — the customer can always see everything support has touched.
21. **Documented + enforced redaction/DLP** on all export and share paths.

## G. In-product ticketing  *(quick–medium)*

22. **Report an issue** button anywhere — creates a ticket (email or shared board item) with the current diagnostic bundle and view/context auto-attached.
23. **Two-sided board link** — support tickets flow onto the shared board so both sides track status.

---

## Suggested build order

1. **Support bundle + email-to-support + redaction** (§A) — highest leverage, small, uses `?plain=1` and the email API already in place.
2. **Health tab + `amux doctor` + error panel** (§B) — turns "it's broken" into a screenshot we can act on, and deflects tickets.
3. **Opt-in heartbeat + fleet console + alerts** (§C) — go from reactive to proactive across the fleet.
4. **Safe update channel + DB backup/restore** (§D) — deliver fixes and enhancements without a site visit.
5. **Consent-based remote assist + audit log** (§E, §F) — full-fidelity support with BAA-grade transparency.

## Guardrails (apply to all of the above)

- Customer-initiated or anonymized by default; nothing leaves the machine without consent.
- Redaction on every export/share path; preview before send.
- Full, customer-visible audit trail (BAA requirement).
- Single-codebase rule: no `IS_CLOUD` branches; cloud-relayed features light up when the gateway/heartbeat is configured, not via build flags.
