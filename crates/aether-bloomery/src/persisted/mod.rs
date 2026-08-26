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

mod rendering;

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;
use core::ptr;
use std::sync::OnceLock;

use aether_data::Kind;
use aether_data::Schema;
use aether_data::schema::SchemaType;
use aether_data::wire::{Error as WireError, from_bytes};
use serde::de::DeserializeOwned;

use crate::digest::{Digest, encode_hex, schema_digest};
use crate::reduce::decisions_v1::DecisionsV1;
use crate::reduce::{Decisions, Event};
use crate::values::{ApprovalPolicy, ModelOverride, PriceTable, SpendCeiling, StageCatalog};

pub use rendering::{RenderError, render_schema};

/// Decoder from a prior persisted shape into the current value.
type UpcastFn<T> = fn(&[u8]) -> Result<T, WireError>;

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
    /// Prior shapes this entry can upcast, oldest first. Each schema is
    /// digested under [`Self::name`] so the pin is computed from the shape,
    /// never hand-maintained.
    pub upcasts: &'static [PersistedUpcast],
    current: OnceLock<Digest>,
    upcast_digests: OnceLock<Vec<Digest>>,
}

/// One prior shape a [`PersistedKind`] can carry forward.
pub struct PersistedUpcast {
    /// The prior shape's schema. Its digest is `schema_digest(kind, schema)`.
    pub schema: &'static SchemaType,
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

    /// Digest of a prior shape listed on this kind.
    ///
    /// # Panics
    ///
    /// Panics if the compiled schema exceeds the rendering budget.
    #[must_use]
    pub fn upcast_digest(&self, upcast: &PersistedUpcast) -> Digest {
        let cached = self
            .upcast_digests
            .get_or_init(|| self.upcasts.iter().map(|prior| digest_of_schema(self.name, prior.schema)).collect());
        self.upcasts
            .iter()
            .position(|prior| ptr::eq(prior, upcast))
            .and_then(|index| cached.get(index).copied())
            .unwrap_or_else(|| digest_of_schema(self.name, upcast.schema))
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
    let current = kind.current_digest();
    match recorded {
        Some(found) if found == current.as_bytes() => from_bytes(bytes).map_err(PersistedSchemaError::Decode),
        None => decode_bootstrap(kind, bytes, upcasts),
        Some(found) => decode_upcast(kind, found, bytes, upcasts, current),
    }
}

fn decode_bootstrap<T: DeserializeOwned>(
    kind: &PersistedKind,
    bytes: &[u8],
    upcasts: &[UpcastFn<T>],
) -> Result<T, PersistedSchemaError> {
    match kind.bootstrap {
        Bootstrap::Current => from_bytes(bytes).map_err(PersistedSchemaError::Decode),
        Bootstrap::Upcast(index) => upcasts
            .get(index)
            .ok_or_else(|| PersistedSchemaError::NoUpcast {
                kind: kind.name,
                found: String::from("absent"),
                current: kind.current_digest(),
            })
            .and_then(|decode| decode(bytes).map_err(PersistedSchemaError::Decode)),
    }
}

fn decode_upcast<T: DeserializeOwned>(
    kind: &PersistedKind,
    found: &[u8],
    bytes: &[u8],
    upcasts: &[UpcastFn<T>],
    current: Digest,
) -> Result<T, PersistedSchemaError> {
    for (prior, decode) in kind.upcasts.iter().zip(upcasts.iter()) {
        if found == kind.upcast_digest(prior).as_bytes() {
            return decode(bytes).map_err(PersistedSchemaError::Decode);
        }
    }
    Err(PersistedSchemaError::NoUpcast { kind: kind.name, found: encode_hex(found), current })
}

/// Decode journaled [`Decisions`] under the writing-schema digest stamped
/// beside them (ADR-0187).
///
/// The current identity decodes as today. A missing digest is the implicit v1
/// identity — rows written before the column existed. v1 upcasts by filling
/// `StageProgress::reconcile_assembles_base` as `false`. Any other identity is
/// a named refusal.
///
/// # Errors
///
/// [`PersistedSchemaError`] when the bytes do not decode as the named shape,
/// or when this binary has no upcast for the recorded digest.
pub fn decode_recorded_decisions(bytes: &[u8], schema: Option<&[u8]>) -> Result<Decisions, PersistedSchemaError> {
    decode_persisted(&DECISIONS, schema, bytes, &[upcast_decisions_v1])
}

fn upcast_decisions_v1(bytes: &[u8]) -> Result<Decisions, WireError> {
    from_bytes::<DecisionsV1>(bytes).map(Decisions::from)
}

/// Decode a journaled [`Event`] under the writing-schema digest stamped beside
/// it (ADR-0187).
///
/// # Errors
///
/// [`PersistedSchemaError`] when the bytes do not decode as the named shape,
/// or when this binary has no upcast for the recorded digest.
pub fn decode_recorded_event(bytes: &[u8], schema: Option<&[u8]>) -> Result<Event, PersistedSchemaError> {
    decode_persisted(&EVENT, schema, bytes, &[])
}

/// The [`PersistedKind`] for journaled decisions.
pub static DECISIONS: PersistedKind = PersistedKind {
    name: DECISIONS_KIND,
    schema: &<Decisions as Schema>::SCHEMA,
    bootstrap: Bootstrap::Upcast(0),
    upcasts: &[PersistedUpcast { schema: &<DecisionsV1 as Schema>::SCHEMA }],
    current: OnceLock::new(),
    upcast_digests: OnceLock::new(),
};

/// The [`PersistedKind`] for journaled events.
pub static EVENT: PersistedKind = PersistedKind {
    name: EVENT_KIND,
    schema: &<Event as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
    upcast_digests: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`ApprovalPolicy`].
pub static APPROVAL_POLICY: PersistedKind = PersistedKind {
    name: ApprovalPolicy::NAME,
    schema: &<ApprovalPolicy as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
    upcast_digests: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`ModelOverride`].
pub static MODEL_OVERRIDE: PersistedKind = PersistedKind {
    name: ModelOverride::NAME,
    schema: &<ModelOverride as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
    upcast_digests: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`PriceTable`].
pub static PRICE_TABLE: PersistedKind = PersistedKind {
    name: PriceTable::NAME,
    schema: &<PriceTable as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
    upcast_digests: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`SpendCeiling`].
pub static SPEND_CEILING: PersistedKind = PersistedKind {
    name: SpendCeiling::NAME,
    schema: &<SpendCeiling as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
    upcast_digests: OnceLock::new(),
};

/// The [`PersistedKind`] for sealed [`StageCatalog`].
pub static STAGE_CATALOG: PersistedKind = PersistedKind {
    name: StageCatalog::NAME,
    schema: &<StageCatalog as Schema>::SCHEMA,
    bootstrap: Bootstrap::Current,
    upcasts: &[],
    current: OnceLock::new(),
    upcast_digests: OnceLock::new(),
};

/// Every kind this binary persists. The fixture walks this table.
pub static PERSISTED_KINDS: &[&PersistedKind] =
    &[&DECISIONS, &EVENT, &APPROVAL_POLICY, &MODEL_OVERRIDE, &PRICE_TABLE, &SPEND_CEILING, &STAGE_CATALOG];

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
