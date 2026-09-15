//! Codec for persisted outbox rows that have adopted the ADR-0059 storage
//! shape: one encode / decode pair, so a store or reactor site never picks
//! between encoders.
//!
//! The writing schema rides beside the bytes (`payload_schema` on the outbox
//! table). Absent, or [`POSITIONAL_ROW_SCHEMA`], is the pre-adoption
//! positional identity. The current identity is the row type's [`aether_data::Kind::NAME`].
//! Anything else is a named refusal — the same sentence
//! [`crate::decode_recorded_decisions`] produces for a journaled decision.

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

use aether_data::storage::{MAX_STORAGE_DEPTH, RecordReader, RecordWriter, fold_path_segment, terminate_field_hash};
use aether_data::wire::{WireDecode, WireEncode, decode_from_slice, from_bytes, to_vec};
use aether_data::{Schema, Storage, StorageData, StorageError, StorageLeaves};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::projection::{
    AwaitingSurfaceView, BloomView, CompositionCursorView, CompositionView, ExecutorFaultView, HostFaultView,
    LandingBlock, LeaseEvictionView, LeaseView, MemberView, NarrowedCompositionView, PendingDecisionView,
    ReviewParkView, WedgeCause, WithdrawnView,
};
use crate::ids::WorkpieceId;
use crate::reduce::RecordedRefusal;
use crate::values::SpendQuiesce;
use crate::values::{CoordinationState, Evidence, OperatorHold, PrecheckState, ResolutionClaim, Wedge};
use crate::{BaseAlertView, BloomId, BloomStatus, Digest, MemberPark, ViewDocument};
use serde::Deserialize;

/// Pre-adoption positional identity. An absent stamp is this identity.
pub const POSITIONAL_ROW_SCHEMA: &str = "positional";

/// Why a persisted outbox row could not be folded into the current shape.
#[derive(Debug)]
pub enum RowSchemaError {
    /// The bytes did not decode as the shape the recorded identity named.
    Decode(String),
    /// The bytes could not be encoded as the shape the recorded identity named.
    Encode(String),
    /// The row names a writing schema this binary has no path for.
    NoUpcast {
        /// The kind the row is filed under.
        kind: &'static str,
        /// The identity stamped beside the bytes.
        found: String,
        /// The identity this binary writes.
        current: &'static str,
    },
}

impl fmt::Display for RowSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "persisted value did not decode: {error}"),
            Self::Encode(error) => write!(f, "persisted value did not encode: {error}"),
            Self::NoUpcast { kind, found, current } => {
                write!(f, "no migration from schema `{found}` to current `{current}` for kind `{kind}`")
            }
        }
    }
}

impl Error for RowSchemaError {}

/// Encode `value` under the writing-schema identity `schema`.
///
/// The current identity ([`aether_data::Kind::NAME`]) writes the storage shape. Absent or
/// [`POSITIONAL_ROW_SCHEMA`] writes the sealed positional shape. Anything else
/// is [`RowSchemaError::NoUpcast`].
///
/// # Errors
///
/// [`RowSchemaError::Encode`] when the bytes cannot be produced, and
/// [`RowSchemaError::NoUpcast`] when this binary has no encoder for `schema`.
pub fn encode_row<T: Storage + Serialize + Clone>(value: &T, schema: Option<&str>) -> Result<Vec<u8>, RowSchemaError> {
    match schema {
        None | Some(POSITIONAL_ROW_SCHEMA) => to_vec(value).map_err(|error| RowSchemaError::Encode(error.to_string())),
        Some(found) if found == T::NAME => T::encode_storage(&StorageData::from_value(value.clone()))
            .map_err(|error| RowSchemaError::Encode(error.to_string())),
        Some(found) => Err(RowSchemaError::NoUpcast { kind: T::NAME, found: found.to_owned(), current: T::NAME }),
    }
}

/// Decode persisted `bytes` under the writing-schema identity `schema`.
///
/// The current identity ([`aether_data::Kind::NAME`]) decodes the storage shape. Absent or
/// [`POSITIONAL_ROW_SCHEMA`] decodes the sealed positional shape. Anything else
/// is [`RowSchemaError::NoUpcast`].
///
/// # Errors
///
/// [`RowSchemaError::Decode`] when the bytes do not decode as the named shape,
/// and [`RowSchemaError::NoUpcast`] when this binary has no path for `schema`.
pub fn decode_row<T: Storage + DeserializeOwned>(bytes: &[u8], schema: Option<&str>) -> Result<T, RowSchemaError> {
    match schema {
        None | Some(POSITIONAL_ROW_SCHEMA) => {
            from_bytes(bytes).map_err(|error| RowSchemaError::Decode(error.to_string()))
        }
        Some(found) if found == T::NAME => {
            T::decode_storage(bytes).map(|data| data.value).map_err(|error| RowSchemaError::Decode(error.to_string()))
        }
        Some(found) => Err(RowSchemaError::NoUpcast { kind: T::NAME, found: found.to_owned(), current: T::NAME }),
    }
}

fn contribute_tuple0<T: StorageLeaves>(
    inner: &T,
    carry: u64,
    depth: u32,
    sink: &mut RecordWriter,
) -> Result<(), StorageError> {
    inner.contribute(fold_path_segment(carry, b"0", depth), depth + 1, sink)
}

fn assemble_tuple0<T: StorageLeaves>(carry: u64, depth: u32, source: &mut RecordReader) -> Result<T, StorageError> {
    T::assemble(fold_path_segment(carry, b"0", depth), depth + 1, source)
}

fn tuple0_absent<T: StorageLeaves>(carry: u64, depth: u32, source: &RecordReader) -> bool {
    T::is_absent(fold_path_segment(carry, b"0", depth), depth + 1, source)
}

impl StorageLeaves for Digest {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        contribute_tuple0(self.as_bytes(), carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        assemble_tuple0::<[u8; 32]>(carry, depth, source).map(Self::from_bytes)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        tuple0_absent::<[u8; 32]>(carry, depth, source)
    }
}

impl StorageLeaves for WorkpieceId {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        contribute_tuple0(&self.0, carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        assemble_tuple0(carry, depth, source).map(Self)
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        tuple0_absent::<String>(carry, depth, source)
    }
}

fn contribute_opaque<T: Schema + WireEncode>(
    value: &T,
    carry: u64,
    depth: u32,
    sink: &mut RecordWriter,
) -> Result<(), StorageError> {
    if depth > MAX_STORAGE_DEPTH {
        return Err(StorageError::NestingTooDeep);
    }
    let mut body = Vec::new();
    value.encode(&mut body).map_err(StorageError::from)?;
    sink.emit(terminate_field_hash(carry, &T::SCHEMA), body)
}

fn assemble_opaque<T>(carry: u64, depth: u32, source: &mut RecordReader) -> Result<T, StorageError>
where
    T: Schema + for<'de> WireDecode<'de>,
{
    if depth > MAX_STORAGE_DEPTH {
        return Err(StorageError::NestingTooDeep);
    }
    let hash = terminate_field_hash(carry, &T::SCHEMA);
    let body = source.take(hash).ok_or(StorageError::MissingRequiredField { hash, name: "" })?;
    decode_from_slice(&body).map_err(StorageError::from)
}

fn opaque_absent<T: Schema>(carry: u64, source: &RecordReader) -> bool {
    !source.contains(terminate_field_hash(carry, &T::SCHEMA))
}

impl StorageLeaves for SpendQuiesce {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        contribute_opaque(self, carry, depth, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        assemble_opaque(carry, depth, source)
    }

    fn is_absent(carry: u64, _depth: u32, source: &RecordReader) -> bool {
        opaque_absent::<Self>(carry, source)
    }
}

impl StorageLeaves for BaseAlertView {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.base.contribute(fold_path_segment(carry, b"base", depth), depth + 1, sink)?;
        self.tree.contribute(fold_path_segment(carry, b"tree", depth), depth + 1, sink)?;
        self.failed.contribute(fold_path_segment(carry, b"failed", depth), depth + 1, sink)?;
        self.evidence.contribute(fold_path_segment(carry, b"evidence", depth), depth + 1, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self {
            base: Digest::assemble(fold_path_segment(carry, b"base", depth), depth + 1, source)?,
            tree: Digest::assemble(fold_path_segment(carry, b"tree", depth), depth + 1, source)?,
            failed: Vec::assemble(fold_path_segment(carry, b"failed", depth), depth + 1, source)?,
            evidence: Digest::assemble(fold_path_segment(carry, b"evidence", depth), depth + 1, source)?,
        })
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        Digest::is_absent(fold_path_segment(carry, b"base", depth), depth + 1, source)
            && Digest::is_absent(fold_path_segment(carry, b"tree", depth), depth + 1, source)
            && Vec::<String>::is_absent(fold_path_segment(carry, b"failed", depth), depth + 1, source)
            && Digest::is_absent(fold_path_segment(carry, b"evidence", depth), depth + 1, source)
    }
}

// The member element both frozen fixtures below carry — everything a
// `MemberView` held before #5969 added `intake_refusal`. Frozen separately for
// the reason the bloom elements are: a member is a `Vec` element inside a
// positional bloom, so a defaulted field appended to the current shape gives
// those rows no additive decoding window and serde cannot upcast an element.
#[derive(aether_data::Schema, Clone, Serialize, Deserialize)]
struct MemberViewPreIntakeRefusal {
    workpiece: WorkpieceId,
    scope_revision: Digest,
    approval: Evidence,
    resolution: Option<ResolutionClaim>,
    pending_decision: Option<PendingDecisionView>,
    wedge: Option<Wedge>,
    blocked_by: Option<WorkpieceId>,
    host_fault: Option<HostFaultView>,
    machinery_rolls: u32,
    machinery_budget: u32,
    wedge_cause: Option<WedgeCause>,
    cursor: Option<CompositionCursorView>,
    park: Option<MemberPark>,
    awaiting_surface: Option<AwaitingSurfaceView>,
    withdrawn: Option<WithdrawnView>,
    leases: Vec<String>,
    evicted_by: Option<LeaseEvictionView>,
}

impl From<MemberViewPreIntakeRefusal> for MemberView {
    fn from(prior: MemberViewPreIntakeRefusal) -> Self {
        Self {
            workpiece: prior.workpiece,
            scope_revision: prior.scope_revision,
            approval: prior.approval,
            resolution: prior.resolution,
            pending_decision: prior.pending_decision,
            wedge: prior.wedge,
            blocked_by: prior.blocked_by,
            host_fault: prior.host_fault,
            machinery_rolls: prior.machinery_rolls,
            machinery_budget: prior.machinery_budget,
            wedge_cause: prior.wedge_cause,
            cursor: prior.cursor,
            park: prior.park,
            awaiting_surface: prior.awaiting_surface,
            withdrawn: prior.withdrawn,
            leases: prior.leases,
            evicted_by: prior.evicted_by,
            intake_refusal: None,
        }
    }
}

// The view row written at d04707893456077046715e189d2739e80c97646c.
// Bloom elements were positional even inside storage rows. Keep that exact
// element shape for queued rows; serde defaults cannot upcast a Vec element.
#[derive(aether_data::Storage, Clone, Serialize, Deserialize)]
#[kind(name = "aether.bloomery.view_document")]
struct ViewDocumentPrePrecheck {
    mainline: Digest,
    observed: Digest,
    spend_quiesce: Option<SpendQuiesce>,
    blooms: Vec<BloomViewPrePrecheck>,
    base_alert: Option<BaseAlertView>,
}

#[derive(aether_data::Schema, Clone, Serialize, Deserialize)]
struct BloomViewPrePrecheck {
    id: BloomId,
    status: BloomStatus,
    superseded_by: Option<BloomId>,
    members: Vec<MemberViewPreIntakeRefusal>,
    landing_blocked: Option<LandingBlock>,
    executor_fault: Option<ExecutorFaultView>,
    review_park: Option<ReviewParkView>,
    composition: Option<CompositionView>,
    operator_hold: Option<OperatorHold>,
    blocker: Option<RecordedRefusal>,
    leases: Vec<LeaseView>,
    narrowed_compositions: Vec<NarrowedCompositionView>,
}

impl From<BloomViewPrePrecheck> for BloomView {
    fn from(prior: BloomViewPrePrecheck) -> Self {
        Self {
            id: prior.id,
            status: prior.status,
            superseded_by: prior.superseded_by,
            members: prior.members.into_iter().map(MemberView::from).collect(),
            landing_blocked: prior.landing_blocked,
            executor_fault: prior.executor_fault,
            review_park: prior.review_park,
            composition: prior.composition,
            operator_hold: prior.operator_hold,
            blocker: prior.blocker,
            leases: prior.leases,
            narrowed_compositions: prior.narrowed_compositions,
            precheck: None,
            coordination: None,
            admin: None,
            waivers: Vec::new(),
            recent_completions: Vec::new(),
            base_verify: None,
        }
    }
}

impl From<ViewDocumentPrePrecheck> for ViewDocument {
    fn from(prior: ViewDocumentPrePrecheck) -> Self {
        Self {
            mainline: prior.mainline,
            observed: prior.observed,
            spend_quiesce: prior.spend_quiesce,
            blooms: prior.blooms.into_iter().map(BloomView::from).collect(),
            base_alert: prior.base_alert,
        }
    }
}

// The exact bloom element written by 7e623b642dc0d5040eec704ce78853132045c891.
// Keep this shape separate: adding a defaulted field inside Vec<BloomView>
// does not give positional rows an additive encoding window.
#[derive(aether_data::Storage, Clone, Serialize, Deserialize)]
#[kind(name = "aether.bloomery.view_document")]
struct ViewDocumentPreCoordination {
    mainline: Digest,
    observed: Digest,
    spend_quiesce: Option<SpendQuiesce>,
    blooms: Vec<BloomViewPreCoordination>,
    base_alert: Option<BaseAlertView>,
}

#[derive(aether_data::Schema, Clone, Serialize, Deserialize)]
struct BloomViewPreCoordination {
    id: BloomId,
    status: BloomStatus,
    superseded_by: Option<BloomId>,
    members: Vec<MemberViewPreIntakeRefusal>,
    landing_blocked: Option<LandingBlock>,
    executor_fault: Option<ExecutorFaultView>,
    review_park: Option<ReviewParkView>,
    composition: Option<CompositionView>,
    operator_hold: Option<OperatorHold>,
    blocker: Option<RecordedRefusal>,
    leases: Vec<LeaseView>,
    narrowed_compositions: Vec<NarrowedCompositionView>,
    precheck: Option<PrecheckState>,
}

impl From<BloomViewPreCoordination> for BloomView {
    fn from(prior: BloomViewPreCoordination) -> Self {
        Self {
            id: prior.id,
            status: prior.status,
            superseded_by: prior.superseded_by,
            members: prior.members.into_iter().map(MemberView::from).collect(),
            landing_blocked: prior.landing_blocked,
            executor_fault: prior.executor_fault,
            review_park: prior.review_park,
            composition: prior.composition,
            operator_hold: prior.operator_hold,
            blocker: prior.blocker,
            leases: prior.leases,
            narrowed_compositions: prior.narrowed_compositions,
            precheck: prior.precheck,
            coordination: None,
            admin: None,
            waivers: Vec::new(),
            recent_completions: Vec::new(),
            base_verify: None,
        }
    }
}

impl From<ViewDocumentPreCoordination> for ViewDocument {
    fn from(prior: ViewDocumentPreCoordination) -> Self {
        Self {
            mainline: prior.mainline,
            observed: prior.observed,
            spend_quiesce: prior.spend_quiesce,
            blooms: prior.blooms.into_iter().map(BloomView::from).collect(),
            base_alert: prior.base_alert,
        }
    }
}

impl ViewDocument {
    /// Decode current, pre-progress, pre-coordination, or pre-precheck view
    /// outbox rows, preserving their blooms in both storage and positional
    /// forms.
    ///
    /// # Errors
    /// Returns the current decoder's refusal if neither supported shape decodes.
    pub fn decode_row(bytes: &[u8], schema: Option<&str>) -> Result<Self, RowSchemaError> {
        decode_row(bytes, schema).or_else(|current_error| {
            decode_row::<ViewDocumentPreProgress>(bytes, schema)
                .map(Self::from)
                .or_else(|_| {
                    decode_row::<ViewDocumentPreCoordination>(bytes, schema)
                        .map(Self::from)
                        .or_else(|_| decode_row::<ViewDocumentPrePrecheck>(bytes, schema).map(Self::from))
                })
                .map_err(|_| current_error)
        })
    }
}

// The exact bloom element written between the coordination projection and this
// batch: coordination present, and every field appended after it absent — the
// progress channel's `recent_completions` / `base_verify` and ADR-0219's
// `admin` / `waivers` alike, since both appends first ship together and no
// binary ever wrote a row carrying one pair without the other. Keep this shape
// separate for the reason `ViewDocumentPreCoordination` states: a defaulted
// field inside `Vec<BloomView>` does not give positional rows an additive
// encoding window.
#[derive(aether_data::Storage, Clone, Serialize, Deserialize)]
#[kind(name = "aether.bloomery.view_document")]
struct ViewDocumentPreProgress {
    mainline: Digest,
    observed: Digest,
    spend_quiesce: Option<SpendQuiesce>,
    blooms: Vec<BloomViewPreProgress>,
    base_alert: Option<BaseAlertView>,
}

#[derive(aether_data::Schema, Clone, Serialize, Deserialize)]
struct BloomViewPreProgress {
    id: BloomId,
    status: BloomStatus,
    superseded_by: Option<BloomId>,
    members: Vec<MemberView>,
    landing_blocked: Option<LandingBlock>,
    executor_fault: Option<ExecutorFaultView>,
    review_park: Option<ReviewParkView>,
    composition: Option<CompositionView>,
    operator_hold: Option<OperatorHold>,
    blocker: Option<RecordedRefusal>,
    leases: Vec<LeaseView>,
    narrowed_compositions: Vec<NarrowedCompositionView>,
    precheck: Option<PrecheckState>,
    coordination: Option<CoordinationState>,
}

impl From<BloomViewPreProgress> for BloomView {
    fn from(prior: BloomViewPreProgress) -> Self {
        Self {
            id: prior.id,
            status: prior.status,
            superseded_by: prior.superseded_by,
            members: prior.members,
            landing_blocked: prior.landing_blocked,
            executor_fault: prior.executor_fault,
            review_park: prior.review_park,
            composition: prior.composition,
            operator_hold: prior.operator_hold,
            blocker: prior.blocker,
            leases: prior.leases,
            narrowed_compositions: prior.narrowed_compositions,
            precheck: prior.precheck,
            coordination: prior.coordination,
            admin: None,
            waivers: Vec::new(),
            recent_completions: Vec::new(),
            base_verify: None,
        }
    }
}

impl From<ViewDocumentPreProgress> for ViewDocument {
    fn from(prior: ViewDocumentPreProgress) -> Self {
        Self {
            mainline: prior.mainline,
            observed: prior.observed,
            spend_quiesce: prior.spend_quiesce,
            blooms: prior.blooms.into_iter().map(BloomView::from).collect(),
            base_alert: prior.base_alert,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{POSITIONAL_ROW_SCHEMA, decode_row, encode_row};
    use crate::{
        BaseAlertView, BaseVerifyVerdict, BaseVerifyView, BloomId, BloomStatus, BloomView, CompletionRecord,
        CompletionVerdict, Digest, SharedRunMode, SpendQuiesce, StageId, ViewDocument, WorkpieceId,
    };
    use aether_data::Kind;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, PartialEq, Eq, Debug, aether_data::Storage, Serialize, Deserialize)]
    #[kind(name = "aether.bloomery.view_document")]
    struct ViewDocumentShadow {
        mainline: Digest,
        observed: Digest,
        spend_quiesce: Option<SpendQuiesce>,
        blooms: Vec<BloomView>,
        base_alert: Option<BaseAlertView>,
        extra: Option<u32>,
    }

    fn sample() -> ViewDocument {
        ViewDocument {
            mainline: Digest::from_bytes([1; 32]),
            observed: Digest::from_bytes([2; 32]),
            spend_quiesce: Some(SpendQuiesce::Window {
                window: String::from("day"),
                spent_micro_usd: 1,
                ceiling_micro_usd: 2,
            }),
            blooms: Vec::new(),
            base_alert: Some(BaseAlertView {
                base: Digest::from_bytes([3; 32]),
                tree: Digest::from_bytes([4; 32]),
                failed: vec![String::from("lint")],
                evidence: Digest::from_bytes([5; 32]),
            }),
        }
    }

    #[test]
    fn storage_row_tolerates_a_trailing_optional_in_either_direction() {
        // Tripwire: the row root adopted the storage shape. Encoding through
        // the positional path would make a longer reader fail on a shorter
        // payload and a shorter reader fail on a longer one.
        let produced = encode_row(&sample(), Some(ViewDocument::NAME)).unwrap();
        let shadow: ViewDocumentShadow = decode_row(&produced, Some(ViewDocumentShadow::NAME)).unwrap();
        assert!(shadow.extra.is_none(), "a reader with the extra field sees it absent");

        let newer = ViewDocumentShadow {
            mainline: shadow.mainline,
            observed: shadow.observed,
            spend_quiesce: shadow.spend_quiesce,
            blooms: shadow.blooms,
            base_alert: shadow.base_alert,
            extra: Some(7),
        };
        let newer_bytes = encode_row(&newer, Some(ViewDocumentShadow::NAME)).unwrap();
        let older: ViewDocument = decode_row(&newer_bytes, Some(ViewDocument::NAME)).unwrap();
        assert_eq!(older, sample(), "a reader without the extra field still decodes");
    }

    #[test]
    fn an_unknown_identity_refuses_by_name() {
        let error = decode_row::<ViewDocument>(b"x", Some("aether.bloomery.no-such-shape")).unwrap_err().to_string();
        assert!(error.contains("no migration from schema `aether.bloomery.no-such-shape`"), "{error}");
        assert!(error.contains(&format!("to current `{}`", ViewDocument::NAME)), "{error}");
        assert!(error.contains(&format!("for kind `{}`", ViewDocument::NAME)), "{error}");
    }

    #[test]
    fn positional_identity_still_decodes_the_serde_path() {
        let value = sample();
        let bytes = encode_row(&value, Some(POSITIONAL_ROW_SCHEMA)).unwrap();
        let decoded = decode_row::<ViewDocument>(&bytes, None).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn positional_bloom_elements_carry_the_progress_fields() {
        // Tripwire: the new bloom-element tail (`recent_completions`,
        // `base_verify`) rides the positional wire inside `Vec<BloomView>`.
        // A field the `Schema` derive cannot encode, or one written in the
        // wrong slot, fails this round trip rather than corrupting a queued
        // row the upcast then refuses.
        let mut value = sample();
        value.blooms.push(BloomView {
            id: BloomId(Digest::from_bytes([9; 32])),
            status: BloomStatus::Sealed,
            members: Vec::new(),
            recent_completions: vec![CompletionRecord {
                nonce: Digest::from_bytes([10; 32]),
                member: WorkpieceId(String::from("issue-1")),
                step: 1,
                steps: 2,
                stage: StageId::Verify,
                mode: SharedRunMode::Contextual,
                gates: vec![String::from("check")],
                verdict: CompletionVerdict::Passed,
                duration_millis: Some(84_000),
                tree: Some(Digest::from_bytes([11; 32])),
            }],
            base_verify: Some(BaseVerifyView {
                base: Digest::from_bytes([12; 32]),
                tree: None,
                verdict: BaseVerifyVerdict::Running,
                failed: Vec::new(),
            }),
            ..BloomView::default()
        });
        for schema in [None, Some(POSITIONAL_ROW_SCHEMA), Some(ViewDocument::NAME)] {
            let bytes = encode_row(&value, schema).unwrap();
            assert_eq!(decode_row::<ViewDocument>(&bytes, schema).unwrap(), value);
        }
    }
}
