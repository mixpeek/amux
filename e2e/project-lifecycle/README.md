# Project lifecycle UI acceptance

Build the branch server with `AMUX_SESSION=codex-lifecycle-adherence scripts/safe-cargo.sh build -p amux-server --bin amux-server`, then copy the binary out of the shared target directory before running:

```
python3 e2e/project-lifecycle/run.py --binary /absolute/path/to/private/amux-server --out /absolute/path/to/results
```

The runner creates a new temporary Amux home, SQLite database, local bare Git remote, source clone, tmux socket and actual HTTPS server. Playwright interacts with the mounted dashboard; it does not intercept or mock API requests. Only provider outputs are deterministic fixtures. No paid model is invoked. Existing Amux instances, browser profiles and production repositories are untouched.

The test covers persisted drafts, duplicate intake, independent fan-out, real commits and verified main integration, safe retirement, pause killing active work, bounded repair, server restart, observed budget holds, malformed intake, desktop/mobile rendering, global orchestration, migration apply/rollback, and preservation of dirty work. Git/SQLite/process reads independently verify results; filesystem fault injection is confined to the fixture.

The runner stops its own server and tmux instance even after a failed test. It retains repositories, database, screenshots, logs and `results.json` for inspection. A failure is not automatically retried. Review the failed artifact before another run. HTTPS certificate bypass is limited to the disposable Playwright browser; it does not change user browser settings.

The fixture validates the harness contract. It does not prove a live provider's reasoning quality, token savings, or behavior across every model. Unit/integration suites cover additional claim, scope, concurrency and Git failure boundaries.
