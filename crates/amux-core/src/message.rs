//! Message as a durable entity (RR-0010, Invariant 29).
//!
//! Steering messages, `@worker` mentions, task discussion, and offline
//! commands are all the same thing: a Message. Making it a durable entity —
//! not command plumbing — gives threads, delivery tracking, search, and
//! audit history without building each separately, and it is why
//! `WorkerCommand::DeliverMessage(MessageId)` carries a REFERENCE: the text
//! lives here, in a store that survives restarts. The Python system lost
//! pending steering text on every restart because the text lived in the
//! delivery pipe.
//!
//! Delivery state moves FORWARD ONLY (Queued -> Delivered -> Acknowledged ->
//! ActedOn). A backwards move would be rewriting delivery history, and the
//! delivery record is part of the audit trail (Invariant 24's spirit applied
//! to messages: 2026-07-27, a delivered message was reported as swallowed —
//! the delivery record is what settles that argument, so it must not be
//! editable backwards).

use crate::events::Actor;
use crate::ids::{GroupId, MessageId, TaskId, WorkerId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a message is addressed. A group target is expanded by
/// [`fan_out`] into per-worker child messages — delivery is always tracked
/// per recipient, never "the group probably saw it".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum MessageTarget {
    Worker(WorkerId),
    Group(GroupId),
    /// The human owner (surfaced in the dashboard/notification lanes).
    Human,
}

/// How far a message has travelled. Each stage past `Queued` records WHEN —
/// supplied by the caller, since core never reads a clock. `ActedOn` may
/// name the task the recipient opened as a result, closing the loop between
/// a steer and the work it caused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeliveryState {
    /// Durable, awaiting the recipient's next turn boundary (Invariant 34:
    /// the message queue never dead-letters — messages are durable and wait).
    Queued,
    /// Handed to the recipient.
    Delivered { at: DateTime<Utc> },
    /// The recipient confirmed reading it.
    Acknowledged { at: DateTime<Utc> },
    /// The recipient did something because of it.
    ActedOn {
        at: DateTime<Utc>,
        task: Option<TaskId>,
    },
}

impl DeliveryState {
    /// Progress rank; delivery only ever moves to a HIGHER rank.
    fn rank(&self) -> u8 {
        match self {
            DeliveryState::Queued => 0,
            DeliveryState::Delivered { .. } => 1,
            DeliveryState::Acknowledged { .. } => 2,
            DeliveryState::ActedOn { .. } => 3,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            DeliveryState::Queued => "queued",
            DeliveryState::Delivered { .. } => "delivered",
            DeliveryState::Acknowledged { .. } => "acknowledged",
            DeliveryState::ActedOn { .. } => "acted_on",
        }
    }
}

/// A backwards (or sideways) delivery transition — rewriting delivery
/// history. Carries both sides so the refusal is diagnosable from itself.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("delivery state only moves forward: {from} -> {to} rejected")]
pub struct DeliveryError {
    pub from: &'static str,
    pub to: &'static str,
}

/// A durable message (Invariant 29). `thread` links a reply (or a fan-out
/// child) to its parent, which is how a worker's reply to a steering message
/// shows up in the task activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    /// Stamped by the system at the boundary, never taken from body text
    /// (AMUX-1768: provenance is the stamp, not the signature).
    pub from: Actor,
    pub to: MessageTarget,
    pub body: String,
    /// Parent message, when this is a reply or a fan-out child.
    pub thread: Option<MessageId>,
    pub created_at: DateTime<Utc>,
    pub delivery: DeliveryState,
}

impl Message {
    /// A fresh message starts `Queued` — there is no way to mint one
    /// already-delivered, because a delivery record nobody performed would
    /// be a claimed-but-not-real audit trail (ethos rule 6).
    pub fn new(
        id: MessageId,
        from: Actor,
        to: MessageTarget,
        body: impl Into<String>,
        thread: Option<MessageId>,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            from,
            to,
            body: body.into(),
            thread,
            created_at,
            delivery: DeliveryState::Queued,
        }
    }

    /// Advance delivery. Forward-only: the new state's rank must be
    /// strictly greater than the current one. Skips are legal (an
    /// acknowledgement observed out-of-band implies delivery); moving
    /// backwards or re-stamping the same stage is not, because either
    /// rewrites the delivery record.
    pub fn advance_delivery(&mut self, next: DeliveryState) -> Result<(), DeliveryError> {
        if next.rank() <= self.delivery.rank() {
            return Err(DeliveryError {
                from: self.delivery.name(),
                to: next.name(),
            });
        }
        self.delivery = next;
        Ok(())
    }
}

/// Expand a group-addressed message into one child message per member,
/// threaded to the parent (Invariant 29 + Invariant 12: groups are
/// first-class, and group delivery is tracked PER RECIPIENT — five members
/// means five delivery records, not one optimistic "sent").
///
/// `mint` supplies fresh [`MessageId`]s because core is pure: ID randomness
/// comes from the caller (the server mints ULIDs; tests pass fixed ones).
/// Children copy the parent's `from`, `body`, and `created_at` — the
/// fan-out is distribution, not authorship, so nothing about the content or
/// its provenance changes.
pub fn fan_out(
    msg: &Message,
    group_members: &[WorkerId],
    mint: &mut dyn FnMut() -> MessageId,
) -> Vec<Message> {
    group_members
        .iter()
        .map(|member| Message {
            id: mint(),
            from: msg.from.clone(),
            to: MessageTarget::Worker(member.clone()),
            body: msg.body.clone(),
            thread: Some(msg.id.clone()),
            created_at: msg.created_at,
            delivery: DeliveryState::Queued,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ulid(s: &str) -> ulid::Ulid {
        s.parse().unwrap()
    }

    fn t(secs: u32) -> DateTime<Utc> {
        // Seconds offset from a fixed epoch — callers pass values >= 60, so
        // this must be arithmetic, not a clock-field constructor.
        chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
            + chrono::Duration::seconds(secs as i64)
    }

    fn human() -> Actor {
        Actor::Human {
            name: "ethan".into(),
        }
    }

    fn msg_to_worker() -> Message {
        Message::new(
            MessageId::from_ulid(ulid("01JGXV0000000000000000AAAA")),
            human(),
            MessageTarget::Worker(WorkerId::from_ulid(ulid("01JGXV0000000000000000TEST"))),
            "please review AR-42",
            None,
            t(0),
        )
    }

    // -- CRUD-shape construction -------------------------------------------

    #[test]
    fn construction_starts_queued_and_round_trips() {
        let m = msg_to_worker();
        assert_eq!(m.delivery, DeliveryState::Queued);
        assert_eq!(m.body, "please review AR-42");
        assert_eq!(m.thread, None);
        let json = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    // -- thread linking -----------------------------------------------------

    #[test]
    fn reply_links_to_parent_thread() {
        let parent = msg_to_worker();
        let reply = Message::new(
            MessageId::from_ulid(ulid("01JGXV0000000000000000BBBB")),
            Actor::Worker {
                id: WorkerId::from_ulid(ulid("01JGXV0000000000000000TEST")),
            },
            MessageTarget::Human,
            "done, see the card",
            Some(parent.id.clone()),
            t(60),
        );
        assert_eq!(reply.thread.as_ref(), Some(&parent.id));
    }

    // -- delivery state transitions (forward only) --------------------------

    #[test]
    fn delivery_advances_through_full_chain() {
        let mut m = msg_to_worker();
        m.advance_delivery(DeliveryState::Delivered { at: t(1) })
            .unwrap();
        m.advance_delivery(DeliveryState::Acknowledged { at: t(2) })
            .unwrap();
        m.advance_delivery(DeliveryState::ActedOn {
            at: t(3),
            task: Some(TaskId::from_ulid(ulid("01JGXV0000000000000000CCCC"))),
        })
        .unwrap();
        assert!(matches!(m.delivery, DeliveryState::ActedOn { .. }));
    }

    #[test]
    fn delivery_can_skip_forward_but_never_backward() {
        // Skip: an ack observed out-of-band implies delivery.
        let mut m = msg_to_worker();
        m.advance_delivery(DeliveryState::Acknowledged { at: t(5) })
            .unwrap();

        // Backward: rewriting delivery history is refused.
        let err = m
            .advance_delivery(DeliveryState::Delivered { at: t(6) })
            .unwrap_err();
        assert_eq!(err.from, "acknowledged");
        assert_eq!(err.to, "delivered");

        // Same-rank re-stamp is also a rewrite (a second "acknowledged at"
        // would replace the first, and the first is the record).
        assert!(m
            .advance_delivery(DeliveryState::Acknowledged { at: t(7) })
            .is_err());

        // Queued is rank 0; nothing returns there.
        assert!(m.advance_delivery(DeliveryState::Queued).is_err());
    }

    // -- group fan-out -------------------------------------------------------

    #[test]
    fn fan_out_produces_one_child_per_member_threaded_to_parent() {
        let parent = Message::new(
            MessageId::from_ulid(ulid("01JGXV0000000000000000AAAA")),
            human(),
            MessageTarget::Group(GroupId::from_ulid(ulid("01JGXV0000000000000000TEST"))),
            "standup in 5",
            None,
            t(0),
        );
        let members = vec![
            WorkerId::from_ulid(ulid("01JGXV0000000000000000BBBB")),
            WorkerId::from_ulid(ulid("01JGXV0000000000000000CCCC")),
            WorkerId::from_ulid(ulid("01JGXV0000000000000000DDDD")),
        ];
        // Deterministic mint: fixed ULIDs, one per child (core is pure; the
        // caller owns randomness).
        let child_ulids = [
            "01JGXV0000000000000000MAAA",
            "01JGXV0000000000000000MBBB",
            "01JGXV0000000000000000MCCC",
        ];
        let mut next = 0usize;
        let mut mint = || {
            let id = MessageId::from_ulid(ulid(child_ulids[next]));
            next += 1;
            id
        };

        let children = fan_out(&parent, &members, &mut mint);

        assert_eq!(children.len(), members.len());
        for (child, member) in children.iter().zip(&members) {
            assert_eq!(child.to, MessageTarget::Worker(member.clone()));
            assert_eq!(child.thread.as_ref(), Some(&parent.id));
            assert_eq!(child.body, parent.body);
            assert_eq!(child.from, parent.from);
            assert_eq!(child.created_at, parent.created_at);
            // Each child gets its own delivery record, starting at Queued.
            assert_eq!(child.delivery, DeliveryState::Queued);
            assert_ne!(child.id, parent.id);
        }
        // Children have distinct identities: per-recipient tracking, not
        // one shared row.
        assert_ne!(children[0].id, children[1].id);
        assert_ne!(children[1].id, children[2].id);
    }

    #[test]
    fn fan_out_of_empty_group_is_empty() {
        let parent = msg_to_worker();
        let mut mint = || -> MessageId { unreachable!("no members, no minting") };
        assert!(fan_out(&parent, &[], &mut mint).is_empty());
    }
}
