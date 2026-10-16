<!-- Thanks for contributing to amux! Keep PRs small and single-purpose. -->

## What & why

<!-- One or two sentences. What does this change and why? -->

Closes #<!-- issue number -->

## Checklist

- [ ] **One focused change** — single issue, one crate/module where possible
- [ ] **It builds clean:** `cargo check --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` (CI denies warnings)
- [ ] **Tests pass:** `cargo test --workspace`
- [ ] **Client change?** (`crates/amux-dashboard/static/`) `node --check` passes and `npm run build:state` was run (no version bump: the server derives it from content)
- [ ] **Tested end-to-end** — I drove the actual UI/endpoint, not just verified it compiles
- [ ] Rebased on latest `main`

## How I tested

<!-- Commands run, endpoints/UI exercised, before/after screenshots for dashboard changes. -->
