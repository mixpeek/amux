# Project lifecycle UI acceptance

Build the branch server with `AMUX_SESSION=codex-lifecycle-adherence scripts/safe-cargo.sh build -p amux-server --bin amux-server`, then copy the binary out of the shared target directory before running:

```
python3 e2e/project-lifecycle/run.py --binary /absolute/path/to/private/amux-server --out /absolute/path/to/results
```

The runner creates a new temporary Amux home, SQLite database, local bare Git remote, source clone, tmux socket and actual HTTPS server. Playwright interacts with the mounted dashboard. Provider outputs are deterministic fixtures; one UI recovery case deliberately aborts its own project read, then removes the fault and uses Retry. No API response is fabricated and no paid model is invoked. Existing Amux instances, browser profiles and production repositories are untouched.

The test covers persisted per-project drafts, duplicate intake, parallel disposable task executors, real commits and verified main integration, safe retirement with readable evidence, pause killing active work, bounded repair, checks-only recovery without a model call, server restart, observed budget/quota holds, malformed intake, desktop/mobile rendering, legacy orchestration history, migration apply/rollback, settings save/cancel, empty/error states, and preservation of dirty work. Git/SQLite/process reads independently verify results; filesystem fault injection is confined to the fixture.

The runner stops its own server and tmux instance even after a failed test. It retains repositories, database, screenshots, logs and `results.json` for inspection. A failure is not automatically retried. Review the failed artifact before another run. HTTPS certificate bypass is limited to the disposable Playwright browser; it does not change user browser settings.

The fixture validates the harness contract. It does not prove a live provider's reasoning quality, token savings, or behavior across every model. Unit/integration suites cover additional claim, scope, concurrency and Git failure boundaries.

## Opt-in real Codex transport

For a separately authorized real-provider run, use `serve.py --live-codex` with
an explicit private home, port and built binary. This installs no fake provider,
uses the existing Codex account authentication, and defaults the helper to
`gpt-6-luna`. Select Codex / `gpt-6-luna` / low for both project profiles and for
each worker in the UI. The fixture creates its own repository, bare remote and
tmux socket. Record message receipts **and** worker-produced files: accepted or
queued is not proof that the model consumed a message. This opt-in consumes the
account's model allowance; the default fixture remains deterministic and free.
