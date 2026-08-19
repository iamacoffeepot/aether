//! Bounded artifact ranges and server-side kind resolution.
//!
//! The console never owns the wire vocabulary: this module tries each known
//! artifact type and keeps a hit only when the value round-trips to the
//! stored bytes.

use aether_bloomery::{
    Adjudication, Artifact, LandingReceipt, OperatorHold, OperatorRepair, OrphanClaimRelease, Question, Statement,
    StudyRecord, TimeoutRecord,
};
use aether_data::wire::{from_bytes, to_vec};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::query::ArtifactQuery;

/// A byte range that fits inside an artifact, or a request past its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactRange {
    /// `bytes` is the slice `[offset, offset+len)`, `total` is the artifact length.
    Ok { bytes: Vec<u8>, offset: u64, total: u64, truncated: bool, notice: Option<String> },
    /// `offset` is at or past `total`.
    Unsatisfiable { total: u64 },
}

/// Slice `bytes` under `query`. `offset >= len` is unsatisfiable, including
/// a non-zero offset into an empty artifact.
pub fn range_bytes(bytes: &[u8], query: &ArtifactQuery) -> ArtifactRange {
    let total = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if query.offset >= total && !(query.offset == 0 && total == 0) {
        return ArtifactRange::Unsatisfiable { total };
    }
    let start = usize::try_from(query.offset).unwrap_or(usize::MAX).min(bytes.len());
    let want = usize::try_from(query.limit).unwrap_or(usize::MAX);
    let end = start.saturating_add(want).min(bytes.len());
    let slice = bytes[start..end].to_vec();
    let truncated = end < bytes.len();
    ArtifactRange::Ok { bytes: slice, offset: query.offset, total, truncated, notice: query.notice.clone() }
}

/// Server-side type resolution. `kind` is the content-address domain of the
/// first type whose decode round-trips; `None` means no known type matched.
pub fn resolve_kind(bytes: &[u8]) -> Option<(&'static str, serde_json::Value)> {
    try_known::<StudyRecord>(bytes, "aether.bloomery.study_record")
        .or_else(|| try_known::<TimeoutRecord>(bytes, "aether.bloomery.timeout_record"))
        .or_else(|| try_known::<Question>(bytes, "aether.bloomery.question"))
        .or_else(|| try_known::<Statement>(bytes, "aether.bloomery.statement"))
        .or_else(|| try_known::<LandingReceipt>(bytes, "aether.bloomery.landing_receipt"))
        .or_else(|| try_known::<Adjudication>(bytes, "aether.bloomery.adjudication"))
        .or_else(|| try_known::<OperatorRepair>(bytes, "aether.bloomery.operator_repair"))
        .or_else(|| try_known::<OperatorHold>(bytes, "aether.bloomery.operator_hold"))
        .or_else(|| try_known::<OrphanClaimRelease>(bytes, "aether.bloomery.orphan_claim_release"))
        .or_else(|| try_known::<Artifact>(bytes, "aether.bloomery.artifact"))
}

fn try_known<T>(bytes: &[u8], kind: &'static str) -> Option<(&'static str, serde_json::Value)>
where
    T: DeserializeOwned + Serialize,
{
    let decoded: T = from_bytes(bytes).ok()?;
    // The untagged wire can coincidentally decode some other artifact;
    // only a round-trip back to the stored bytes is that type.
    if to_vec(&decoded).ok()?.as_slice() != bytes {
        return None;
    }
    let value = serde_json::to_value(&decoded).ok()?;
    Some((kind, value))
}
