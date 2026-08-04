//! The `aether.signing.*` transact-mail kind family (ADR-0149 step 3).
//!
//! The typed request the live answer gate (`api/runtime.rs`) sends to the
//! `aether.signing` mailbox to verify an author-signed statement against the
//! host-custodied allowlist, plus its reply. The
//! [`aether_bloomery::Statement`] value type is carried as its canonical
//! [`aether_data::wire`] bytes rather than typed fields — exactly as the
//! `aether.source.*` family carries its port values — because `Statement` is
//! serde-encoded but not `Schema`, and this capability has no reason to key or
//! filter on any of its fields.

use serde::{Deserialize, Serialize};

/// Verify `statement`'s author signature against the host-custodied
/// authorized-signer allowlist (ADR-0151 key policy).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.signing.verify")]
pub struct Verify {
    /// The `aether_data::wire`-encoded [`aether_bloomery::Statement`] to verify.
    #[serde(with = "aether_data::bytes")]
    pub statement: Vec<u8>,
}

/// Reply to [`Verify`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.signing.verify_result")]
pub enum VerifyResult {
    /// The statement decoded; `verified` is whether its author signature checks
    /// against an allowlisted signer over its exact words. A non-author
    /// provenance and an unknown / mismatched / malformed signature all resolve
    /// `verified: false` — the gate is fail-closed.
    Ok {
        /// Whether the author signature verified.
        verified: bool,
    },
    /// The request could not be evaluated — the `statement` bytes did not decode
    /// to a [`aether_bloomery::Statement`]. Distinct from `Ok { verified: false }`
    /// (a well-formed statement that simply did not verify).
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}
