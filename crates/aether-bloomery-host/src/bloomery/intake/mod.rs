//! Evidence intake: the return path of ADR-0149 migration step 2 (issue #3502).
//!
//! Migration step 2 dispatches fully-resolved work orders to a zero-secret
//! worker lane; the workers upload evidence bytes. This module is the side that
//! *accepts* that evidence — under two hard constraints:
//!
//! - **Trust.** A worker runs untrusted and can lie, so the broker admits an
//!   upload only when its idempotency nonce names a real outstanding order
//!   **and** its bound digest equals the digest that order displayed. A
//!   fabricated or replayed upload is refused, and even an accepted claim is an
//!   *untrusted* claim the reducer re-checks (`reduce_integrate` re-verifies
//!   `claim.evidence.validates(&claim.candidate)`), never a state advance by
//!   itself — the trust boundary is defended twice.
//! - **Reachability.** The host sits behind NAT, so intake is pull-based: it
//!   polls run status and streams uploaded evidence through the existing
//!   [`ExecutorShell`](crate::bloomery::executor::ExecutorShell) surface (`inspect` / `stream_evidence`), never a webhook.
//!
//! # The registry, and the dependency direction
//!
//! The nonce → reducer-context linkage is a **host-side dispatch-record**, not a
//! field on the portable core [`WorkOrder`](aether_bloomery::WorkOrder): ADR-0149 §The line defines a work
//! order as a portable unit blind to which bloom dispatched it, so the bloom /
//! workpiece / scope-revision / candidate context rides the persisted
//! [`OutstandingOrder`](crate::store::OutstandingOrder) registry the host writes
//! at dispatch ([`record_dispatch`]) and reads/consumes at intake
//! ([`admit_uploaded`]). The core order stays `{ transformation, nonce }`.
//!
//! This slice owns the registry plus its write API and the read side; the
//! production dispatch trigger that calls the write when the reducer emits a
//! work-order dispatch decision is #3505's wiring. So the direction is
//! **#3505 → #3502**: #3505 will call [`record_dispatch`] / [`dispatch_and_record`]
//! when it dispatches. This slice drives the write API directly in its own
//! tests; the production loop closes when #3505 wires dispatch to it.
//!
//! [`run_intake_cycle`] is the substantive pull primitive — inspect → stream →
//! broker → admit — that a production loop drives. The *periodic scheduler* that
//! calls it on an interval (and the ADR-0090 poll-interval `Config` knob that
//! paces it) land with that production loop rather than here: until #3505 wires
//! dispatch, there are no tracked runs to poll, so a live timer would poll an
//! empty handle set and an interval knob would have no reader.

mod admit;
mod claims;
mod cycle;
mod dispatch;

pub use admit::{Admission, AdmitDecision, IntakeError, IntakeRefusal, UploadedEvidence, admit_uploaded};
pub use claims::{EvidenceClaims, NameEvidenceClaims, attempt_artifact_name};
pub use cycle::{AdmitSink, CycleError, CycleReport, run_intake_cycle};
pub use dispatch::{DispatchError, DispatchRecord, dispatch_and_record, record_dispatch};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
