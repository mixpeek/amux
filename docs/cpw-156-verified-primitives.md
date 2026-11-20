# CPW-156 verified primitives

Status: source and automated tests only. This change has not been installed, deployed, restarted, or enabled against production workers. No live worker was stopped.

## Session-bound stop

Source: `crates/amux-server/src/api/session_verbs.rs` and `crates/amux-server/src/api/session_pidfd.rs`.

Callable contract:

```http
POST /api/sessions/{name}/stop-bound
Content-Type: application/json

{"session_ids":["<current cc_conversation_id>","<current codex_session_id>"]}
```

Include each non-empty ID currently persisted for the session, and omit IDs that are empty. The set must be non-empty, unique, and exactly equal to the current persisted identity set. Read that set again before each retry. A changed identity returns HTTP 409 without signaling; invalid input returns HTTP 400. An unsupported herdr backend returns HTTP 501. A successful stop returns HTTP 200 with `{"ok":true,"stopped":true}`.

The handler serializes lifecycle operations for the session, resolves exactly one tmux pane, then holds the metadata writer lock through identity validation and signaling. PIDFD handles plus `/proc` start-time checks prevent signaling a reused PID. It freezes the pane shell while discovering and terminating descendants; the shell remains alive. Signal and tmux lookup failures return 503 or 409 without clearing or rewriting metadata.

A retry after server recovery is safe when the caller reloads the durable IDs: there is no transient plan token. Replaying an old identity after replacement is refused.

## Revision-guarded reassignment

Source: `crates/amux-server/src/api/board.rs`, `POST /api/board/{id}/conditional-patch`, integrated from the reviewed conditional patch at `/models-fast/lmqr/worktrees/cw1492-livebase-w3-01a1c037` (integration SHA `0a24c28f0406fc46d46151143528c6559d9eacee`).

Callable contract:

```http
POST /api/board/{id}/conditional-patch
Content-Type: application/json

{"expect_revs":{"<target-id>":17,"<dependency-id>":4},"patch":{"session":"new-owner"}}
```

`expect_revs` must include the target and every card whose revision informed the decision. The server checks the whole read set under the board writer transaction before applying the ordinary patch. A missing or changed row returns a conflict with no partial mutation. There is no `not_after` deadline field: the conditional-patch endpoint rejects it with 400, so callers enforce evidence freshness themselves before calling. The patch cannot override guard fields. Existing gates, ownership checks, and normal patch semantics still apply.

## Verification

Focused tests:

- `cargo test -p amux-server bound_stop_rejects_replaced_identity_and_retry_targets_current_process -- --nocapture`
- `cargo test -p amux-server api::session_pidfd -- --nocapture`
- `cargo test -p amux-server --test board_api conditional_patch_ -- --nocapture`

The board API tests cover a competing caller, stale reassignment preserving the new owner, ABA, a terminal transition between snapshot and write, and no partial mutation. The session tests cover identity replacement between planning and stop, recovery from the durable identity record, retry using the recovered current identity, PID reuse refusal, and retained pane-shell behavior. A separate server restart was not run; the stop operation keeps no plan state outside the durable IDs.
