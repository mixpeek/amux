---
description: When editing the Rust server/dashboard — layout and syntax gates
globs: ["crates/**", "Cargo.toml", "Cargo.lock"]
---

amux is a Cargo workspace (the single-file Python server this rule used to govern was
removed 2026-08-09; git history has it):

- `crates/amux-server` — axum server: `src/api/` (one module per API family),
  `src/db/` (SQLite), `migrations/` (SQL, append-only), `src/runtime_jobs/`
- `crates/amux-dashboard` — the SPA as real static files (`static/app.js`,
  `app.css`, `index.html`, `sw.js`) — client JS is NO LONGER inside a Python
  string; edit the static files and bump `APP_VER` (app.js) + `CACHE` (sw.js) together
- `crates/amux-core`, `crates/amux-cli` — shared types and the Rust CLI

Always verify after edits:

```bash
scripts/safe-cargo.sh check --workspace   # syntax/type gate, isolated from this pane's scope (AMUX-70)
node --check crates/amux-dashboard/static/app.js   # after client JS edits
```

Prefer remote offload over any local `cargo` invocation at all (see CLAUDE.md). When local really
is necessary, always go through `scripts/safe-cargo.sh` rather than bare `cargo` — an OOM-killed
cargo process sharing this pane's systemd scope takes the whole interactive session down with it,
not just the build (confirmed via journalctl, AMUX-70).

Before pushing: `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test -p amux-server` — CI (`.github/workflows/rust.yml`) denies warnings.

The PostToolUse hook (`.claude/check-and-commit.sh`) runs `node --check` on dashboard
static JS edits automatically; Rust edits are gated by the builder + CI, so run
`cargo check` yourself before committing batch edits.
