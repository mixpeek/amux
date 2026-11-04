<!-- Moved verbatim from root CLAUDE.md on 2026-10-30 (VISION.md A10). -->
## Reading CI: use the App token, and CHECK that you got it

`gh` here defaults to the user identity (`esteininger`, id 15973166) on a **5000/hr**
budget shared by every amux lane. A GitHub App is already provisioned on this box
and sits on its own larger budget:

```bash
eval "$(~/.amux/github-app/get-token.sh)"
gh api rate_limit --jq '.resources.core | "limit=\(.limit) remaining=\(.remaining)"'
```

**The limit number is the check, and it costs nothing** because `rate_limit` is not
itself counted. `limit=5000` means you are on the shared user budget; anything
higher means you got the App. Measured 2026-09-01: bare `gh` reported
`limit=5000 remaining=4999`, the App `limit=8700 remaining=8625`.

Run the check, do not assume the eval worked. `get-token.sh`'s own docstring records
why: if the script exits non-zero the `eval` sets nothing, `gh` falls back to user
auth, and you are back on the contended budget with no sign that anything happened.

**And the token EXPIRES, so one check at the start is not a check for a long job.**
Measured 2026-09-22: a fresh `eval` returned `--expires-in 2095`, about 35 minutes.
Anything that outlives that (a CI watcher, a gate run, a slow push) starts getting
`401 Bad credentials` partway through, and the shape of that failure is the danger:
a 401 body contains no job names, so a loop asking "are any checks still pending?"
counts zero and reads it as FINISHED. A watcher that cannot see must say so, never
report all-clear. Require a positive signal, like a non-empty `check_runs` array,
before you believe a verdict.

`get-token.sh` already solves the common case and neither flag is named above,
which is why this paragraph exists:

```bash
eval "$(~/.amux/github-app/get-token.sh --min-remaining 1800)"  # token good for >=N more seconds
~/.amux/github-app/get-token.sh --expires-in                    # seconds remaining, no token
```

**`--min-remaining` REFRESHES, it does not refuse**, whatever the script's own
one-line summary says: if the cached token has under N seconds left it skips the
cache and mints a new one, so it exits 0 with a token either way. Measured today,
`--min-remaining 999999` took `--expires-in` from 2073 to 2965 rather than failing.
Do not treat a non-zero exit as the signal that your token was too old, because
there is not one.

The ceiling is about 50 minutes, since the cache refreshes at that mark. A job
that can outlive that gets no help from any single eval: re-eval on every pass
instead of once at the top.

**Do not poll `gh` in a loop.** Secondary limits are per-account and trigger on
request RATE, so one lane's 30s `until` loop 403s every other lane, on every
endpoint, including a plain repo read. On 2026-09-01 two lanes lost CI visibility
for hours that way, and `gh api rate_limit` reported `used: 0, remaining: 5000`
throughout, because the counter it reads is not the counter being enforced. Use
ScheduleWakeup, or one delay sized to the job. (AF-396)
