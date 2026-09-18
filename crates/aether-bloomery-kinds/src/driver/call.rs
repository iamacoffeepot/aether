//! Native call mail: ask the driver to run one program from outside the journal.

use crate::{Detail, Digest, Fault, Head, NativeOrigin, OpaqueBytes, ProgramName, Transition};

/// Ask the driver to run program `name` from the bundle `program` resolves to, over `input`.
///
/// Sent by a native caller, not a reactor rule. `origin` and `key` together
/// are the idempotency key: a replayed `(origin, key)` answers with exactly
/// what the first request recorded (ADR-0226 decision 11).
#[aether_data::kind(name = "aether.bloomery.driver.call", eq, no_serde)]
pub struct Call {
    pub program: Head<OpaqueBytes>,
    pub name: ProgramName,
    pub input: Digest,
    pub origin: NativeOrigin,
    /// The caller-chosen idempotency key within `origin`.
    pub key: u64,
}

/// Reply to one [`Call`]. Exactly one outcome per call (ADR-0226 decision 11).
#[aether_data::kind(name = "aether.bloomery.driver.call_outcome", eq, no_serde)]
pub enum CallOutcome {
    /// The attempt produced an execution.
    Transition {
        /// Echoes the request's idempotency key.
        key: u64,
        /// The outcome entry's own sequence.
        seq: u64,
        /// The recorded value: the reply is the record.
        transition: Transition,
    },
    /// The attempt ended without an execution.
    Fault {
        /// Echoes the request's idempotency key.
        key: u64,
        /// The outcome entry's own sequence.
        seq: u64,
        /// The recorded value: the reply is the record.
        fault: Fault,
    },
    /// Nothing was recorded for this call; `reason` says why.
    Refused {
        /// Echoes the request's idempotency key.
        key: u64,
        reason: CallRefusal,
    },
}

/// Why a [`Call`] was refused before anything was recorded.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub enum CallRefusal {
    /// The head named by `Call::program` is unbound at the journal head.
    HeadUnbound,
    /// A repeated `(origin, key)` named a different request. A caller bug.
    KeyReused,
    /// The journal backend refused the `Requested` append.
    Journal { reason: Detail },
}
