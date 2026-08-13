//! The scripted-lane seam (#4711): hand a booted coordinator the verdict a lane
//! would have uploaded, for an order it really dispatched.
//!
//! The in-process fixture scenarios drive a whole bloom reactor-to-reactor with
//! no model and no lane subprocess. The decision path stays production: the
//! reducer decides, [`project`](crate::control) writes the outbox row, the
//! boot-constructed executor reactor drains it and records a real outstanding
//! order. What the scenarios substitute is the part a model would have produced
//! — the verdict, and the candidate a construct run captured.
//!
//! One production step is substituted alongside it, and it is not a detail. The
//! real pull path ends by pushing an admitted capture to its bloom candidate ref
//! (ADR-0152); the scripted path omits that push, because the only pusher the
//! reactor is built with shells a real `git push --force origin`. The fixture
//! harness plants the candidate ref itself instead, so nothing in this tier can
//! catch a wrong ref name, a dropped push, or a mis-resolved correspondence.
//!
//! The substitution rides the same trust boundary a real upload does. A
//! scripted verdict names a nonce and a subject; [`admit_uploaded`] looks the
//! nonce up in the outstanding-order registry and refuses one that names no
//! live order or binds a digest the order did not display. So a scenario cannot
//! invent an attempt the coordinator never ordered, and cannot bind a verdict to
//! a tree the order never showed — which is exactly what makes the fixture a
//! test of the handoff rather than a way around it.
//!
//! Compiled only under `cfg(all(feature = "github", any(test, feature =
//! "testing")))` — the `github` half because the order registry and the fixture
//! it scripts against are both that feature's, the `testing` half beside the
//! [`FakeGithub`](aether_bloomery_github::testing::FakeGithub) fixture it gates,
//! so a production binary carries neither the kinds nor the handler that admits
//! them.
//!
//! [`admit_uploaded`]: crate::bloomery::admit_uploaded

use aether_bloomery::{CandidateRef, Digest, Nonce, StageVerdict, StudyCall, StudyCost, VerifyFailureSet};
use serde::{Deserialize, Serialize};

use crate::bloomery::UploadedEvidence;

/// The wire-carryable spelling of [`StageVerdict`].
///
/// Its own enum because the port's verdict is a pure value type with no serde —
/// deliberately, since it never crosses a wire on the production path (a real
/// verdict is decoded from an artifact name). A scripted one does cross, so the
/// scripted vocabulary carries the encoding and [`into_stage_verdict`] is the
/// single place the two spellings meet.
///
/// [`into_stage_verdict`]: Self::into_stage_verdict
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ScriptedVerdict {
    /// A model lane approved its own output — the Construct / Refine pass.
    Approved,
    /// A mechanical gate passed — the Verify / `AggregateVerify` pass.
    VerificationPassed,
    /// A mechanical gate failed; the upload must name the failed members.
    VerificationFailed,
    /// A critic found something — the failing `AggregateReview` verdict.
    ReviewFinding,
    /// The lane raised a question instead of a verdict (ADR-0151).
    Parked,
}

impl ScriptedVerdict {
    /// The port verdict this scripted one stands for.
    #[must_use]
    pub const fn into_stage_verdict(self) -> StageVerdict {
        match self {
            Self::Approved => StageVerdict::Approved,
            Self::VerificationPassed => StageVerdict::VerificationPassed,
            Self::VerificationFailed => StageVerdict::VerificationFailed,
            Self::ReviewFinding => StageVerdict::ReviewFinding,
            Self::Parked => StageVerdict::Parked,
        }
    }
}

/// What a scenario uploads on a lane's behalf — the fields
/// [`crate::bloomery::UploadedEvidence`] carries, in their wire-carryable
/// spelling.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ScriptedUpload {
    /// The nonce the upload answers. Must name a live outstanding order.
    pub nonce: Nonce,
    /// The digest the verdict is about. Must be the digest that order displayed.
    pub subject: Digest,
    /// The verdict itself.
    pub verdict: ScriptedVerdict,
    /// The supporting artifact digest (the check output, the review record).
    pub detail: Digest,
    /// The candidate a construct-lane run captured (ADR-0152), if any.
    pub candidate: Option<CandidateRef>,
    /// The critic's findings prose, if any.
    pub findings: Option<String>,
    /// The exact failed `verify.check` members (ADR-0178). Nonempty only for a
    /// failed member Verify.
    pub failed_verifiers: VerifyFailureSet,
    /// What the attempt cost (#4679), if the scenario measures one.
    pub cost: Option<StudyCost>,
    /// Per-call usage when a scenario measures a banded dispatch.
    #[serde(default)]
    pub calls: Option<Vec<StudyCall>>,
}

impl ScriptedUpload {
    /// The port upload this scripted one stands for — what the broker admits.
    #[must_use]
    pub fn into_upload(self) -> UploadedEvidence {
        UploadedEvidence {
            nonce: self.nonce,
            subject: self.subject,
            verdict: self.verdict.into_stage_verdict(),
            detail: self.detail,
            candidate: self.candidate,
            findings: self.findings,
            failed_verifiers: self.failed_verifiers,
            cost: self.cost,
            calls: self.calls,
        }
    }
}

/// `aether.bloomery.testing.scripted_evidence` — admit one scripted lane verdict
/// through the executor reactor's real intake boundary. Reply:
/// [`ScriptedEvidenceResult`].
///
/// The payload is the wire bytes of a [`ScriptedUpload`], following the
/// opaque-bytes convention the `aether.bloomery.admit` ingress uses: the value
/// vocabulary stays out of the wire schema, and only this crate decodes it.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.bloomery.testing.scripted_evidence")]
pub struct ScriptedEvidence {
    /// The scripted upload's canonical `aether_data::wire` bytes.
    #[serde(with = "aether_data::bytes")]
    pub upload: Vec<u8>,
}

/// Reply to [`ScriptedEvidence`].
///
/// Three arms rather than a boolean because a scenario that fails needs to know
/// *which* boundary refused it: `Refused` is the broker declining a nonce or a
/// binding (a scenario that read the wrong order), while `Err` is the harness
/// itself faulting (a decode, a store read). Collapsing them would make a
/// scripting mistake and a coordinator defect look identical.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.bloomery.testing.scripted_evidence_result")]
pub enum ScriptedEvidenceResult {
    /// The broker admitted the verdict. `idempotency_key` is the key the
    /// admitted event carries, which names the route the broker chose
    /// (`aether.bloomery.integrate:…` / `…verify_failed:…` / `…attempt:…` /
    /// `…aggregate_verify:…` / `…aggregate_review:…` / `…park:…`), so a
    /// scenario asserts the routing without decoding the event.
    Admitted {
        /// The admitted event's idempotency key.
        idempotency_key: String,
    },
    /// The broker refused the upload without touching the reducer; `refusal`
    /// renders the [`IntakeRefusal`](crate::bloomery::IntakeRefusal).
    Refused {
        /// The rendered refusal.
        refusal: String,
    },
    /// The scripted lane itself faulted before or during admission.
    Err {
        /// The rendered fault.
        error: String,
    },
}
