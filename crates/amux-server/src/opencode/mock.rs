//! MockProtocol: the deterministic AgentProtocol for orchestrator
//! simulation (Invariant 22) and the conformance suite (RR-0030).
//!
//! Tests script the agent's behavior; the orchestrator under test cannot
//! tell the difference — which is the point. Every trait method records its
//! call so tests assert on WHAT WAS ASKED, not on side effects.

use super::{AgentProtocol, AgentState, Prompt, ProtocolError, Result};
use amux_core::ids::{MessageId, WorkerId};
use amux_core::protocol::WorkerEvent;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Mutex;
use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq)]
pub enum RecordedCall {
    SendPrompt {
        worker: WorkerId,
        prompt: Prompt,
    },
    /// `body` is recorded so tests can assert the REAL message text reached
    /// the agent — delivering the right MessageId with an empty body is
    /// exactly the bug RR-0066 fixes, and a recorder blind to the body
    /// could not fail on it (ethos rule 7).
    DeliverMessage {
        worker: WorkerId,
        msg: MessageId,
        body: String,
    },
    Cancel(WorkerId),
    Pause(WorkerId),
    Resume(WorkerId),
}

struct WorkerSim {
    state: AgentState,
    events: broadcast::Sender<WorkerEvent>,
}

#[derive(Default)]
pub struct MockProtocol {
    workers: Mutex<BTreeMap<WorkerId, WorkerSim>>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl MockProtocol {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a worker with an initial state. Without registration, every
    /// protocol call fails with NoSession — matching the real transport's
    /// behavior for a worker whose process is gone.
    pub fn register(&self, worker: WorkerId, state: AgentState) {
        let (tx, _) = broadcast::channel(256);
        self.workers
            .lock()
            .unwrap()
            .insert(worker, WorkerSim { state, events: tx });
    }

    /// Test-side control: change a worker's state and optionally emit the
    /// event a real agent would have produced alongside.
    pub fn set_state(&self, worker: &WorkerId, state: AgentState, event: Option<WorkerEvent>) {
        let mut ws = self.workers.lock().unwrap();
        if let Some(sim) = ws.get_mut(worker) {
            sim.state = state;
            if let Some(ev) = event {
                let _ = sim.events.send(ev);
            }
        }
    }

    pub fn emit(&self, worker: &WorkerId, event: WorkerEvent) {
        let ws = self.workers.lock().unwrap();
        if let Some(sim) = ws.get(worker) {
            let _ = sim.events.send(event);
        }
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: RecordedCall) {
        self.calls.lock().unwrap().push(call);
    }

    fn ensure(&self, worker: &WorkerId) -> Result<()> {
        if self.workers.lock().unwrap().contains_key(worker) {
            Ok(())
        } else {
            Err(ProtocolError::NoSession(worker.to_string()))
        }
    }
}

#[async_trait]
impl AgentProtocol for MockProtocol {
    async fn send_prompt(&self, worker: &WorkerId, prompt: Prompt) -> Result<()> {
        self.ensure(worker)?;
        self.record(RecordedCall::SendPrompt {
            worker: worker.clone(),
            prompt,
        });
        Ok(())
    }

    async fn deliver_message(&self, worker: &WorkerId, msg: MessageId, body: String) -> Result<()> {
        self.ensure(worker)?;
        self.record(RecordedCall::DeliverMessage {
            worker: worker.clone(),
            msg,
            body,
        });
        Ok(())
    }

    async fn cancel(&self, worker: &WorkerId) -> Result<()> {
        self.ensure(worker)?;
        self.record(RecordedCall::Cancel(worker.clone()));
        Ok(())
    }

    async fn pause(&self, worker: &WorkerId) -> Result<()> {
        self.ensure(worker)?;
        self.record(RecordedCall::Pause(worker.clone()));
        Ok(())
    }

    async fn resume(&self, worker: &WorkerId) -> Result<()> {
        self.ensure(worker)?;
        self.record(RecordedCall::Resume(worker.clone()));
        Ok(())
    }

    async fn state(&self, worker: &WorkerId) -> Result<AgentState> {
        let ws = self.workers.lock().unwrap();
        ws.get(worker)
            .map(|s| s.state.clone())
            .ok_or_else(|| ProtocolError::NoSession(worker.to_string()))
    }

    fn events(&self, worker: &WorkerId) -> broadcast::Receiver<WorkerEvent> {
        let ws = self.workers.lock().unwrap();
        match ws.get(worker) {
            Some(sim) => sim.events.subscribe(),
            None => {
                // A subscription to a dead worker yields a closed channel —
                // the subscriber sees Closed immediately instead of hanging.
                let (tx, rx) = broadcast::channel(1);
                drop(tx);
                rx
            }
        }
    }
}
