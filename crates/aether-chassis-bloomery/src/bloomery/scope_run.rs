//! Opening a pre-bloom scoping run (ADR-0208, #5304) — the non-reducer
//! producer for [`Topic::ScopeDispatch`](aether_bloomery::Topic::ScopeDispatch).
//!
//! ADR-0208 names this as missing machinery, and half of that is right: for
//! [`Topic::Dispatch`](aether_bloomery::Topic::Dispatch) the reducer really is the only producer. But a
//! *host-minted* topic is not new — [`Topic::ViewDocument`](aether_bloomery::Topic::ViewDocument),
//! [`Topic::SourceReplica`](aether_bloomery::Topic::SourceReplica), and [`Topic::Commission`](aether_bloomery::Topic::Commission) are all produced and
//! drained by the host with no [`Decision`](aether_bloomery::Decision) behind
//! them, and the last one is this module's exact template: the commission
//! store writes a raw outbox row inside its own transaction.
//!
//! So the producer half is a copy of a shipped idiom, and everything genuinely
//! new is on the consumer side — an order the executor submits without a bloom
//! (`reactor::executor`'s scope drain) and a verdict the intake routes without
//! one (`intake::admit`'s Scope arm).
//!
//! # The run's state is not in the journal
//!
//! [`Snapshot`](aether_bloomery::Snapshot) keys everything by `BloomId` and
//! every `Fact` carries one, so a scoping run in the journal means either a
//! synthetic bloom contaminating membership, the view, and the metrics ledger,
//! or a second reducer. It lives in the commission store's `scope_runs` ledger
//! instead — the same file, the same versioned migration, the same
//! immutability triggers, and the same transaction that already mints an
//! outbox row.

use aether_bloomery::control::ScopeDispatchPayload;
use aether_bloomery::{Digest, StageCatalog, StageId, Transformation, WorkpieceId};
use aether_data::wire::to_vec;

use crate::store::{ScopeRunOpen, ScopeRunRow, StoreBackend};

/// The domain tag a scoping run's content-addressed subject is minted under.
/// Its own tag rather than a bare hash, so the subject cannot collide with any
/// other digest the estate mints over the same bytes.
const SCOPE_RUN_SUBJECT_DOMAIN: &str = "aether.bloomery.scope_run_subject";

/// The content-addressed subject a scoping run pins: the digest of its own
/// input triple — the commission id, the intent it is scoping, and the base
/// commit it reads code at.
///
/// Every order must pin a digest — the drain refuses a transformation with no
/// `inputs`, and the broker binds returning evidence to the displayed digest —
/// and the run's own *input* is the only content-addressable thing that exists
/// before a revision is frozen. Deliberately not the predecessor revision: a
/// first-ever scope has none (`ScopeRevision.predecessor` is an `Option`).
#[must_use]
pub fn scope_run_subject(commission: &WorkpieceId, intent: Digest, base: Digest) -> Digest {
    let mut bytes = Vec::with_capacity(commission.0.len() + 65);
    bytes.extend_from_slice(commission.0.as_bytes());
    // A separator, so `("ab", "c")` and `("a", "bc")` are different runs.
    bytes.push(0);
    bytes.extend_from_slice(intent.as_bytes().as_slice());
    bytes.extend_from_slice(base.as_bytes().as_slice());
    Digest::of_domain_tagged(SCOPE_RUN_SUBJECT_DOMAIN, &bytes)
}

/// Build the payload a scoping run is enqueued with.
///
/// The transformation comes from
/// [`Transformation::for_scoping_run`](aether_bloomery::Transformation::for_scoping_run)
/// rather than a hand-built literal, because that constructor is what keeps
/// the dispatched wall-clock limit tied to the stage being dispatched. The
/// binding and the seat come from the compiled line: pre-bloom means there is
/// no sealed catalog, so the compiled line is the authority, exactly as
/// `stage_binding` already falls back for a bloom that sealed none.
#[must_use]
pub fn scope_dispatch_payload(
    commission: WorkpieceId,
    ordinal: u64,
    intent: Digest,
    base: Digest,
) -> ScopeDispatchPayload {
    let binding = StageCatalog::binding_of(StageId::Scope);
    let subject = scope_run_subject(&commission, intent, base);
    ScopeDispatchPayload {
        commission,
        ordinal,
        subject,
        intent,
        base,
        stage: StageId::Scope,
        transformation: Transformation::for_scoping_run(&binding, subject, base),
        // Carried unread: whatever the compiled line calibrates is what
        // dispatches. No `ModelOverride` is resolved against it — that type is
        // sealed into a *bloom's* registry, and there is no bloom here.
        profile: StageCatalog::profile_of(StageId::Scope),
    }
}

/// Where a commission's scoping stands, read off its append-only run ledger.
///
/// The termination rule ADR-0208 states, expressed once: a run is done when it
/// froze a revision, or when the attempts reach the `Scope` binding's
/// `retry_budget` with no passing verdict. Neither outcome invents wedge
/// vocabulary — there is no bloom to wedge — and the operator's hand path (the
/// existing `POST /commissions/{id}/revisions`) is unchanged and is the
/// recovery. That is ADR-0208's "the manual path stays, and is visibly
/// manual".
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ScopeRunState {
    /// No run has been opened on this commission.
    Unstarted,
    /// The latest run is open — enqueued or dispatched, with no verdict yet.
    InFlight {
        /// The ordinal in flight.
        ordinal: u64,
    },
    /// The latest run answered and did not freeze; more attempts remain.
    Retryable {
        /// How many attempts have been opened.
        attempts: u64,
    },
    /// A run froze a revision — the terminal success.
    Frozen {
        /// The revision digest bytes the run produced.
        revision: Vec<u8>,
    },
    /// Every attempt the binding allows was spent without a passing verdict.
    /// The commission keeps its existing tip and gains no revision; this is
    /// the durable exhausted marker, derived rather than stored so it cannot
    /// disagree with the rows behind it.
    Exhausted {
        /// The budget that was spent.
        attempts: u64,
    },
}

/// Fold a commission's run rows into [`ScopeRunState`].
///
/// Pure over the rows so the termination rule has exactly one implementation
/// and every reader — the enqueue door, the REST projection, a later operator
/// verb — asks the same question of the same data.
#[must_use]
pub fn scope_run_state(rows: &[ScopeRunRow]) -> ScopeRunState {
    if let Some(frozen) = rows.iter().find(|row| row.kind == "frozen") {
        return ScopeRunState::Frozen { revision: frozen.revision.clone().unwrap_or_default() };
    }
    let Some(highest) = rows.iter().map(|row| row.ordinal).max() else {
        return ScopeRunState::Unstarted;
    };
    if !rows.iter().any(|row| row.ordinal == highest && row.kind == "verdict") {
        return ScopeRunState::InFlight { ordinal: highest };
    }
    if highest >= u64::from(StageCatalog::binding_of(StageId::Scope).retry_budget) {
        ScopeRunState::Exhausted { attempts: highest }
    } else {
        ScopeRunState::Retryable { attempts: highest }
    }
}

/// The outbox sequence, attempt ordinal, and subject a successful
/// [`open_scope_run`] produced — enough for the REST door to name the run
/// without reading the ledger back.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OpenedScopeRun {
    /// The outbox sequence the drain mints its `dispatch_nonce` from.
    pub sequence: u64,
    /// The attempt ordinal opened, from `1`.
    pub ordinal: u64,
    /// The run's content-addressed subject.
    pub subject: Digest,
}

/// Why a scoping run could not be opened.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ScopeRunRefusal {
    /// A run on this commission is already dispatched and unanswered. Opening
    /// a second would put two lanes on one workpiece and let the later one's
    /// revision silently supersede the earlier's.
    AlreadyInFlight {
        /// The ordinal already in flight.
        ordinal: u64,
    },
    /// This commission's scoping already froze a revision.
    AlreadyFrozen,
    /// The `Scope` binding's retry budget is spent. The hand path is the
    /// recovery, deliberately.
    Exhausted {
        /// How many attempts were spent.
        attempts: u64,
    },
    /// The payload did not encode.
    Encode(String),
    /// The store faulted.
    Store(String),
}

/// Open a scoping run on `commission`: pick the next ordinal, refuse if the
/// termination rule says the run is over, build the payload, and write the
/// `enqueued` row and its outbox row in one transaction.
///
/// Returns the outbox sequence, ordinal, and subject the run landed at — the
/// sequence the drain mints its `dispatch_nonce` from, so a caller can name
/// the dispatch before it happens.
///
/// # Errors
/// A refusal from the termination rule, an encode failure, or a store fault.
pub fn open_scope_run(
    store: &mut dyn StoreBackend,
    commission: &WorkpieceId,
    intent: Digest,
    base: Digest,
) -> Result<OpenedScopeRun, ScopeRunRefusal> {
    let rows = store.list_scope_runs(&commission.0).map_err(|error| ScopeRunRefusal::Store(error.to_string()))?;
    match scope_run_state(&rows) {
        ScopeRunState::InFlight { ordinal } => return Err(ScopeRunRefusal::AlreadyInFlight { ordinal }),
        ScopeRunState::Frozen { .. } => return Err(ScopeRunRefusal::AlreadyFrozen),
        ScopeRunState::Exhausted { attempts } => return Err(ScopeRunRefusal::Exhausted { attempts }),
        ScopeRunState::Unstarted | ScopeRunState::Retryable { .. } => {}
    }

    let ordinal =
        store.next_scope_run_ordinal(&commission.0).map_err(|error| ScopeRunRefusal::Store(error.to_string()))?;
    let payload = scope_dispatch_payload(commission.clone(), ordinal, intent, base);
    let encoded = to_vec(&payload).map_err(|error| ScopeRunRefusal::Encode(error.to_string()))?;

    let sequence = store
        .enqueue_scope_run(&ScopeRunOpen {
            commission: &commission.0,
            ordinal,
            intent: intent.as_bytes().as_slice(),
            base: base.as_bytes().as_slice(),
            subject: payload.subject.as_bytes().as_slice(),
            payload: &encoded,
        })
        .map_err(|error| ScopeRunRefusal::Store(error.to_string()))?;
    Ok(OpenedScopeRun { sequence, ordinal, subject: payload.subject })
}

#[cfg(test)]
mod tests {
    use aether_bloomery::testing::digest;
    use aether_bloomery::{StageCatalog, StageId, WorkpieceId};

    use super::{ScopeRunState, scope_run_state, scope_run_subject};
    use crate::store::ScopeRunRow;

    fn row(ordinal: u64, kind: &str) -> ScopeRunRow {
        ScopeRunRow {
            ordinal,
            kind: kind.to_owned(),
            nonce: None,
            subject: None,
            verdict: None,
            revision: (kind == "frozen").then(|| digest(7).as_bytes().to_vec()),
        }
    }

    #[test]
    fn the_run_subject_separates_two_scopes_of_one_commission() {
        // The plausible bug: the subject is taken from the commission id alone,
        // so a re-scope against a moved mainline reuses the digest the first
        // run's evidence already bound — and the broker admits a stale
        // artifact against the new order.
        let commission = WorkpieceId("issue-1".to_owned());
        let first = scope_run_subject(&commission, digest(1), digest(2));

        assert_ne!(first, scope_run_subject(&commission, digest(1), digest(3)), "a moved base is a new subject");
        assert_ne!(first, scope_run_subject(&commission, digest(9), digest(2)), "a rewritten intent is a new subject");
        assert_ne!(
            first,
            scope_run_subject(&WorkpieceId("issue-2".to_owned()), digest(1), digest(2)),
            "another commission is a new subject"
        );
    }

    #[test]
    fn the_termination_rule_reads_the_budget_rather_than_a_literal() {
        // The plausible bug: the exhaustion ceiling is hard-coded, so the
        // identity slice recalibrating the Scope binding's retry budget leaves
        // this rule silently one attempt out of step with the lane it bounds.
        let budget = u64::from(StageCatalog::binding_of(StageId::Scope).retry_budget);
        assert!(budget >= 1, "the binding must allow at least one attempt");

        let mut rows = Vec::new();
        for ordinal in 1..budget {
            rows.push(row(ordinal, "enqueued"));
            rows.push(row(ordinal, "verdict"));
            assert_eq!(scope_run_state(&rows), ScopeRunState::Retryable { attempts: ordinal });
        }
        rows.push(row(budget, "enqueued"));
        rows.push(row(budget, "verdict"));
        assert_eq!(scope_run_state(&rows), ScopeRunState::Exhausted { attempts: budget });
    }

    #[test]
    fn an_unanswered_run_is_in_flight_and_a_frozen_one_is_terminal() {
        // The plausible bug: the fold keys on row count rather than on the
        // presence of a verdict at the highest ordinal, so an enqueued-but
        // unanswered run reads as an attempt already spent and a second lane
        // is opened on the same workpiece.
        assert_eq!(scope_run_state(&[]), ScopeRunState::Unstarted);
        assert_eq!(
            scope_run_state(&[row(1, "enqueued"), row(1, "dispatched")]),
            ScopeRunState::InFlight { ordinal: 1 }
        );
        assert!(matches!(
            scope_run_state(&[row(1, "enqueued"), row(1, "verdict"), row(1, "frozen")]),
            ScopeRunState::Frozen { .. }
        ));
    }
}
