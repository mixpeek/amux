# Breaking up the god files: the method

Owner, 2026-10-30: "do it all", the vision plan's step 5 (`docs/design/vision-and-rule-tree.md`).
This is the method. Running it is separate work, one card per move, under the slop ratchet
(`scripts/check_slop_ratchet.py`), which warns whenever a god file grows.

## Why

On origin/main, 2026-10-30, 15 files are past the god-file size (`check_slop_ratchet.py
--report`). Two of them hold most of the code an agent reads:

- `crates/amux-dashboard/static/app.js`: 73,436 lines, 3,711 top-level functions, 289 section
  banners. An agent working on Messages reads a file that also holds the board, the editor, the
  recorder and the vault.
- `crates/amux-server/src/api/session_verbs.rs`: 57,468 lines. Sends, starts, peeks, steering,
  peers and status all live in one module.

Every change to them collides with every other lane's, every review reads them in fragments, and
every unit test in them recompiles together.

## The target

- **Server.** One module per verb family under `src/api/session/`: `send.rs`, `start.rs`,
  `peek.rs`, `steer.rs`, `peers.rs`, `status.rs`, `history.rs`, each with its own tests. This is
  the shape `src/api/` already uses for most other API families. Shared helpers (`tmux()`,
  `load_meta`, `send_dedup_*`) move to `src/api/session/common.rs`.
- **Dashboard.** One file per view or surface under `static/js/`: `messages.js`, `board.js`,
  `files.js`, `mdx.js`, `peek.js`, `settings.js` and so on. They are loaded by `index.html` as
  classic scripts, in today's order, so they share globals exactly as the one file does today.
  Moving to ES modules is a later step, per file, once its globals are known.

## The order

Leaf first, shared last. A leaf is a section nothing else calls into, which its banner's
neighbours make easy to find: the recorder, the vault, Draw, MDAI, the markdown editor. Shared
code moves last: state, the outbox, rendering, `api`.

## One move, as a card

1. **Inventory.** List the section's top-level names and every caller of each, using
   `state/control-registry.json` and `rg`. A name called from outside the section stays visible
   as a global.
2. **Move it verbatim.** Move the whole section, byte for byte, into its file. Add the
   `<script>` tag in `index.html` at the same point in the load order, and add the file to
   `sw.js`'s cached assets. No other change is allowed in the same commit, so the diff is a pure
   move. `git diff --color-moved` shows it.
3. **Prove nothing changed.** These must pass:
   - `node --check` on both files;
   - `scripts/spa-lint.sh` (its globals list is generated, so an undefined name fails);
   - `node scripts/button-audit.mjs --check` and `node scripts/build-state.mjs --check`;
   - the e2e specs for that surface, on desktop, mobile and ios-safari;
   - `tests/dashboard_assets.rs`, which checks every name called exists and none is defined
     twice.
4. **Then improve it,** in later commits: replace inline styles with classes, log or propagate
   empty catches, add missing tests. The ratchet keeps each move from going backwards.

For the server the same four steps apply, with `cargo check`, `clippy -D warnings`,
`scripts/test-contended.sh -p amux-server` and the integration tests as the proof. Steps 2 and 3
can only move items whose callers can see the new path, so a move updates the `use` lines it
needs and nothing else.

## Done

A move is done when the section's file is on main, the old file shrank by exactly the moved
lines, and the proof passed on CI. The method is finished when `--report` shows no file past the
god-file size, or every remaining one has a written reason to stay.
