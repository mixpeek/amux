# Real isolated Codex worker → Chat validation

The 2026-10-09 owner-authorized test created `codex-chat-e2e-20261009` on 8824 with `provider=codex`, `isolated=true`, a private synthetic Git fixture, and no model override. Isolation and disabled board automation were written before startup. Codex CLI 0.155.1 completed an actual terminal turn, corrected the stale report from 370 to 295, and reported that AMUX_SESSION, AMUX_WORKER and AMUX_URL were absent. The terminal rollout hash is the noninterference witness; capture it after the terminal finishes and compare it after Chat operations.

## Reproduced failures

The first two actual Chat UI messages failed: the terminal launch explicitly selected Amux's Codex default, while headless Chat inherited the owner's desktop-only `gpt-6.1-sol` global configuration. The ChatGPT CLI account rejected that model. The persisted assistant errors and SSE errors agree; these turns are failures, not passes. A scoped Chat-model override on **this disposable test worker only** exercised the existing explicit-model path; it does not prove the default fix.

Codex also marked its first conversation as `fresh=false`, skipping the companion introduction. Context omitted the parent directory and isolation flag. The model therefore described fixture isolation without confirming terminal isolation. The fix uses the existing terminal provider default, preserves explicit model choices (including `-m`), introduces new/rebuilt Codex conversations, and refreshes the worker's directory/isolation in each context block. `chat_model_terminal_default` and `chat_companion_context` expose those decisions in logs.

## Private patched UI/server rig

`crates/amux-server/examples/chat_companion_e2e_server.rs` serves the production router, Store, TLS resolver and Chat recovery without production fleet-driving jobs. Require a private AMUX_HOME and a nonproduction AMUX_RS_PORT. The rig refuses the live home and production ports. It reads the already configured owner token and trusted TLS material; it does not install trust or change credentials. No live DB is copied. Seed only the disposable test worker's env/meta, referencing its synthetic checkout and native Codex conversation. Remove the test-only AMUX_CHAT_MODEL override in the private copy to exercise the fix. Never start/stop the terminal through this replica.

```sh
CARGO_TARGET_DIR="$HOME/.amux/rust-build-target" CARGO_BUILD_JOBS=1 scripts/safe-cargo.sh build -p amux-server --example chat_companion_e2e_server
AMUX_HOME=/absolute/private-test-home AMUX_RS_PORT=19119 /absolute/immutable-copy-of/chat_companion_e2e_server
```

Open the trusted hostname on the test port with `?peekEmbed=codex-chat-e2e-20261009&peekTab=chat`. The existing owner session works across ports. Do not bypass a certificate interstitial.

## Required evidence

- Real browser compose/send → real Codex exec → SSE/tool events → persisted reply → rendered Chat. No provider stubs, intercepted responses or synthetic UI state count as this proof.
- Conflict/duplicate reconciliation: A's latest revision is cancelled, B's latest amount is 75, C's identical duplicate counts once. Expected total 295, IDs B/C.
- Queue a follow-up while busy. Recall a label supplied only in the preceding prompt. Reload while busy; assert one owner message and one reply per msg_id/turn_id, stable conversation ID, and no stranded queue/outbox.
- Append a synthetic D=40 event; ask Chat to re-read source data without stating the answer. Expected current total 335; saved report remains 295. This distinguishes fresh evidence from memory.
- Interrupt a deliberately slow read-only Chat turn using Stop; require `interrupted=true` in persisted history and successful subsequent conversation continuation.
- SIGKILL **only the private server PID**, during a real Codex turn with a follow-up queued. Restart the same immutable binary/home. Require detached provider adoption, exactly one reply per accepted message, intact queue, same conversation, and no repeated terminal turn.
- Compare terminal rollout hash, conversation/start count, saved report hash and README hash after every case. The only allowed input change is the explicitly appended synthetic event. Capture health/build identities; another lane may deploy to 8824 during a test, so do not claim it held one build throughout.
- Preserve the initial errors and refused build/test attempts as evidence. Exit 75 with NO TEST RAN is build contention, not a red regression or green test.

Focused automated coverage: `scripts/test-contended.sh -p amux-server --lib api::chat_worker`. The model-default, explicit-model and initial/resumed isolation-context cases run without provider billing. For a negative control, use `scripts/mutate.sh` to restore the missing default argument or force the Codex fresh flag off and require the corresponding named test to fail.

## Recorded real-provider run (2026-10-09)

A newly created `codex-chat-e2e-20261009` isolated terminal ran Codex CLI
0.155.1. Its raw owner prompt corrected a deliberately wrong report from 370
to 295. Native conversation `01a122b7-1a91-7bd2-9c48-60481d1e0366` had one
launch and one exact owner-prompt delivery. After adding the explicit test event,
Chat read the current total 335 and identified the stale saved report 295.

The private production-router rig, without a Chat model override, completed
seven owner turns covering queued follow-ups, label recall, reload, live file
reads, server SIGKILL/adoption, deduplicated retries, Stop and continuation.
The SIGKILL occurred with a real provider process alive and one message queued.
Restart retained the same companion conversation, adopted the running provider,
and delivered each accepted prompt/reply once. The native provider rollout
contains each owner marker once, including the pre-crash prompt; persisted
message counts alone were not used to infer no replay.

An initial Stop landed after a 30-second command completed and is excluded
from interruption proof. The repeat stopped a 60-second command after 13.098
seconds, persisted `interrupted=true`, omitted its completion marker, and
continued the same conversation successfully. No companion errors or duplicate
replies occurred in the private seven-turn matrix.

The terminal rollout SHA-256 remained
`2863e079c165fe32945b870e6b1699a4dcef756ca8c8d26b30afc2361265d994`,
with the same thread/start count and unchanged saved report and README. The
first private matrix binary was
`acbe39cfadd1ebf31edbe0458c44109e0869118f1cfc8b775cda3e324261b9bd`;
a subsequent build adds the companion-context log signal, with a final real
UI turn verifying the same behavior and both log signals. This is local PR
validation; it does not claim that the patch was deployed to 8824.

Local evidence is retained under
`outputs/worker-chat-e2e-20261009` in the requesting task workspace: independent
assertion receipts, native-provider delivery counts, SSE events, message/meta
snapshots, health/build snapshots, fault receipts and exact build/test logs.
Credentials and full owner token material are excluded from the receipts.
