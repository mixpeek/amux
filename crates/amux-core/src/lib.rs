//! amux-core: the system's vocabulary. Shared types, scope resolution, board
//! state machine, command/event protocol. Pure logic — no I/O, no async, no
//! database. Everything here is testable with plain `cargo test`.
//!
//! Module ↔ RR-item map (docs/rust-rebuild-plan.md §Execution Checklist):
//! - `scope`         RR-0002 (Invariant 2)
//! - `worker`        RR-0003, RR-0018 (Invariants 1, 43)
//! - `session`       RR-0004 (Invariants 1, 33, 8)
//! - `board`         RR-0005, RR-0011 (Invariants 3, 4, 18, 19)
//! - `protocol`      RR-0006 (Invariants 5, 34)
//! - `provider`      RR-0007 (Invariants 8, 20)
//! - `revision`      RR-0008 (Invariants 35, 37)
//! - `events`        RR-0009 (Invariant 24)
//! - `message`       RR-0010 (Invariant 29)
//! - `stall`         RR-0012 (Invariant 10)
//! - `turn`          RR-0013 (Invariants 6, 16, 27)
//! - `memory`        RR-0014 (Invariant 42)
//! - `verification`  RR-0015 (Invariants 7, 28)
//! - `search`        RR-0017 (Invariants 32, 40)
//! - `limits`        RR-0028f (Invariants 47, 49)
//! - `circuit`       RR-0028h (Invariants 48, 45, 10)
//! - `provider_fleet` RR-0044b (Invariants 20, 22)
//! - `criteria`      RR-0028i (Invariant 50)
//!
//! Deleted 2026-08-11 (AMUX-2906, Ethan: "you don't need me for any of these
//! just do it"): `capability`, `group`, `mention`, `isolation` — port-era
//! scaffolding with zero references from any crate for the life of the Rust
//! server. If the AgentRuntime seam (ethos D6 exit) wants them back, git
//! history at this commit^ has all four intact; re-adopting from history is
//! cheaper than carrying 1,100 dead lines every build. `stall` looked like
//! the same case and is NOT: board.rs and orchestrator.rs import it.

pub mod board;
pub mod circuit;
pub mod criteria;
pub mod events;
pub mod harness;
pub mod ids;
pub mod limits;
pub mod memory;
pub mod message;
pub mod orchestrator;
pub mod policy;
pub mod project;
pub mod protocol;
pub mod provider;
pub mod provider_fleet;
pub mod revision;
pub mod scope;
pub mod search;
pub mod session;
pub mod stall;
pub mod task_graph;
pub mod turn;
pub mod verification;
pub mod worker;
pub mod workflow;
