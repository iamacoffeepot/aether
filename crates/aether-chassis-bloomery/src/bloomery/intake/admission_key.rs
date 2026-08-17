//! The nonce-keyed admission vocabulary: every idempotency key an admitted
//! dispatch result can be journaled under.
//!
//! Every fact an evidence upload turns into is keyed by the nonce of the order
//! that produced it, so a journal row keyed to a nonce is the durable statement
//! that *that dispatch has been accounted for*. Two readers depend on the set
//! being exactly right and in opposite directions: [`admit_uploaded`] mints one
//! key per admission, and the boot-time strand check reads the whole set to ask
//! whether any of them landed. A key spelled in one place and not the other
//! would make a completed dispatch look stranded (re-running a lane that already
//! finished) or a stranded one look complete (the silent park issue #4956 is
//! about) — so the vocabulary is enumerated once here and both sides go through
//! it.
//!
//! [`admit_uploaded`]: super::admit_uploaded

use aether_bloomery::IdempotencyKey;

/// Which admission a nonce-keyed journal row records.
///
/// Closed on purpose: the set is what makes "no journal row names this nonce" a
/// sound statement, so a new admission shape must be added here rather than
/// spelling its own key at the admit site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionKey {
    /// A member-line attempt completion (Construct / Refine / Reconcile).
    Attempt,
    /// A passing terminal `Verify`, which admits as the member's integration.
    Integrate,
    /// A failing terminal `Verify`, carrying its typed verifier identities.
    VerifyFailed,
    /// An attempt that parked on a question (ADR-0151).
    Park,
    /// A whole-bloom aggregate-review verdict (ADR-0153).
    AggregateReview,
    /// An aggregate review whose executor could not judge the fold (ADR-0176).
    AggregateReviewExecutorFault,
    /// A whole-bloom aggregate-verify verdict.
    AggregateVerify,
    /// A member stage whose executor could not judge the subject (ADR-0195).
    MemberExecutorFault,
    /// A study-record evidence admission. Not a dispatch-accounting key: a
    /// study row landing must not mark the dispatch complete — the verdict
    /// is what consumes the order. Excluded from [`Self::ALL`].
    Study,
}

impl AdmissionKey {
    /// Every *dispatch-accounting* admission shape. A journal row under one
    /// of these is the durable statement that the dispatch reached the
    /// reducer as a verdict. [`Self::Study`] is deliberately absent: it
    /// rides the same nonce but must not satisfy the strand check.
    pub const ALL: [Self; 8] = [
        Self::Attempt,
        Self::Integrate,
        Self::VerifyFailed,
        Self::Park,
        Self::AggregateReview,
        Self::AggregateReviewExecutorFault,
        Self::AggregateVerify,
        Self::MemberExecutorFault,
    ];

    /// The key's stable prefix — the half of the key that is not the nonce.
    const fn prefix(self) -> &'static str {
        match self {
            Self::Attempt => "aether.bloomery.attempt",
            Self::Integrate => "aether.bloomery.integrate",
            Self::VerifyFailed => "aether.bloomery.verify_failed",
            Self::Park => "aether.bloomery.park",
            Self::AggregateReview => "aether.bloomery.aggregate_review",
            Self::AggregateReviewExecutorFault => "aether.bloomery.aggregate_review_executor_fault",
            Self::AggregateVerify => "aether.bloomery.aggregate_verify",
            Self::MemberExecutorFault => "aether.bloomery.member_executor_fault",
            Self::Study => "aether.bloomery.study",
        }
    }

    /// This admission's idempotency key for an order dispatched under `nonce`.
    #[must_use]
    pub fn of(self, nonce: &str) -> IdempotencyKey {
        IdempotencyKey(format!("{}:{nonce}", self.prefix()))
    }

    /// Every key an admission of `nonce` could have been journaled under — the
    /// exhaustive question "did this dispatch reach the journal at all".
    #[must_use]
    pub fn every_key_for(nonce: &str) -> Vec<String> {
        Self::ALL.into_iter().map(|key| key.of(nonce).0).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::AdmissionKey;

    // The plausible bug: Study is added to ALL, so a study row landing
    // before the verdict marks the dispatch complete and the strand check
    // never re-drives a crash that lost the verdict.
    #[test]
    fn a_study_key_does_not_account_for_the_dispatch() {
        let nonce = "n-study";
        assert!(
            !AdmissionKey::every_key_for(nonce).contains(&AdmissionKey::Study.of(nonce).0),
            "a study row must not satisfy the strand check",
        );
    }
}
