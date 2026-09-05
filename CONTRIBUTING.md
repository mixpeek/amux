# Contributing to amux

Thanks for your interest in amux! The product is a Rust workspace (`crates/`) serving an embedded dashboard; contributing is meant to be low-friction.

## Where the code lives

**All new work lands in the Rust server** (`crates/amux-server`, port 8824). If a family of endpoints exists, it answers natively there; the ownership map is [docs/rust-migration/server-boundary.md](docs/rust-migration/server-boundary.md), cross-checked by tests and served live at `GET /api/debug/boundary`.

> **Legacy:** the Python predecessor (`amux-server.py`) has been removed — git history has it, and the Rust server answers 8824, plus the retired 8822 via a compatibility bind that is being removed (`GET /api/debug/legacy-port`). Only `cloud/` still runs the last-built Python image, pending its own Rust migration; do not build anything new on it.

## Local development

```bash
git clone https://github.com/mixpeek/amux && cd amux
./install.sh              # one command: build, install, launchd agent, dashboard on :8824
```

Or run it directly without the service:

```bash
cargo run -p amux-server            # binds 8823 by default (AMUX_RS_PORT overrides)
```

Before pushing:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

CI treats warnings as errors. Integration tests marked `#[ignore]` (the live-model goldens in `golden_live.rs`/`golden_remaining.rs`) spend real agent tokens and are run manually.

While iterating, `cargo check --workspace` is the fast syntax/type gate. If several agents or checkouts share this machine, point `CARGO_TARGET_DIR` at a scratch dir (e.g. `/tmp/amux-target`) so parallel builds don't thrash one lock.

### Editing the dashboard SPA

The SPA is real static files under `crates/amux-dashboard/static/` — no build step, no framework. Two rules:

- **Bump `APP_VER` (`static/app.js`) and `CACHE` (`static/sw.js`) together.** Miss either and a browser holding the cached script never receives your fix — the change is live on the server and invisible to every existing client.
- Syntax-check with `node --check crates/amux-dashboard/static/app.js`. A PostToolUse hook (`.claude/check-and-commit.sh`) does this automatically on agent edits; `node --check` proves the file *parses*, not that every function it calls exists, so still exercise the UI path you touched.

### Shared checkout: check what you are shipping

⚠ Several agent sessions may commit into the same working tree. **Any push of `main` ships every unpushed commit, not just yours** — and a peer's commit can sweep up your in-flight working-tree changes. Before pushing:

```bash
git fetch origin
git rev-list --count origin/main..main    # how many commits am I about to ship?
git log --oneline origin/main..main       # whose are they?
```

If commits you did not write are listed, confirm with their author before pushing. Re-read a shared file immediately before editing it.

## Project layout

| Path | What |
|------|------|
| `crates/amux-server` | The server: API, store, runtime, embedded dashboard |
| `crates/amux-dashboard` | The SPA (embedded at build time via rust-embed) |
| `crates/amux-cli` | `amux-rs`, the CLI |
| `crates/amux-core` | Shared domain types |
| `install.sh` / `uninstall.sh` | One-command setup / teardown |
| `docs/` | Guides, reference, and the rust-migration docs |
| `amux`, `amux-remote` | **Legacy** bash CLIs (HTTP clients of the server; pending `amux-rs` verb parity) |
| `cloud/` | Cloud gateway + VM provisioning (the hosted tunnel/SSO tier) — still runs the last Python image, pending Rust migration |
| `site/` | The marketing site (deployed separately) |
| `ios/`, `android/`, `desktop/` | Native wrappers |

## Adding a database migration

`crates/amux-server/migrations/NNNN_name.sql`, plus an entry in the `MIGRATIONS`
list in `crates/amux-server/src/db/migrate.rs`. A guard
(`versions_are_dense_and_match_their_filenames`) requires the versions to be
**dense and ascending** and each filename to start with its own zero-padded
version, so `0036_thing.sql` must be registered as `version: 36` in the 36th
position.

**If your number collides with one you could not see, that is ours to fix, not
yours.** Pick the next free number against `origin/main` and open the PR. The
maintainer's `main` runs ahead of `origin/main` — often by dozens of commits —
so a number that is genuinely free from where you stand can already be taken in
unpushed work. Nothing you can run detects that: CI builds your branch against
`origin/main`, where there is no conflict, so the collision surfaces only when a
maintainer merges.

This has already happened twice on one PR (#160). The second time, the
contributor had renumbered correctly against `origin/main` and a maintainer took
the new number hours later. Renumbering is a one-line change we can make at merge
time; discovering the rule by having a PR bounce twice is not a reasonable thing
to ask of you.

So: do not hunt for a "safe" high number, and do not treat a renumber request as
a mistake you made. If we ask, it is bookkeeping.

## Making changes

1. **Branch** off `main`; keep the change focused — one logical change per PR.
2. **Tests ride with the change.** Ports of Python behavior pin the Python contract (see the golden/live-oracle tests in `crates/amux-server/tests/` for the pattern); new behavior gets its own coverage. A check that cannot fail is theatre.
3. **Verify end-to-end**, not just that it compiles — drive the actual UI/endpoint you changed.
4. The dashboard is a mobile-first PWA: new UI must fit the `@media (max-width: 600px)` breakpoints, keep touch targets ≥ 44×44, and use `env(safe-area-inset-*)` at screen edges.
5. Match the surrounding style; the dashboard has no build step or framework.

## Building the harness roadmap

amux is becoming the **durable operating system around agents**: it owns execution, state, isolation, recovery, observability, and verification — the model owns reasoning. The roadmap lives in **[the roadmap epic](https://github.com/mixpeek/amux/issues/46)** and its linked issues.

### Seams first, then leaves

- **Seams (maintainer-owned):** the interfaces everything else plugs into — the [agent runtime contract + capability registry](https://github.com/mixpeek/amux/issues/47) and [event-sourced worker state](https://github.com/mixpeek/amux/issues/48).
- **Leaves (great for contributors):** once a seam exists, the work it unlocks is parallel and self-contained — **provider adapters, verification runners, MCP tools, eval scenarios, policy hooks**. If a leaf issue is blocked on a seam, it says so.

If you want to help, comment on a `help wanted` roadmap issue to claim it, or propose a smaller sub-task on the epic.

## Reporting bugs / ideas

Open an issue with what you did, what you expected, and what happened. Screenshots of the dashboard help. Security-sensitive reports: please disclose privately per [SECURITY.md](SECURITY.md).

## License

By contributing, you agree your contributions are licensed under the repository's [LICENSE](LICENSE) (MIT + Commons Clause).
