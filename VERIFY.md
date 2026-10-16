# VERIFY.md — what counts as proof, per surface

The board refuses `done` without evidence (AF-321). This file is what that
refusal points at: for each surface, the literal thing you run, and what a pass
looks like.

It exists because Ethan dictated the how-to-verify individually at least seven
times in 34 hours ("make sure u verify test etc", "verify via browser when
they're done to ensure everything renders properly", "did you verify every
single thing ... against their studio ui and verify it all e2e"). Written down
once, it reaches every lane by default, which is ethos rule 1.

**Paste the command AND its result line into `--evidence`.** A command with no
result is a claim that you ran it.

## Rust server (`crates/amux-server`)

```bash
CARGO_TARGET_DIR=~/.amux/rust-build-target cargo clippy --workspace --all-targets -- -D warnings
scripts/test-contended.sh -p amux-server
```

Pass: clippy exits 0, and `test result: ok` with a non-zero pass count.

**`--lib` IS A PARTIAL RUN THAT READS LIKE A FULL ONE.** `cargo test -p amux-server
--lib` skips every `tests/*.rs` integration target — 1,625 tests pass and the
number looks total. The wrapper runs all of them (1,831 on the same tree). On
2026-08-30 a board change shipped a fleet-wide blank-preview regression on a
`--lib` green; the guard that catches it, `list_is_slim_by_default_and_serves_
prose_only_on_request`, lives in `tests/board_api.rs` and was never executed.
Running it at that commit fails in 0.16s. The coverage existed; the command did
not reach it.

Use the wrapper, not bare `cargo test`. This box builds amux continuously, so
the auto-builder can rewrite the shared binary while your tests spawn it and the
ETXTBSY family surfaces as failures in modules you never touched. The wrapper
prints whether a build was running, in both directions, which is the clause a
bare green silently omits.

**A green suite is not evidence that your change is covered.** Say which test
fails without your change. `scripts/mutate.sh run <file> <old> <new> -- <cmd>`
applies one exact string, runs the command, and reverts in a trap even if the
command is killed. Do not use `cp file bak` on this shared checkout; it is a
whole-file write and has reverted a peer's in-flight work twice.

## Installer Rust source and artifact isolation (`install.sh`)

```bash
python3 scripts/test-install-committed-source.py
```

Pass: `install committed source: 79 passed, 0 failed`. The actual installer runs
through both Rust binary publications in eleven disposable Git fixtures; the
compiler reads committed/dirty specimen files and installation targets temporary
paths. The fixture stops before Bash CLI installation, hooks, services or databases.
It covers dirty/untracked source, HEAD advance, relative target, unresolved index,
missing Git, failed build, shared-output replacement at compiler return and at
publication, replacement between binaries, and altered private artifact refusal.
Old installed sentinels must survive every refusal. Private stages are cleaned.

Dependencies still use the shared Cargo target. Final executables are linked
straight into a private directory using `cargo rustc -- --emit=link=<private>`;
both publication candidates must match a pinned-commit SHA256 manifest before
any live rename. Each rename is atomic; the pair is not a filesystem transaction.
Source decisions, artifact identities, mismatches, partial publication failures and
successful publication self-announce in `$AMUX_HOME/logs/server-install.log`
(default `~/.amux/logs/server-install.log`). The fixture does not deploy a server
or substitute for a real compiler check of the private link-output path.

## Dashboard client JS (`crates/amux-dashboard/static`)

```bash
node --check crates/amux-dashboard/static/app.js
npm run build:state
bash scripts/spa-lint.sh
npm run test:state
```

Commit the regenerated `static/state/control-registry.json` and `kernel.js`
when handlers change. `spa-lint.sh` checks their freshness before ESLint; a
targeted Playwright configuration can skip that preflight and pass against a
stale interaction bundle. When testing candidate assets against a running
server, set `AMUX_E2E_STATE_KERNEL` (and `AMUX_E2E_CONTROL_REGISTRY` when
handlers changed) alongside the candidate app/HTML/CSS paths.

No version bump: `APP_VER` (`app.js`) and `CACHE` (`sw.js`) hold the
`__AMUX_ASSET_VER__` placeholder, and the server stamps a content hash of
`static/` into both when serving them.

Pass: `node --check` silent, and `curl -sk "$(amux url)/app.js" | grep -m1 'const APP_VER'`
shows a 12-hex-digit hash after the server adopts the commit.

## Anything with a UI

Exercise it in the real UI before `verified`. Tests, API calls and greps justify
`done` at most.

```bash
node skills/chrome-cdp/scripts/cdp.mjs shot <target>
```

Evidence is the screenshot path. **Look at the image.** "make sure u rview the
image and post for clarity when its done this shit is cut off" is a report about
an image that was produced and never opened.

Check it at phone width too: amux is mobile-first, the iPhone PWA is the primary
surface, and layout breaks live at 375px.

## e2e (`e2e/`)

For the combined product lifecycle, use `npm run test:lifecycle` and
[the consolidated acceptance guide](docs/consolidated-lifecycle.md). It retains
per-stage failures, skipped prerequisites, visual evidence, real task completion
and peer coordination. `npm run test:lifecycle:browser` is the browser-only scope.

```bash
npx playwright test --config=e2e/playwright.config.ts e2e/<spec>.spec.ts
```

Pass: the spec name and `N passed`. If e2e infra is genuinely unavailable, say
so in the evidence and why — that is a legitimate `verified` note, not a failure
to hide.

## Vault secrets (`api/vault_secrets.rs`)

```bash
AMUX_CHAOS_BINARY=$CARGO_TARGET_DIR/debug/amux-server node e2e/chaos/vault-secrets.mjs
```

Pass: `VERDICT: PASS` with `28 checks, 0 failed` (the JSON's `n_considered` and
`failed`). It launches real workers under a fake agent that reports only the
SHA-256 of each delivered variable, so it proves the worker > group > global
resolution, the vault winning over a plaintext line, a replaced value arriving
and a deleted key being unset on relaunch, and it sweeps the server log, audit,
store, `ps` argv, pane history and database for any value. It runs on the file
keystore; the Keychain backend is checked once on the live server by adding a
throwaway item and running `security find-generic-password -s amux-vault-master`
WITHOUT `-w`, which prints the item's attributes and never the key.

## A deployed server change

A fix in `git log` is not live until the running binary carries it.

```bash
curl -sk $AMUX_URL/api/health | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("commit"), d.get("build"))'
```

Pass: `commit` matches your sha. Use `commit` for "did source change?" and
`build` for "same process image?" — the builder swaps the binary on any commit,
so bracket every measurement with `build`.

## A scheduled or launchd job

The hand-run is not the scheduled run: the exec environment differs, and that is
where the restic chain broke once already (launchd does not inherit shell PATH).
Evidence is a log line carrying a timestamp from a run nobody started by hand.

## When there is genuinely no artifact

`none: <reason>` (three words or more). An escalation that closed because the
owner decided, or a watch that stood down, produces nothing to link. That answer
is stored verbatim and counted (`evidence LIKE 'none:%'`), which is what makes
it an escape rather than a blind spot.

## The trap this file is downstream of

Before believing a green or a zero, say what a positive would look like and
confirm your probe could produce it.

Two live examples, both from closing AF-324 the same night this file was
written. `restic ls <snapshot> <path>` is **not** recursive, so a grep for churn
paths returned `0` and read as a clean pass when it had listed one directory
level. And the backup's own drift probe prints its counts *beside* its verdict,
because "drift 0" alone is also what a probe that found nothing to look at
would print.

Name what should appear BESIDE the answer if the probe really ran, and check for
THAT: a count beside a zero, a hash beside "adopted", a PASS line beside a green
suite.
