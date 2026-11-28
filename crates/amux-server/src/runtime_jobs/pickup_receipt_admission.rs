//! `[pickup-admission]` - pure pickup admission over version-bound receipts
//! (CW-2983a, parent CW-2983).
//!
//! A pickup receipt records that a card was admitted for pickup while it was
//! at a particular version. A later pickup is admitted only when a receipt for
//! the SAME card was issued at the CURRENT version. A receipt from an older
//! version is stale: the card changed after it was issued, so the admission it
//! records no longer describes the card being picked up.
//!
//! This module is deliberately pure: no database, no clock, no I/O. The caller
//! loads the receipts and the current version, and acts on the decision.

/// One pickup receipt: the card it was issued for and the card version it was
/// issued at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickupReceipt {
    pub card_id: String,
    pub version: u64,
}

/// Why a pickup was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefuseReason {
    /// No receipt exists for this card at all.
    NoReceipt,
    /// Receipts exist for this card, but none was issued at the current
    /// version. `latest` is the highest receipt version seen for the card.
    StaleReceipt { latest: u64, current: u64 },
}

/// The admission decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Admit,
    Refuse(RefuseReason),
}

/// Decide whether `card_id` may be picked up at `current_version`.
///
/// Admit if and only if some receipt for `card_id` has exactly
/// `current_version`. Receipts for other cards are ignored. A receipt newer
/// than `current_version` does not admit: versions only move forward, so a
/// receipt ahead of the card's current version cannot describe it.
pub fn admit(receipts: &[PickupReceipt], card_id: &str, current_version: u64) -> Admission {
    let mut latest: Option<u64> = None;
    for receipt in receipts.iter().filter(|r| r.card_id == card_id) {
        if receipt.version == current_version {
            return Admission::Admit;
        }
        latest = Some(latest.map_or(receipt.version, |l| l.max(receipt.version)));
    }
    match latest {
        None => Admission::Refuse(RefuseReason::NoReceipt),
        Some(latest) => Admission::Refuse(RefuseReason::StaleReceipt {
            latest,
            current: current_version,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(card: &str, version: u64) -> PickupReceipt {
        PickupReceipt {
            card_id: card.to_string(),
            version,
        }
    }

    #[test]
    fn pickup_admission_admits_receipt_at_current_version() {
        let receipts = [r("SW-1", 3)];
        assert_eq!(admit(&receipts, "SW-1", 3), Admission::Admit);
    }

    #[test]
    fn pickup_admission_refuses_card_with_no_receipt() {
        assert_eq!(
            admit(&[], "SW-1", 3),
            Admission::Refuse(RefuseReason::NoReceipt)
        );
    }

    #[test]
    fn pickup_admission_refuses_stale_receipt_and_reports_latest() {
        let receipts = [r("SW-1", 1), r("SW-1", 2)];
        assert_eq!(
            admit(&receipts, "SW-1", 3),
            Admission::Refuse(RefuseReason::StaleReceipt { latest: 2, current: 3 })
        );
    }

    #[test]
    fn pickup_admission_admits_when_any_receipt_matches_among_stale_ones() {
        let receipts = [r("SW-1", 1), r("SW-1", 3), r("SW-1", 2)];
        assert_eq!(admit(&receipts, "SW-1", 3), Admission::Admit);
    }

    #[test]
    fn pickup_admission_ignores_receipts_for_other_cards() {
        let receipts = [r("SW-2", 3)];
        assert_eq!(
            admit(&receipts, "SW-1", 3),
            Admission::Refuse(RefuseReason::NoReceipt)
        );
    }

    #[test]
    fn pickup_admission_refuses_receipt_ahead_of_current_version() {
        let receipts = [r("SW-1", 5)];
        assert_eq!(
            admit(&receipts, "SW-1", 4),
            Admission::Refuse(RefuseReason::StaleReceipt { latest: 5, current: 4 })
        );
    }
}
