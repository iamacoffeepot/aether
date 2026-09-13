//! The persisted-kind registry and the one read entry point (ADR-0187).
//!
//! Bytes never persist without the schema that wrote them. Each store column
//! that is sealed history is in [`PERSISTED_KINDS`]; each column that is
//! re-derivable from that history is out. The table is explicit because
//! [`ConfigKind`](crate::values::ConfigKind) is blanket-implemented for every
//! serializable kind, so nothing derivable distinguishes a kind that *is*
//! persisted from one that merely could be.
//!
//! # Store columns
//!
//! - **journal event** — in. Boot replay and the metrics refold decode it, and
//!   a shape change is a fatal abort until an upcast ships.
//! - **journal decisions** — in. The same fold reads it; the v1 prior shape
//!   is the first registered upcast.
//! - **config** — in. Sealed configuration is history: the kind *name* survives
//!   schema evolution so entries are not orphaned, which makes the schema
//!   digest the remaining place drift can be detected. The generic authoring
//!   route resolves a runtime-named kind's schema through the descriptor
//!   inventory; the typed read still goes through this table when the kind is
//!   listed.
//! - **metrics rollups, outbox, outstanding orders, parked question** — out.
//!   The store refolds metric caches from the journal, and in-flight rows are
//!   not sealed history.
//!
//! # Upcast pins are recorded history, not recomputed shapes
//!
//! Each [`PersistedUpcast`] pins the digest its rows actually carry, copied
//! from the store's stamps into a literal. The pin used to be computed from a
//! "frozen" shape module, and #5500 is why it no longer is: the frozen shape
//! embedded live leaf types, so a change to those leaves silently moved the
//! pin along with the code it existed to check. A literal cannot drift; the
//! ledger test (`tests/golden_decisions/schema_digests.rs`) holds each literal
//! against the append-only digest history, and the frozen type copies that
//! remain (`decisions_v1`, `process_instructions_pre_reader`) exist only to
//! *decode* rows whose wire layout differs structurally from today's.
//!
//! # Two read entry points, one upcast list
//!
//! A journal column has a hand-written read entry point per kind
//! ([`decode_recorded_decisions`], [`decode_recorded_event`]), so it passes its
//! typed decoders to [`decode_persisted`] at the call site. Sealed
//! configuration has a single generic read serving every config kind, with no
//! call site that knows which kind it is decoding, so a config kind's upcast
//! carries a [`PersistedUpcast::reshape`] rewriter into current-shape bytes and
//! resolves through [`decode_reshaped`]. Both walk the same
//! [`PersistedKind::upcasts`] list under the same digest rules.

mod rendering;

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;
use std::sync::OnceLock;

use aether_data::Kind;
use aether_data::Schema;
use aether_data::schema::SchemaType;
use aether_data::wire::{Error as WireError, from_bytes, to_vec};
use serde::de::DeserializeOwned;

use crate::digest::{Digest, encode_hex, schema_digest};
use crate::reduce::decisions_v1::DecisionsV1;
use crate::reduce::{Decisions, Event};
use crate::values::process_instructions_pre_reader::ModelProcessInstructionsPreReader;
use crate::values::{
    ApprovalPolicy, CoordinationPolicy, ModelOverride, ModelProcessInstructions, PipelineManifest, PrecheckPolicy,
    PriceTable, SpendCeiling, StageCatalog,
};

pub use rendering::{RenderError, render_schema};

/// Decoder from a prior persisted shape into the current value.
type UpcastFn<T> = fn(&[u8]) -> Result<T, WireError>;

/// Rewriter from a prior persisted shape into the current shape's canonical
/// wire bytes — the decoder a generic read entry point can carry in the
/// registry instead of taking at the call site.
pub type ReshapeFn = fn(&[u8]) -> Result<Vec<u8>, WireError>;

/// Kind name persisted for journaled [`Decisions`].
pub const DECISIONS_KIND: &str = "decisions";
/// Kind name persisted for journaled [`Event`].
pub const EVENT_KIND: &str = "event";

/// How an absent recorded digest is read — the pre-column shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bootstrap {
    /// Pre-column rows decode as the current shape. Event and config rows are
    /// stamped with the identity current at the column's migration, so an
    /// absent digest after that is the same shape this binary writes.
    Current,
    /// Pre-column rows decode through [`PersistedKind::upcasts`] at this index.
    /// Journaled decisions treat an absent stamp as v1.
    Upcast(usize),
}

/// One persisted kind's current schema and the prior shapes it can upcast.
pub struct PersistedKind {
    /// The kind's stable name — the `K` in ADR-0187 §5.
    pub name: &'static str,
    /// The current shape this binary writes.
    pub schema: &'static SchemaType,
    /// The pre-column identity an absent recorded digest names.
    pub bootstrap: Bootstrap,
    /// Prior shapes this entry can carry forward, oldest first, in lockstep
    /// with the `upcasts` decoder array a per-kind read entry point passes —
    /// or, for a generic one, with each entry's own [`PersistedUpcast::reshape`].
    pub upcasts: &'static [PersistedUpcast],
    current: OnceLock<Digest>,
}

/// One prior shape a [`PersistedKind`] can carry forward.
pub struct PersistedUpcast {
    /// The digest the prior shape's rows are stamped with — copied from
    /// recorded history (the store's stamps, mirrored by the append-only
    /// ledger in `tests/golden_decisions/fixtures/schema-digests.txt`) into a
    /// literal that no change to live code can move (#5500).
    pub digest: Digest,
    /// How [`decode_reshaped`] carries a row of this shape forward. `None` for
    /// a kind whose read entry point passes its own decoders to
    /// [`decode_persisted`] — the journal columns.
    pub reshape: Option<ReshapeFn>,
}

impl PersistedKind {
    /// Digest of this kind's current schema.
    ///
    /// Computed once per entry: hashing the schema on every journal row would
    /// turn a 32-byte comparison into a full schema walk.
    ///
    /// # Panics
    ///
    /// Panics if the compiled schema exceeds the rendering budget, which no
    /// persisted kind does.
    #[must_use]
    pub fn current_digest(&self) -> Digest {
        *self.current.get_or_init(|| digest_of_schema(self.name, self.schema))
    }
}

fn digest_of_schema(kind: &'static str, schema: &SchemaType) -> Digest {
    schema_digest(kind, schema).expect("compiled persisted kinds never exceed the schema-rendering budget")
}

/// Why persisted bytes could not be folded into the current shape (ADR-0187).
#[derive(Debug)]
pub enum PersistedSchemaError {
    /// The bytes did not decode as the shape the recorded digest named.
    Decode(WireError),
    /// The row names a writing schema this binary has no upcast for.
    NoUpcast {
        /// The kind the row is filed under.
        kind: &'static str,
        /// The identity stamped beside the bytes.
        found: String,
        /// The identity this binary writes.
        current: Digest,
    },
}

impl fmt::Display for PersistedSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "persisted value did not decode: {error}"),
            Self::NoUpcast { kind, found, current } => {
                write!(f, "no migration from schema `{found}` to current `{current}` for kind `{kind}`")
            }
        }
    }
}

impl Error for PersistedSchemaError {}

/// Decode persisted `bytes` under the writing-schema identity `recorded`.
///
/// An equal digest takes the current-shape decode. An absent digest means the
/// pre-column shape [`PersistedKind::bootstrap`] names. A differing digest
/// walks `upcasts` in lockstep with [`PersistedKind::upcasts`]. Anything else
/// is [`PersistedSchemaError::NoUpcast`].
///
/// # Errors
///
/// [`PersistedSchemaError::Decode`] when the bytes do not decode as the shape
/// the recorded digest named, and [`PersistedSchemaError::NoUpcast`] when this
/// binary has no upcast for that digest.
pub fn decode_persisted<T: DeserializeOwned>(
    kind: &PersistedKind,
    recorded: Option<&[u8]>,
    bytes: &[u8],
    upcasts: &[UpcastFn<T>],
) -> Result<T, PersistedSchemaError> {
    match recorded_shape(kind, recorded)? {
        RecordedShape::Current => from_bytes(bytes).map_err(PersistedSchemaError::Decode),
        RecordedShape::Prior(index) => upcasts
            .get(index)
            .ok_or_else(|| no_upcast(kind, recorded))
            .and_then(|decode| decode(bytes).map_err(PersistedSchemaError::Decode)),
    }
}

/// Decode persisted `bytes` under `recorded` through the rewriters the registry
/// carries, for a kind whose read entry point is generic (ADR-0187).
///
/// The typed twin of [`decode_persisted`] for the one caller that cannot name
/// its kind at the call site: sealed configuration resolves through a single
/// generic read, so a prior shape is carried forward by
/// [`PersistedUpcast::reshape`] rewriting the row into current-shape bytes,
/// which then decode as `T` the ordinary way.
///
/// # Errors
///
/// [`PersistedSchemaError::Decode`] when the bytes do not decode as the shape
/// the recorded digest named, and [`PersistedSchemaError::NoUpcast`] when this
/// binary has no rewriter for that digest.
pub fn decode_reshaped<T: DeserializeOwned>(
    kind: &PersistedKind,
    recorded: Option<&[u8]>,
    bytes: &[u8],
) -> Result<T, PersistedSchemaError> {
    match recorded_shape(kind, recorded)? {
        RecordedShape::Current => from_bytes(bytes).map_err(PersistedSchemaError::Decode),
        RecordedShape::Prior(index) => kind
            .upcasts
            .get(index)
            .and_then(|prior| prior.reshape)
            .ok_or_else(|| no_upcast(kind, recorded))
            .and_then(|reshape| reshape(bytes).map_err(PersistedSchemaError::Decode))
            .and_then(|current| from_bytes(&current).map_err(PersistedSchemaError::Decode)),
    }
}

/// Which shape the identity stamped beside a row names.
enum RecordedShape {
    /// The shape this binary writes.
    Current,
    /// [`PersistedKind::upcasts`] at this index.
    Prior(usize),
}

fn recorded_shape(kind: &PersistedKind, recorded: Option<&[u8]>) -> Result<RecordedShape, PersistedSchemaError> {
    let current = kind.current_digest();
    match recorded {
        Some(found) if found == current.as_bytes() => Ok(RecordedShape::Current),
        None => match kind.bootstrap {
            Bootstrap::Current => Ok(RecordedShape::Current),
            Bootstrap::Upcast(index) => Ok(RecordedShape::Prior(index)),
        },
        Some(found) => kind
            .upcasts
            .iter()
            .position(|prior| found == prior.digest.as_bytes())
            .map(RecordedShape::Prior)
            .ok_or_else(|| no_upcast(kind, recorded)),
    }
}

/// Refuse `recorded`, naming the identity found and the one this binary writes.
///
/// Reached both when no registered upcast claims the digest and when one does
/// but the read path carries no decoder for it — a registry entry out of
/// lockstep with its entry point. Both are the same thing to a caller: the row
/// names a shape this binary cannot read.
fn no_upcast(kind: &PersistedKind, recorded: Option<&[u8]>) -> PersistedSchemaError {
    PersistedSchemaError::NoUpcast {
        kind: kind.name,
        found: recorded.map_or_else(|| String::from("absent"), encode_hex),
        current: kind.current_digest(),
    }
}

/// The stamp on journaled decisions rows written in the frozen v1 shape
/// (#5330) — including the pre-column rows the ADR-0187 store migration
/// stamped retroactively.
pub const DECISIONS_V1_DIGEST: Digest =
    Digest::pinned("c81d9b3dc65ef7ee0f154c5498ea543a2f8bf8c4fa7ec67828215614a27a52f6");

/// The stamp on journaled decisions rows written before the #5278 propose
/// door appended `Decision` and `Outcome` variants.
pub const DECISIONS_PRE_PROPOSE_DIGEST: Digest =
    Digest::pinned("ee7c8fcea13dc3607ffd0733d3e9f0f6a7b90b011e77b1f36b59ca3eb82d9aab");

/// The stamp on journaled event rows written before the #5278 propose door
/// appended `Fact::ProposeChange`.
pub const EVENT_PRE_PROPOSE_DIGEST: Digest =
    Digest::pinned("0e7389945913e33b12660db11903787e58ce5f1f7f6e19fe64b72d6266d93117");

/// The stamp on journaled decisions rows written before ADR-0216 appended
/// `Decision::DispatchStudy` and the two `Outcome` variants the reader's
/// result decides.
///
/// Copied from the `decisions` line the ledger already carried as current at
/// `5e778243d` — the identity every row written under the pre-reader shape
/// bears — never recomputed from a type here.
pub const DECISIONS_PRE_STUDY_DIGEST: Digest =
    Digest::pinned("7ed0db5b5946fee65b566f4f36229438888552f46863d3552e0230d8037f4108");

/// The stamp on journaled event rows written before ADR-0216 appended
/// `Fact::StudyCompleted`.
///
/// Copied from the `event` line the ledger already carried as current at
/// `5e778243d`, the same way.
pub const EVENT_PRE_STUDY_DIGEST: Digest =
    Digest::pinned("a12b797cefd02c054cc72dca261e09b84564d21d80a4007b2384636d90f480df");

/// The stamp on journaled decisions rows written before ADR-0215 appended
/// `Decision::RecordPipelineManifest`.
///
/// The shape ADR-0216's reader slice left current, copied verbatim from the
/// `decisions` line the ledger carried as current at `3ad573947` — the identity
/// every row written between that slice and this one bears. Never recomputed
/// from live code (#5500), and distinct from
/// [`DECISIONS_PRE_STUDY_DIGEST`]: the two appends landed in sequence, so each
/// names the shape it displaced rather than a shared ancestor.
pub const DECISIONS_PRE_MANIFEST_DIGEST: Digest =
    Digest::pinned("d5c94ca64bbd97709c01b194af82cabce70170f6b354999ad03839bfd52b99ca");

/// The stamp on journaled decisions rows written before ADR-0215's seal-time
/// cross-check appended `SealError::CatalogOutsideDeclaredLanes` and
/// `CatalogError::UndeclaredLaneCommand`.
///
/// The shape the record slice left current, copied verbatim from the
/// `decisions` line the ledger carried as current at `eaab93254` — the identity
/// every row written between that slice and this one bears. Never recomputed
/// from live code (#5500). A `SealError` is journaled through
/// `Outcome::SealRejected`, so widening one moves this column exactly as
/// appending a `Decision` variant does.
pub const DECISIONS_PRE_CROSS_CHECK_DIGEST: Digest =
    Digest::pinned("9ea3845b12c08bee7643a95397d64c4517d6beaa1bd882277a46b79d5eb86b8f");

/// The stamp on journaled decisions rows written before ADR-0215's last
/// slice appended `SealError::UnusablePipelineManifest`.
///
/// The shape the cross-check slice left current, copied verbatim from the
/// `decisions` line the ledger carried as current at `fd967bb57` —
/// `33a113561983582492aec42df044603d3e7e2462556ac8c3f9f7ee76aca1fdfc`. Never
/// recomputed from live code (#5500). A `SealError` is journaled through
/// `Outcome::SealRejected`, so appending one moves this column exactly as
/// appending a `Decision` variant does.
pub const DECISIONS_PRE_MANIFESTLESS_DIGEST: Digest =
    Digest::pinned("33a113561983582492aec42df044603d3e7e2462556ac8c3f9f7ee76aca1fdfc");

/// The stamp on journaled event rows written before aggregate pre-checks
/// appended their preparation, request, and completion facts.
pub const EVENT_PRE_PRECHECK_DIGEST: Digest =
    Digest::pinned("485537c3a579a9c3c4319ecf269fbd1202a46a9fa791da9b857e9fa5c1f1e569");

/// The stamp on journaled decisions rows written before aggregate pre-checks
/// appended their durable state and host-work vocabulary.
pub const DECISIONS_PRE_PRECHECK_DIGEST: Digest =
    Digest::pinned("f44f42438d47caae2833239284d63068ef8b67abef0139e305bbceb543c5d352");

/// Journal schema immediately before shared verification and eager integration.
pub const DECISIONS_PRE_COORDINATION_DIGEST: Digest =
    Digest::pinned("cd1234d263eb61be6bacc167ac49bd83894c505b527726061d6758e3b19cbc24");

/// Event schema immediately before shared verification and eager integration.
pub const EVENT_PRE_COORDINATION_DIGEST: Digest =
    Digest::pinned("55af52a6acdc3e695b0da93f05a73b414485a6526a863e1ab4129a42be788052");

/// The stamp on sealed model-process instruction bundles written before
/// ADR-0216 appended `retrospect` and `retrospect_finding_contract`.
pub const MODEL_PROCESS_INSTRUCTIONS_PRE_READER_DIGEST: Digest =
    Digest::pinned("c0a9677ad8116334fe7b217401fb5f14b06965ae33af4add641f685c5768f3e6");

/// Decode journaled [`Decisions`] under the writing-schema digest stamped
/// beside them (ADR-0187).
///
/// The current identity decodes as today. A missing digest is the implicit v1
/// identity — rows written before the column existed. v1 upcasts by filling
/// `StageProgress::reconcile_assembles_base` as `false`; the pre-#5278,
/// pre-ADR-0216 and the three pre-ADR-0215 shapes decode as today because
/// everything since each is a tail-appended variant. Any other identity is a
/// named refusal.
///
/// # Errors
///
/// [`PersistedSchemaError`] when the bytes do not decode as the named shape,
/// or when this binary has no upcast for the recorded digest.
pub fn decode_recorded_decisions(bytes: &[u8], schema: Option<&[u8]>) -> Result<Decisions, PersistedSchemaError> {
    decode_persisted(
        &DECISIONS,
        schema,
        bytes,
        &[
            upcast_decisions_v1,
            upcast_decisions_pre_propose,
            upcast_decisions_pre_study,
            upcast_decisions_pre_manifest,
            upcast_decisions_pre_cross_check,
            upcast_decisions_pre_manifestless,
            upcast_decisions_pre_precheck,
            upcast_decisions_pre_coordination,
        ],
    )
}

fn upcast_decisions_v1(bytes: &[u8]) -> Result<Decisions, WireError> {
    from_bytes::<DecisionsV1>(bytes).map(Decisions::from)
}

/// Pre-#5278 rows carry the same wire layout today's decoder reads: the fold
/// only appended `Decision` and `Outcome` variants, so every discriminant a
/// row of that era could hold is unmoved.
fn upcast_decisions_pre_propose(bytes: &[u8]) -> Result<Decisions, WireError> {
    from_bytes(bytes)
}

/// Pre-ADR-0216 rows carry the same wire layout today's decoder reads: the
/// reader slice only appended a `Decision` variant and two `Outcome` variants,
/// past every discriminant a row of that era could hold.
fn upcast_decisions_pre_study(bytes: &[u8]) -> Result<Decisions, WireError> {
    from_bytes(bytes)
}

/// Pre-ADR-0215 rows carry the same wire layout today's decoder reads:
/// `Decision::RecordPipelineManifest` is appended past `DispatchStudy`, and so
/// past every discriminant a row of that era could hold, leaving no wire
/// position moved.
fn upcast_decisions_pre_manifest(bytes: &[u8]) -> Result<Decisions, WireError> {
    from_bytes(bytes)
}

/// Pre-cross-check rows carry the same wire layout today's decoder reads: the
/// seal-time cross-check appended one `SealError` variant past
/// `CyclicDependencies` and one `CatalogError` variant past
/// `WallClockOutOfRange`, both tails, so no discriminant a row of that era could
/// hold has moved.
fn upcast_decisions_pre_cross_check(bytes: &[u8]) -> Result<Decisions, WireError> {
    from_bytes(bytes)
}

/// Pre-manifestless-refusal rows carry the same wire layout today's decoder
/// reads: `SealError::UnusablePipelineManifest` is appended past
/// `CatalogOutsideDeclaredLanes`, a tail, so no discriminant a row of that era
/// could hold has moved.
fn upcast_decisions_pre_manifestless(bytes: &[u8]) -> Result<Decisions, WireError> {
    from_bytes(bytes)
}

/// Pre-pre-check rows carry the same wire layout today's decoder reads: the
/// new decision and outcome vocabulary was appended at the enum tails.
fn upcast_decisions_pre_precheck(bytes: &[u8]) -> Result<Decisions, WireError> {
    from_bytes(bytes)
}

fn upcast_decisions_pre_coordination(bytes: &[u8]) -> Result<Decisions, WireError> {
    from_bytes(bytes)
}

/// Pre-#5278 rows carry the same wire layout today's decoder reads: the fold
/// only appended `Fact::ProposeChange`, past every discriminant a row of that
/// era could hold.
fn upcast_event_pre_propose(bytes: &[u8]) -> Result<Event, WireError> {
    from_bytes(bytes)
}

/// Pre-ADR-0216 rows carry the same wire layout today's decoder reads: the
/// reader slice only appended `Fact::StudyCompleted`, past every discriminant a
/// row of that era could hold.
fn upcast_event_pre_study(bytes: &[u8]) -> Result<Event, WireError> {
    from_bytes(bytes)
}

/// Pre-pre-check event rows carry the same wire layout today's decoder reads:
/// the new facts were appended past every prior discriminant.
fn upcast_event_pre_precheck(bytes: &[u8]) -> Result<Event, WireError> {
    from_bytes(bytes)
}

fn upcast_event_pre_coordination(bytes: &[u8]) -> Result<Event, WireError> {
    from_bytes(bytes)
}

/// Pre-ADR-0216 bundles carry seventeen fields where today's decoder reads
/// nineteen, so the row is decoded through its frozen shape and re-encoded
/// with both reader fields empty.
fn reshape_instructions_pre_reader(bytes: &[u8]) -> Result<Vec<u8>, WireError> {
    to_vec(&ModelProcessInstructions::from(from_bytes::<ModelProcessInstructionsPreReader>(bytes)?))
}

/// Decode a journaled [`Event`] under the writing-schema digest stamped beside
/// it (ADR-0187).
///
/// # Errors
///
/// [`PersistedSchemaError`] when the bytes do not decode as the named shape,
/// or when this binary has no upcast for the recorded digest.
pub fn decode_recorded_event(bytes: &[u8], schema: Option<&[u8]>) -> Result<Event, PersistedSchemaError> {
    decode_persisted(
        &EVENT,
        schema,
        bytes,
        &[upcast_event_pre_propose, upcast_event_pre_study, upcast_event_pre_precheck, upcast_event_pre_coordination],
    )
}

/// The [`PersistedKind`] for journaled decisions.
pub static DECISIONS: PersistedKind = PersistedKind {
    name: DECISIONS_KIND,
    schema: &<Decisions as Schema>::SCHEMA,
    bootstrap: Bootstrap::Upcast(0),
    upcasts: &[
        PersistedUpcast { digest: DECISIONS_V1_DIGEST, reshape: None },
        PersistedUpcast { digest: DECISIONS_PRE_PROPOSE_DIGEST, reshape: None },
        PersistedUpcast { digest: DECISIONS_PRE_STUDY_DIGEST, reshape: None },
        PersistedUpcast { digest: DECISIONS_PRE_MANIFEST_DIGEST, reshape: None },
        PersistedUpcast { digest: DECISIONS_PRE_CROSS_CHECK_DIGEST, reshape: None },
        PersistedUpcast { digest: DECISIONS_PRE_MANIFESTLESS_DIGEST, reshape: None },
        PersistedUpcast { digest: DECISIONS_PRE_PRECHECK_DIGEST, reshape: None },
        PersistedUpcast { digest: DECISIONS_PRE_COORDINATION_DIGEST, reshape: None },
    ],
    current: OnceLock::new(),
};

/// The [`PersistedKind`] for journaled events.
pub static EVENT: PersistedKind = PersistedKind {
    name: EVENT_KIND,
    schema: &<Event as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[
        PersistedUpcast { digest: EVENT_PRE_PROPOSE_DIGEST, reshape: None },
        PersistedUpcast { digest: EVENT_PRE_STUDY_DIGEST, reshape: None },
        PersistedUpcast { digest: EVENT_PRE_PRECHECK_DIGEST, reshape: None },
        PersistedUpcast { digest: EVENT_PRE_COORDINATION_DIGEST, reshape: None },
    ],
    current: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`ApprovalPolicy`].
pub static APPROVAL_POLICY: PersistedKind = PersistedKind {
    name: ApprovalPolicy::NAME,
    schema: &<ApprovalPolicy as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`PrecheckPolicy`].
pub static PRECHECK_POLICY: PersistedKind = PersistedKind {
    name: PrecheckPolicy::NAME,
    schema: &<PrecheckPolicy as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`CoordinationPolicy`].
pub static COORDINATION_POLICY: PersistedKind = PersistedKind {
    name: CoordinationPolicy::NAME,
    schema: &<CoordinationPolicy as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`ModelOverride`].
pub static MODEL_OVERRIDE: PersistedKind = PersistedKind {
    name: ModelOverride::NAME,
    schema: &<ModelOverride as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
};

/// The [`PersistedKind`] for the sealed [`ModelProcessInstructions`] bundle.
///
/// The first config kind to carry an upcast: ADR-0216 appended the reader's two
/// instruction fields, and without the pre-reader shape registered here every
/// bundle sealed under ADR-0214 would resolve as
/// [`ConfigResolveError::NoUpcast`](crate::values::ConfigResolveError::NoUpcast).
///
/// The kind postdates the config column's schema-digest migration, so every row
/// of it was stamped as it was written and the bootstrap arm never fires. It is
/// [`Bootstrap::Current`] because no unstamped era exists for this kind to name,
/// not because an absent stamp is known to be today's shape.
pub static MODEL_PROCESS_INSTRUCTIONS: PersistedKind = PersistedKind {
    name: ModelProcessInstructions::NAME,
    schema: &<ModelProcessInstructions as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[PersistedUpcast {
        digest: MODEL_PROCESS_INSTRUCTIONS_PRE_READER_DIGEST,
        reshape: Some(reshape_instructions_pre_reader),
    }],
    current: OnceLock::new(),
};

/// The [`PersistedKind`] for the sealed [`PipelineManifest`] a base declares
/// (ADR-0215). Host-derived rather than operator-authored, and persisted like
/// any other sealed configuration, so its shape is stamped and pinned like one.
pub static PIPELINE_MANIFEST: PersistedKind = PersistedKind {
    name: PipelineManifest::NAME,
    schema: &<PipelineManifest as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`PriceTable`].
pub static PRICE_TABLE: PersistedKind = PersistedKind {
    name: PriceTable::NAME,
    schema: &<PriceTable as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`SpendCeiling`].
pub static SPEND_CEILING: PersistedKind = PersistedKind {
    name: SpendCeiling::NAME,
    schema: &<SpendCeiling as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`StageCatalog`].
pub static STAGE_CATALOG: PersistedKind = PersistedKind {
    name: StageCatalog::NAME,
    schema: &<StageCatalog as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
};

/// Every kind this binary persists. The fixture walks this table.
pub static PERSISTED_KINDS: &[&PersistedKind] = &[
    &DECISIONS,
    &EVENT,
    &APPROVAL_POLICY,
    &PRECHECK_POLICY,
    &COORDINATION_POLICY,
    &MODEL_OVERRIDE,
    &MODEL_PROCESS_INSTRUCTIONS,
    &PIPELINE_MANIFEST,
    &PRICE_TABLE,
    &SPEND_CEILING,
    &STAGE_CATALOG,
];

/// The registry entry whose [`PersistedKind::name`] is `name`, if any.
#[must_use]
pub fn kind_named(name: &str) -> Option<&'static PersistedKind> {
    PERSISTED_KINDS.iter().copied().find(|kind| kind.name == name)
}

#[cfg(test)]
mod tests {
    use aether_data::Schema;
    use aether_data::wire::to_vec;

    use super::{DECISIONS, EVENT, PersistedSchemaError, decode_persisted, decode_recorded_decisions};
    use crate::digest::{Digest, SCHEMA_DIGEST_DOMAIN, schema_digest};
    use crate::reduce::{Decisions, Event, Outcome};

    fn empty_decisions() -> Decisions {
        Decisions { outcome: Outcome::Duplicate, effects: Vec::new() }
    }

    #[test]
    fn a_matching_recorded_digest_takes_the_fast_path() {
        // A digest comparison is the common path: one 32-byte equality and the
        // same decode as today. Computing the digest per row would hash the
        // schema on every journal read.
        let recorded = empty_decisions();
        let bytes = to_vec(&recorded).expect("decisions encode");
        let current = DECISIONS.current_digest();
        let decoded = decode_persisted(&DECISIONS, Some(current.as_bytes()), &bytes, &[super::upcast_decisions_v1])
            .expect("matching digest decodes");
        assert_eq!(decoded, recorded);
    }

    #[test]
    fn rendering_is_stable_across_static_and_owned_schema_cells() {
        // SchemaCell's static and owned forms encode identically, so a schema
        // decoded from the wire must digest the same as a compiled-in const.
        // An unstable rendering would stamp one identity and refuse the other.
        let static_schema = &<Event as Schema>::SCHEMA;
        let owned = static_schema.clone();
        assert_eq!(
            schema_digest(EVENT.name, static_schema).expect("static schema renders"),
            schema_digest(EVENT.name, &owned).expect("owned schema renders")
        );
    }

    #[test]
    fn an_unknown_digest_is_refused_by_name() {
        let found = Digest::from_bytes([0xab; 32]);
        let error = decode_recorded_decisions(&[0xff], Some(found.as_bytes())).expect_err("unknown digest refuses");
        let text = format!("{error}");
        assert!(text.contains("no migration from schema `"), "{text}");
        assert!(text.contains(&found.to_hex()), "{text}");
        assert!(text.contains(&DECISIONS.current_digest().to_hex()), "{text}");
        assert!(text.contains("for kind `decisions`"), "{text}");
        match error {
            PersistedSchemaError::NoUpcast { kind, found: named, current } => {
                assert_eq!(kind, "decisions");
                assert_eq!(named, found.to_hex());
                assert_eq!(current, DECISIONS.current_digest());
            }
            other @ PersistedSchemaError::Decode(_) => panic!("expected NoUpcast, got {other:?}"),
        }
    }

    #[test]
    fn a_schema_digest_does_not_collide_with_a_value_digest_over_the_same_bytes() {
        let rendering = super::render_schema(EVENT.name, &<Event as Schema>::SCHEMA).expect("event schema renders");
        let schema = Digest::of_domain_tagged(SCHEMA_DIGEST_DOMAIN, &rendering);
        let value = Digest::of_domain_tagged("event", &rendering);
        assert_ne!(schema, value);
    }
}
