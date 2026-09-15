//! Ejecting a member whose `Verify` did not go green (ADR-0218 §Amendment: low
//! tolerance, 2026-09-15).
//!
//! The move itself is [`super::withdraw`]'s — the same four effects, the same
//! remainder arithmetic, the same one-way exit. What lives here is the part
//! that is about *verification*: which sentence the member leaves under, and
//! the one place that sentence is composed, so the three positions that can
//! eject — a red standalone verdict, a run the host killed at its sealed wall
//! clock, and an attribution that never resolved who owed the failure — read
//! the same way to whoever picks the candidate up.
//!
//! Composing rather than restating is the whole point. A reader of an ejected
//! member has one question — what stopped it, and where do I look — and the
//! answer has to survive being read out of the GitHub mirror with no journal
//! at hand. So the reason names the cause in words, the typed verifier
//! identities that failed, the evidence digest that addresses the full
//! diagnostics (ADR-0178 §Wedge and projection), and the lane's own findings
//! when it wrote any.

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;

use super::withdraw::depart;
use super::{BloomRecord, Decision, Decisions, Snapshot};
use crate::ids::{BloomId, WorkpieceId};
use crate::values::{Evidence, VerifyFailureSet, Withdrawal, WithdrawalCause};

/// Who a reducer-authored ejection records as the decider.
///
/// [`Withdrawal::operator`] asks who decided, and for an ejection the honest
/// answer is the coordinator acting on the disposition a bloom sealed — not a
/// person, and not the absent name a blank would leave behind.
const EJECTING_DECIDER: &str = "bloomery";

/// The sentence one ejected member leaves under.
///
/// `cause` is the clause that completes "this member left because …" and is the
/// only part the three ejection positions differ in. Everything after it is the
/// same four facts in the same order, so two ejections from different positions
/// are comparable on sight.
pub(super) fn ejection_reason(cause: &str, failed: VerifyFailureSet, evidence: &Evidence, findings: &str) -> String {
    let named: Vec<String> = failed.iter().map(|failure| failure.as_str().to_string()).collect();
    let verifiers = if named.is_empty() {
        String::new()
    } else {
        format!(" ({})", named.join(", "))
    };
    let findings = findings.trim();
    let observed = if findings.is_empty() {
        String::new()
    } else {
        format!(" Findings: {findings}")
    };

    format!(
        "{cause}{verifiers}. The bloom's sealed disposition ejects on a verify that does not go green, so no \
         repair lap was dispatched and the candidate is left on its ref for a person to pick up. Evidence: {}.{observed}",
        evidence.detail.to_hex()
    )
}

/// Withdraw `workpiece` from `bloom` under [`WithdrawalCause::Verify`], with
/// `reason` already composed by [`ejection_reason`].
///
/// A thin wrapper over [`depart`] on purpose: what makes an ejection an
/// ejection is the cause and the sentence, and every other consequence — the
/// cancelled lane, the released claim ref and membership, the emptied bloom
/// going terminal, the fold that a completed remainder owes — is a member
/// leaving a bloom and is already decided in one place.
pub(super) fn eject(
    snapshot: &Snapshot,
    record: &BloomRecord,
    bloom: &BloomId,
    workpiece: &WorkpieceId,
    reason: &str,
    effects: Vec<Decision>,
) -> Decisions {
    let withdrawal = Withdrawal {
        workpiece: workpiece.clone(),
        cause: WithdrawalCause::Verify,
        reason: reason.to_string(),
        operator: EJECTING_DECIDER.to_string(),
    };

    depart(snapshot, record, bloom, alloc::vec![withdrawal], effects)
}
