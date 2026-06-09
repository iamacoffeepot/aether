//! aether-data: the universal data layer (ADR-0069).
//!
//! Single home for everything that describes typed bytes — what makes
//! a kind a kind, what its schema looks like, how its identity is
//! computed, how the bytes are walked. Used by mail dispatch (the
//! original consumer), by the codec (`aether-codec`), and by any
//! future schema-described data consumer (the prompt-system save
//! format being the next).
//!
//! Two payload tiers (ADR-0005):
//!   - POD: `#[repr(C)]` types implementing `bytemuck::NoUninit` /
//!     `AnyBitPattern`. Encoded as their native byte layout; decoded
//!     zero-copy to `&T` or `&[T]`. Used for vertex streams, fixed-
//!     layout structs, anything where throughput or zero-copy matters.
//!   - Structural: `serde::Serialize + DeserializeOwned` types. Encoded
//!     with postcard (Rust-native, varint-compact, no_std-friendly).
//!     Used for small control messages with Option/Vec/enum shape.
//!
//! A type picks one tier — not both — as part of its contract.
//!
//! ## What lives here
//!
//! - **Typed-id newtypes** (ADR-0064 / ADR-0065): `MailboxId`, `KindId`,
//!   `HandleId`, plus `Tag`, tag-bit constants, and FNV hashing.
//! - **Schema vocabulary** (ADR-0019 / ADR-0031 / ADR-0032): `SchemaType`,
//!   `LabelNode`, `KindShape`, `KindLabels`, `InputsRecord`, canonical
//!   bytes encoders.
//! - **Kind / Schema / `CastEligible` traits** (ADR-0030): the binding
//!   between a Rust type and its wire form.
//! - **`Ref<K>`** (ADR-0045): typed handle reference for fields that
//!   may inline a value or carry a handle into the substrate's store.
//! - **Encode / decode helpers**: the `encode` / `decode` family for
//!   POD and postcard kinds.
//! - **`__inventory`** (issue #243): native-only auto-collection of
//!   `#[derive(Kind)]` types into the substrate's descriptor list.

#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use serde::de::DeserializeOwned;

pub mod canonical;
pub mod hash;
pub mod ids;
pub mod mail;
#[cfg(not(target_arch = "wasm32"))]
pub mod name_inventory;
pub mod schema;
pub mod tag_bits;
pub mod tagged_id;
pub mod transform;
pub mod wire_id;
pub use hash::{
    HANDLE_DOMAIN, KIND_DOMAIN, MAILBOX_DOMAIN, THREAD_DOMAIN, TRANSFORM_DOMAIN, TYPE_DOMAIN,
    content_addressed_handle_id, fnv1a_64_bytes, fnv1a_64_prefixed, fold_lineage,
    mailbox_id_from_name, mailbox_id_from_name_pair, mailbox_id_from_path, thread_id_from_name,
};
pub use ids::{
    ActorId, DagId, HandleId, KindId, MailboxId, ThreadId, TransformId, tag_for_type_id,
    type_name_for_type_id,
};
pub use mail::{MailId, Source, SourceAddr};
#[cfg(not(target_arch = "wasm32"))]
pub use name_inventory::{
    NameEntry, ParamKind, TemplateEntry, build_static_reverse_map, fill_template, id_for_name,
    name_entries, template_entries,
};
pub use schema::*;
pub use tagged_id::{Tag, with_tag};
pub use transform::{InvokeFn, TransformError};
#[cfg(not(target_arch = "wasm32"))]
pub use transform::{TransformEntry, transforms};
pub use wire_id::{EngineId, SessionToken, Uuid};

/// Re-exported derive macros from `aether-actor-derive`. Behind the
/// `derive` feature so `cargo build` on a guest that hand-writes
/// `impl Kind` doesn't pay the proc-macro compile cost. The
/// `#[actor]` / `#[handler]` / `#[fallback]` attribute macros
/// (ADR-0033) ride in the same crate because adding a second proc-
/// macro crate would double consumer compile cost for no separation
/// gain — both derives and attributes expand into the same runtime
/// surface. Issue 552 stage 0 consolidated the prior
/// `aether-data-derive` + `aether-component`-internal-only macro
/// split into a single `aether-actor-derive` proc-macro crate so the
/// SDK and the derive share a home.
#[cfg(feature = "derive")]
pub use aether_actor_derive::{
    Embeddable, Instanced, Kind, Schema, Singleton, actor, bridge, capability, fallback, handler,
    local,
};

/// Re-exported `#[transform]` attribute macro from `aether-data-derive`
/// (ADR-0048 §1). A transform is a pure `Kind -> Kind` data-layer
/// primitive — no actor dependence — so its macro re-exports from the
/// data layer as `aether_data::transform`, not from the actor SDK. Lives
/// in the sibling `aether-data-derive` crate because `aether-data` is
/// `no_std` + `alloc` and cannot itself be `proc-macro = true`. Behind
/// the `derive` feature like the other macros.
#[cfg(feature = "derive")]
pub use aether_data_derive::transform;

/// Identifies a mail kind by a stable, namespaced string name (e.g.
/// `"aether.tick"`, `"hello.npc_health"`) and a `u64` id derived from
/// that name plus the kind's canonical schema bytes (ADR-0030 Phase 2,
/// ADR-0032). Both sides of the FFI compute the id the same way — the
/// substrate from the deserialized schema, the guest from the compile-
/// time const — so routing stays in lockstep without a host-fn resolve.
pub trait Kind {
    const NAME: &'static str;
    const ID: KindId;

    /// Decode a single instance from substrate-supplied bytes. The
    /// `Kind` derive auto-implements this with the right body for the
    /// type's wire shape (cast for `#[repr(C)]` + `Pod`, postcard
    /// otherwise). Hand-rolled `Kind` impls that don't participate in
    /// `#[actor]` receive dispatch can leave the default — it
    /// returns `None`, which the SDK surfaces as a strict-receiver
    /// miss (`DISPATCH_UNKNOWN_KIND`).
    ///
    /// The dispatcher synthesised by `#[actor]` calls this through
    /// `Mail::decode_kind::<K>()`, which hands `bytes` already sliced
    /// to the substrate-supplied `byte_len` so the decoder is bounded
    /// by the actual frame and can't read past the substrate-written
    /// payload into adjacent linear memory.
    #[must_use]
    fn decode_from_bytes(_bytes: &[u8]) -> Option<Self>
    where
        Self: Sized,
    {
        None
    }

    /// Encode `self` into a fresh byte buffer in the wire shape this
    /// kind was declared with. The `Kind` derive auto-implements this
    /// using the same `#[repr(C)]` autodetect as `decode_from_bytes`
    /// (cast for `#[repr(C)]` + `NoUninit`, postcard otherwise), so a
    /// single `Sink::send` / `Ctx::reply` call site dispatches both
    /// wire shapes without the caller picking the encoder.
    ///
    /// Default panics — sending a kind whose impl was hand-rolled
    /// without an override is a contract violation, not "I have no
    /// payload" (the symmetric `decode_from_bytes` default returns
    /// `None`, which the dispatcher surfaces as `DISPATCH_UNKNOWN_KIND`;
    /// silently shipping zero bytes here would write a garbled mail
    /// rather than fail loud). Hand-rolled `Kind` impls that need to
    /// send must override.
    fn encode_into_bytes(&self) -> Vec<u8> {
        panic!(
            "aether-data: Kind::encode_into_bytes called on `{}` whose impl does not override \
             it. Use `#[derive(Kind)]` (which emits the body for cast or postcard kinds based \
             on `#[repr(C)]`) or hand-roll an override before sending.",
            Self::NAME,
        );
    }
}

/// `Kind` impl for the unit type. Lets `()` ride the same
/// `Kind::decode_from_bytes` / `Kind::encode_into_bytes` shim path as
/// real kinds, which is what makes the `FfiActor::Config = ()` default
/// (ADR-0090) decode through a uniform macro body. A zero-length byte
/// slice decodes to `Some(())`; any non-empty slice returns `None`.
/// Encoding is the empty byte vector.
///
/// The `NAME` (`"aether.unit"`) gives the unit kind a stable wire name
/// for the rare case it surfaces in diagnostics (`describe_kinds` does
/// not enumerate it because it is not collected via inventory — the
/// `#[cfg(not(target_arch = "wasm32"))]` inventory submission lives in
/// the `Kind` derive, which this hand-rolled impl bypasses).
impl Kind for () {
    const NAME: &'static str = "aether.unit";
    const ID: KindId = KindId(with_tag(
        Tag::Kind,
        fnv1a_64_prefixed(KIND_DOMAIN, Self::NAME.as_bytes()),
    ));

    fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() { Some(()) } else { None }
    }

    fn encode_into_bytes(&self) -> Vec<u8> {
        Vec::new()
    }
}

/// Compile-time predicate: can this type's payload travel across the
/// wire as raw `#[repr(C)]` bytes (and decode by `bytemuck::cast`)?
///
/// Used by `#[derive(Kind)]` to compute `SchemaType::Struct.repr_c`
/// at the consumer's compile time without losing recursion: a struct
/// whose fields are all `CastEligible` is itself eligible *iff* it
/// also carries `#[repr(C)]`. Anything containing a `String`, `Vec`,
/// `Option`, enum, or non-`#[repr(C)]` substruct short-circuits to
/// `false`, which forces the postcard wire path on the descriptor.
pub trait CastEligible {
    const ELIGIBLE: bool;
}

macro_rules! cast_eligible_primitive {
    ($($t:ty),* $(,)?) => {
        $( impl CastEligible for $t { const ELIGIBLE: bool = true; } )*
    };
}

cast_eligible_primitive!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64, bool);

// Typed-id newtypes are `#[repr(transparent)]` over `u64`, so a
// cast-shape struct field typed `MailboxId` / `KindId` / `HandleId`
// is wire-identical to a `u64` field.
impl CastEligible for MailboxId {
    const ELIGIBLE: bool = true;
}
impl CastEligible for KindId {
    const ELIGIBLE: bool = true;
}
impl CastEligible for HandleId {
    const ELIGIBLE: bool = true;
}
impl CastEligible for DagId {
    const ELIGIBLE: bool = true;
}
impl CastEligible for TransformId {
    const ELIGIBLE: bool = true;
}
impl CastEligible for ThreadId {
    const ELIGIBLE: bool = true;
}

impl<T: CastEligible, const N: usize> CastEligible for [T; N] {
    const ELIGIBLE: bool = T::ELIGIBLE;
}

// Variable-length and sum-shaped stdlib types are explicitly cast
// *in*-eligible. The derive's emitted `ELIGIBLE` const ANDs every
// field's value, so without these impls a `#[repr(C)]` struct
// containing a `String` would fail to compile — the trait bound
// on `<String as CastEligible>::ELIGIBLE` would be unsatisfied.
// Listing them here is the price of not having stable Rust
// specialization; new "definitely not eligible" types can be added
// as the kind vocabulary reaches for them.
impl CastEligible for String {
    const ELIGIBLE: bool = false;
}
impl<T> CastEligible for Vec<T> {
    const ELIGIBLE: bool = false;
}
impl<T> CastEligible for Option<T> {
    const ELIGIBLE: bool = false;
}
impl<K> CastEligible for Ref<K> {
    const ELIGIBLE: bool = false;
}
// Issue #232: `BTreeMap<K, V>` is variable-length and disqualifies a
// parent struct from `repr_c`, same as `Vec`/`String`/`Option`.
impl<K, V> CastEligible for BTreeMap<K, V> {
    const ELIGIBLE: bool = false;
}

/// ADR-0045 typed handle reference — wire form for fields that
/// accept either an inline kind value or a handle pointing into the
/// substrate's handle store.
///
/// `Ref<K>` lets a field carry one of two payloads on the wire:
///
/// - `Ref::Inline(K)` — the entire `K` value travels inline. The
///   substrate dispatches identically to a non-`Ref` field after
///   the field-walk step substitutes the inline value.
/// - `Ref::Handle { id, kind_id }` — a reference into the
///   substrate's handle store. On dispatch the substrate looks up
///   `id` and either substitutes the resolved value or parks the
///   mail until the handle resolves (ADR-0045 §4).
///
/// `kind_id` on the `Handle` arm MUST equal `<K as Kind>::ID`. The
/// substrate validates this against the field's expected type
/// before substitution; a mismatched id is a wire-corruption-class
/// error, not a recoverable one. Use [`Ref::handle`] instead of
/// constructing `Handle` directly to pull the id from the kind
/// system rather than passing it by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref<K> {
    /// Inline value — the whole `K` payload is on the wire.
    Inline(K),
    /// Handle reference into the substrate's handle store. `id`
    /// addresses the entry; `kind_id` carries `<K as Kind>::ID` so
    /// the substrate validates type compatibility before
    /// substituting the resolved value.
    Handle { id: u64, kind_id: u64 },
}

impl<K: Kind> Ref<K> {
    /// Construct a `Ref::Handle` with `kind_id` pulled from
    /// `K::ID`. Preferred over hand-constructing the variant —
    /// callers can't pass a kind id that disagrees with the type
    /// parameter.
    #[must_use]
    pub const fn handle(id: u64) -> Self {
        Self::Handle {
            id,
            kind_id: K::ID.0,
        }
    }
}

impl<K> Ref<K> {
    /// Wrap an owned value as `Ref::Inline`. Convenience for call
    /// sites that have the value but want the field shape.
    pub const fn inline(value: K) -> Self {
        Self::Inline(value)
    }

    /// Returns `true` for `Ref::Inline`, `false` for
    /// `Ref::Handle`. Cheap predicate for call sites that branch
    /// on resolution state.
    pub const fn is_inline(&self) -> bool {
        matches!(self, Self::Inline(_))
    }

    /// Returns `true` for `Ref::Handle`, `false` for
    /// `Ref::Inline`.
    pub const fn is_handle(&self) -> bool {
        matches!(self, Self::Handle { .. })
    }

    /// The wire `id` if this is a `Ref::Handle`, `None` for
    /// inline. `kind_id` is recoverable via `<K as Kind>::ID` so
    /// no separate accessor is provided.
    pub const fn handle_id(&self) -> Option<u64> {
        match self {
            Self::Handle { id, .. } => Some(*id),
            Self::Inline(_) => None,
        }
    }
}

/// Hand-written `serde` for `Ref<K>` (ADR-0100). The inline arm carries
/// the kind's own codec image — `K::encode_into_bytes`, length-prefixed
/// — instead of serde-encoding the typed `K`. A cast kind's inline value
/// stays a cast image, and `Ref<K>` needs only `K: Kind`: no
/// `Serialize`/`Deserialize` bound on the wrapped kind.
///
/// The externally-tagged enum representation is preserved — variant
/// index 0 = `Inline`, 1 = `Handle` — so a `Ref::Handle` value's wire
/// bytes are byte-identical to the prior derive and the splice walker's
/// discriminant semantics are unchanged. Inline wire is
/// `disc 0 + varint(len) + K::encode_into_bytes` (`len` bytes); handle
/// wire is `disc 1 + varint(id) + varint(kind_id)`. The handle-store
/// splice walker and the schema-driven JSON codec share this framing.
mod ref_serde {
    use core::fmt;
    use core::marker::PhantomData;

    use alloc::vec::Vec;
    use serde::de::{self, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};
    use serde::ser::SerializeStructVariant;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::{Kind, Ref};

    const VARIANTS: &[&str] = &["Inline", "Handle"];
    const HANDLE_FIELDS: &[&str] = &["id", "kind_id"];

    impl<K: Kind> Serialize for Ref<K> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            match self {
                Self::Inline(value) => {
                    let encoded = value.encode_into_bytes();
                    serializer.serialize_newtype_variant("Ref", 0, "Inline", &InlineBody(&encoded))
                }
                Self::Handle { id, kind_id } => {
                    let mut sv = serializer.serialize_struct_variant("Ref", 1, "Handle", 2)?;
                    sv.serialize_field("id", id)?;
                    sv.serialize_field("kind_id", kind_id)?;
                    sv.end()
                }
            }
        }
    }

    /// Forces `serialize_bytes` for the inline body so the wire is
    /// `varint(len) + raw bytes` — the raw `K::encode_into_bytes` image.
    /// A plain `Vec<u8>` would serialize as a sequence and two-byte any
    /// `>= 0x80` element, corrupting a cast image.
    struct InlineBody<'a>(&'a [u8]);

    impl Serialize for InlineBody<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_bytes(self.0)
        }
    }

    impl<'de, K: Kind> Deserialize<'de> for Ref<K> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_enum("Ref", VARIANTS, RefVisitor(PhantomData))
        }
    }

    /// Externally-tagged variant discriminant. Binary serializers (the
    /// postcard wire) read it as `varint` → `visit_u64`; self-describing
    /// ones read the variant name → `visit_str`.
    enum VariantTag {
        Inline,
        Handle,
    }

    impl<'de> Deserialize<'de> for VariantTag {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_identifier(VariantTagVisitor)
        }
    }

    struct VariantTagVisitor;

    impl Visitor<'_> for VariantTagVisitor {
        type Value = VariantTag;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("`Inline` or `Handle` variant")
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> Result<VariantTag, E> {
            match value {
                0 => Ok(VariantTag::Inline),
                1 => Ok(VariantTag::Handle),
                _ => Err(de::Error::invalid_value(
                    de::Unexpected::Unsigned(value),
                    &self,
                )),
            }
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<VariantTag, E> {
            match value {
                "Inline" => Ok(VariantTag::Inline),
                "Handle" => Ok(VariantTag::Handle),
                _ => Err(de::Error::unknown_variant(value, VARIANTS)),
            }
        }
    }

    struct RefVisitor<K>(PhantomData<K>);

    impl<'de, K: Kind> Visitor<'de> for RefVisitor<K> {
        type Value = Ref<K>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a Ref enum (`Inline` or `Handle`)")
        }

        fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Ref<K>, A::Error> {
            match data.variant()? {
                (VariantTag::Inline, variant) => {
                    let body: InlineBuf = variant.newtype_variant()?;
                    K::decode_from_bytes(&body.0)
                        .map(Ref::Inline)
                        .ok_or_else(|| {
                            de::Error::custom("Ref::Inline payload failed Kind::decode_from_bytes")
                        })
                }
                (VariantTag::Handle, variant) => {
                    variant.struct_variant(HANDLE_FIELDS, HandleVisitor(PhantomData))
                }
            }
        }
    }

    /// Reads the inline body `InlineBody` wrote — a raw byte buffer.
    struct InlineBuf(Vec<u8>);

    impl<'de> Deserialize<'de> for InlineBuf {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer
                .deserialize_byte_buf(InlineBufVisitor)
                .map(InlineBuf)
        }
    }

    struct InlineBufVisitor;

    impl<'de> Visitor<'de> for InlineBufVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("the inline Ref byte buffer")
        }

        fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Vec<u8>, E> {
            Ok(value.to_vec())
        }

        fn visit_byte_buf<E: de::Error>(self, value: Vec<u8>) -> Result<Vec<u8>, E> {
            Ok(value)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(byte) = seq.next_element::<u8>()? {
                out.push(byte);
            }
            Ok(out)
        }
    }

    struct HandleVisitor<K>(PhantomData<K>);

    impl<'de, K> Visitor<'de> for HandleVisitor<K> {
        type Value = Ref<K>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a Ref::Handle with `id` and `kind_id`")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Ref<K>, A::Error> {
            let id = seq
                .next_element::<u64>()?
                .ok_or_else(|| de::Error::invalid_length(0, &self))?;
            let kind_id = seq
                .next_element::<u64>()?
                .ok_or_else(|| de::Error::invalid_length(1, &self))?;
            Ok(Ref::Handle { id, kind_id })
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ref<K>, A::Error> {
            let mut id: Option<u64> = None;
            let mut kind_id: Option<u64> = None;
            while let Some(key) = map.next_key::<HandleField>()? {
                match key {
                    HandleField::Id => {
                        if id.is_some() {
                            return Err(de::Error::duplicate_field("id"));
                        }
                        id = Some(map.next_value()?);
                    }
                    HandleField::KindId => {
                        if kind_id.is_some() {
                            return Err(de::Error::duplicate_field("kind_id"));
                        }
                        kind_id = Some(map.next_value()?);
                    }
                }
            }
            let id = id.ok_or_else(|| de::Error::missing_field("id"))?;
            let kind_id = kind_id.ok_or_else(|| de::Error::missing_field("kind_id"))?;
            Ok(Ref::Handle { id, kind_id })
        }
    }

    enum HandleField {
        Id,
        KindId,
    }

    impl<'de> Deserialize<'de> for HandleField {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_identifier(HandleFieldVisitor)
        }
    }

    struct HandleFieldVisitor;

    impl Visitor<'_> for HandleFieldVisitor {
        type Value = HandleField;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("`id` or `kind_id`")
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> Result<HandleField, E> {
            match value {
                0 => Ok(HandleField::Id),
                1 => Ok(HandleField::KindId),
                _ => Err(de::Error::invalid_value(
                    de::Unexpected::Unsigned(value),
                    &self,
                )),
            }
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<HandleField, E> {
            match value {
                "id" => Ok(HandleField::Id),
                "kind_id" => Ok(HandleField::KindId),
                _ => Err(de::Error::unknown_field(value, HANDLE_FIELDS)),
            }
        }
    }
}

/// ADR-0019 schema producer. Reads `<T as Schema>::SCHEMA` — a compile-
/// time const — to learn how a kind's payload is laid out.
///
/// Blanket impls cover the leaf types in the schema vocabulary
/// (primitives, `String`, `[u8]`-shaped `Vec`s, fixed arrays,
/// `Option`, generic `Vec`). User structs reach the trait via
/// `#[derive(Schema)]`.
///
/// ADR-0031: the schema is a compile-time `const` rather than a
/// runtime `fn`. Taking a reference produces a `&'static SchemaType`
/// which is what `SchemaCell::Static` holds, so nested types
/// (`Vec<T>`, `Option<T>`, `[T; N]`, user structs) resolve by
/// trait dispatch at compile time without a syntactic walker.
///
/// ADR-0032: additionally exposes the Rust type path (`LABEL`) and
/// the parallel labels tree (`LABEL_NODE`). The canonical schema
/// bytes the `aether.kinds` section carries are positional-only
/// (no field/variant/type names); `LABEL_NODE` is serialized into
/// the `aether.kinds.labels` sidecar so the hub can reconstruct a
/// named `SchemaType` for its encoder/decoder and `describe_kinds`
/// output. Primitives, `String`, `Vec<T>`, `Option<T>`, `[T; N]`
/// all carry `LABEL = None` — the containers have no nominal
/// identity and primitives are uniquely determined by their
/// `SchemaType::Scalar(_)` tag.
pub trait Schema {
    const SCHEMA: SchemaType;
    const LABEL: Option<&'static str>;
    const LABEL_NODE: LabelNode;
}

mod schema_impls {
    use alloc::string::String;
    use alloc::vec::Vec;

    use crate::schema::{LabelCell, LabelNode, Primitive, SchemaCell, SchemaType};
    use crate::{DagId, HandleId, KindId, MailboxId, Schema, ThreadId, TransformId};
    use alloc::collections::BTreeMap;

    macro_rules! scalar {
        ($t:ty, $p:ident) => {
            impl Schema for $t {
                const SCHEMA: SchemaType = SchemaType::Scalar(Primitive::$p);
                const LABEL: Option<&'static str> = None;
                const LABEL_NODE: LabelNode = LabelNode::Anonymous;
            }
        };
    }

    scalar!(u8, U8);
    scalar!(u16, U16);
    scalar!(u32, U32);
    scalar!(u64, U64);
    scalar!(i8, I8);
    scalar!(i16, I16);
    scalar!(i32, I32);
    scalar!(i64, I64);
    scalar!(f32, F32);
    scalar!(f64, F64);

    impl Schema for bool {
        const SCHEMA: SchemaType = SchemaType::Bool;
        const LABEL: Option<&'static str> = None;
        const LABEL_NODE: LabelNode = LabelNode::Anonymous;
    }

    // ADR-0090: the unit kind. Its schema is `SchemaType::Unit` and its
    // label is anonymous. Pairs with the `impl Kind for ()` above so a
    // 0-byte payload round-trips through `<() as Kind>::decode_from_bytes`
    // — the macro's `Config = ()` default depends on it.
    impl Schema for () {
        const SCHEMA: SchemaType = SchemaType::Unit;
        const LABEL: Option<&'static str> = None;
        const LABEL_NODE: LabelNode = LabelNode::Anonymous;
    }

    impl Schema for String {
        const SCHEMA: SchemaType = SchemaType::String;
        const LABEL: Option<&'static str> = None;
        const LABEL_NODE: LabelNode = LabelNode::Anonymous;
    }

    // Generic `Vec<T>`. `Vec<u8>` is the canonical byte-buffer shape
    // and the wire vocabulary has a `Bytes` arm to render it as
    // base64/JSON-array params at the hub. We can't add a specialized
    // `impl Schema for Vec<u8>` here because Rust's overlap rules
    // forbid it without nightly specialization — so the derive macro
    // pattern-matches `Vec<u8>` on field types and emits
    // `SchemaType::Bytes` directly, bypassing this blanket. Standalone
    // `Vec<u8>` outside a derived struct still routes through this
    // impl and lands as `Vec(Scalar(U8))`.
    impl<T: Schema + 'static> Schema for Vec<T> {
        const SCHEMA: SchemaType = SchemaType::Vec(SchemaCell::Static(&T::SCHEMA));
        const LABEL: Option<&'static str> = None;
        const LABEL_NODE: LabelNode = LabelNode::Vec(LabelCell::Static(&T::LABEL_NODE));
    }

    impl<T: Schema + 'static> Schema for Option<T> {
        const SCHEMA: SchemaType = SchemaType::Option(SchemaCell::Static(&T::SCHEMA));
        const LABEL: Option<&'static str> = None;
        const LABEL_NODE: LabelNode = LabelNode::Option(LabelCell::Static(&T::LABEL_NODE));
    }

    impl<T: Schema + 'static, const N: usize> Schema for [T; N] {
        const SCHEMA: SchemaType = SchemaType::Array {
            element: SchemaCell::Static(&T::SCHEMA),
            // Schema array lengths are bounded by `u32` on the wire
            // (canonical bytes format). Const-context `try_into` is
            // unavailable; arrays exceeding `u32::MAX` aren't a
            // realistic schema shape and would fail elsewhere first.
            #[allow(clippy::cast_possible_truncation)]
            len: N as u32,
        };
        const LABEL: Option<&'static str> = None;
        const LABEL_NODE: LabelNode = LabelNode::Array(LabelCell::Static(&T::LABEL_NODE));
    }

    // ADR-0064 / ADR-0065 typed-id newtypes.
    impl Schema for MailboxId {
        const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
        const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
        const LABEL_NODE: LabelNode = LabelNode::Anonymous;
    }

    impl Schema for KindId {
        const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
        const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
        const LABEL_NODE: LabelNode = LabelNode::Anonymous;
    }

    impl Schema for HandleId {
        const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
        const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
        const LABEL_NODE: LabelNode = LabelNode::Anonymous;
    }

    impl Schema for DagId {
        const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
        const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
        const LABEL_NODE: LabelNode = LabelNode::Anonymous;
    }

    impl Schema for TransformId {
        const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
        const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
        const LABEL_NODE: LabelNode = LabelNode::Anonymous;
    }

    impl Schema for ThreadId {
        const SCHEMA: SchemaType = SchemaType::TypeId(Self::TYPE_ID);
        const LABEL: Option<&'static str> = Some(Self::TYPE_NAME);
        const LABEL_NODE: LabelNode = LabelNode::Anonymous;
    }

    // ADR-0045 typed handle reference. `Ref<K>` exposes both the
    // inline-value path and the handle-id path through one schema
    // tag — recipients walk fields and dispatch identically once
    // the substrate has substituted the resolved value. The bound
    // is `Schema + 'static` (matching `Vec<T>` etc.) rather than
    // `Kind` because the schema impl only needs the inner
    // `K::SCHEMA` and `K::LABEL_NODE`; the `Kind` bound on `Ref<K>`
    // helpers in lib.rs is for `K::ID` access at construction time.
    impl<K: Schema + 'static> Schema for super::Ref<K> {
        const SCHEMA: SchemaType = SchemaType::Ref(SchemaCell::Static(&K::SCHEMA));
        const LABEL: Option<&'static str> = None;
        const LABEL_NODE: LabelNode = LabelNode::Ref(LabelCell::Static(&K::LABEL_NODE));
    }

    // Issue #232: `BTreeMap<K, V>` lands as `SchemaType::Map`. The
    // `Ord` bound is what proto3-style stringify-and-canonicalize
    // relies on at the codec layer — sorted iteration makes the
    // wire bytes deterministic without a runtime sort, and
    // `Vec`/`Option`/`f32`/`f64` are unreachable as keys at the
    // type level. `HashMap<K, V>` is rejected at the derive layer
    // (mail-derive) because its iteration order is platform-
    // dependent and would diverge canonical bytes across builds.
    impl<K: Schema + Ord + 'static, V: Schema + 'static> Schema for BTreeMap<K, V> {
        const SCHEMA: SchemaType = SchemaType::Map {
            key: SchemaCell::Static(&K::SCHEMA),
            value: SchemaCell::Static(&V::SCHEMA),
        };
        const LABEL: Option<&'static str> = None;
        const LABEL_NODE: LabelNode = LabelNode::Map {
            key: LabelCell::Static(&K::LABEL_NODE),
            value: LabelCell::Static(&V::LABEL_NODE),
        };
    }
}

/// Native-only auto-collection slot for `#[derive(Kind)]` types
/// (issue #243). The Kind derive emits a `cfg(not(target_arch = "wasm32"))`-
/// gated `inventory::submit! { DescriptorEntry { ... } }` against
/// this module's slot; `aether-kinds::descriptors::all()` materializes
/// the Hub-shipped `KindDescriptor` list by iterating the slot at
/// boot. The wasm path (`aether.kinds` custom section, ADR-0032)
/// is unchanged — wasm guests have no inventory dep at all (see
/// the target-gated dependency in Cargo.toml).
///
/// `DescriptorEntry` carries `&'static str` + `&'static SchemaType`
/// so `inventory::submit!` (which requires the value be const-
/// constructible at compile time) accepts it. The derive points
/// `schema` at the per-type `__AETHER_SCHEMA_<NAME>` static it
/// already emits, so no extra storage is required.
///
/// Not part of the public API; the derive macro is the only
/// intended caller.
#[cfg(not(target_arch = "wasm32"))]
#[doc(hidden)]
pub mod __inventory {
    use crate::schema::SchemaType;
    pub use ::inventory;

    /// Static-friendly mirror of `KindDescriptor`. Owns nothing —
    /// every field is `'static` so the value is const-constructible
    /// from `inventory::submit!`. `descriptors::all()` materializes
    /// the owned `KindDescriptor` form at iteration time.
    pub struct DescriptorEntry {
        pub name: &'static str,
        pub schema: &'static SchemaType,
    }

    inventory::collect!(DescriptorEntry);
}

/// Internal re-exports the `#[derive(Schema)]` and `#[derive(Kind)]`
/// macros point at so their output compiles in no_std + alloc
/// consumer crates without those consumers needing `extern crate
/// alloc;` or a direct `aether-data` dep at the site.
/// Not part of the public API; the macros are the only intended
/// callers.
#[doc(hidden)]
pub mod __derive_runtime {
    pub use crate::canonical;
    pub use crate::schema::{
        EnumVariant, KindLabels, LabelCell, LabelNode, NamedField, SchemaType, VariantLabel,
    };
    pub use alloc::borrow::Cow;
    pub use alloc::vec::Vec;
    use serde::de::DeserializeOwned;

    /// Cast-shape decode helper. Routes through `bytemuck::pod_read_unaligned`
    /// after a length check so the Kind derive can emit a uniform call
    /// without the user crate needing `bytemuck` in scope. `T` satisfies
    /// `AnyBitPattern` via the user's `#[derive(Pod)]`; the bound is
    /// enforced at the impl site rather than on `Kind` itself so non-
    /// cast kinds aren't poisoned by a trait they can't satisfy.
    #[must_use]
    pub fn decode_cast<T: bytemuck::AnyBitPattern>(bytes: &[u8]) -> Option<T> {
        if bytes.len() != size_of::<T>() {
            return None;
        }
        Some(bytemuck::pod_read_unaligned(bytes))
    }

    /// Slice-cast helper for batched cast-shape kinds. The native
    /// `#[handler]` macro emits this when a handler's `mail`
    /// parameter is `&[K]` rather than `K` — ADR-0019's `send_many`
    /// wire shape lets one envelope carry `count` contiguous Ks, so
    /// the handler sees the whole batch in one call. `bytes.len()`
    /// must be a multiple of `size_of::<T>()` and the slice must be
    /// suitably aligned; mis-shaped buffers return `None` and the
    /// dispatcher's miss path warn-logs at the chassis side.
    #[must_use]
    pub fn decode_cast_slice<T: bytemuck::AnyBitPattern>(bytes: &[u8]) -> Option<&[T]> {
        bytemuck::try_cast_slice(bytes).ok()
    }

    /// Postcard-shape decode helper. Sibling of `decode_cast` for
    /// schema-shaped kinds (anything carrying `Vec` / `String` /
    /// `Option` / a tagged enum). `T` satisfies `DeserializeOwned`
    /// via the user's `#[derive(Deserialize)]`; the bound lives on
    /// this helper rather than on `Kind` so cast kinds stay
    /// independent of `serde`.
    #[must_use]
    pub fn decode_postcard<T: DeserializeOwned>(bytes: &[u8]) -> Option<T> {
        postcard::from_bytes(bytes).ok()
    }

    /// Cast-shape encode helper. Mirror of `decode_cast`. Routes
    /// through `bytemuck::bytes_of` so the Kind derive emits a uniform
    /// call without the user crate needing `bytemuck` in scope. The
    /// `NoUninit` bound lives on the helper so non-cast kinds aren't
    /// poisoned by a trait their `#[repr(C)]`-less layout can't satisfy.
    pub fn encode_cast<T: bytemuck::NoUninit>(value: &T) -> Vec<u8> {
        bytemuck::bytes_of(value).to_vec()
    }

    /// Postcard-shape encode helper. Mirror of `decode_postcard`. The
    /// `Serialize` bound lives here, not on `Kind`, so cast kinds stay
    /// independent of `serde`.
    pub fn encode_postcard<T: serde::Serialize>(value: &T) -> Vec<u8> {
        postcard::to_allocvec(value).expect("postcard encode to Vec is infallible")
    }
}

/// Reason a decode failed. Encoding is infallible for the tiers we
/// support, so there is no corresponding `EncodeError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Byte length is not compatible with the target layout (wrong size
    /// for POD single, or not a multiple of element size for POD slice).
    SizeMismatch { expected: usize, actual: usize },
    /// Alignment of the input slice is incompatible with the target type.
    Alignment,
    /// Postcard decode failed for a structural payload.
    Postcard(postcard::Error),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SizeMismatch { expected, actual } => {
                write!(
                    f,
                    "data payload size mismatch: expected {expected}, got {actual}"
                )
            }
            Self::Alignment => f.write_str("data payload alignment mismatch"),
            Self::Postcard(e) => write!(f, "postcard decode failed: {e}"),
        }
    }
}

impl From<bytemuck::PodCastError> for DecodeError {
    fn from(err: bytemuck::PodCastError) -> Self {
        use bytemuck::PodCastError::{
            AlignmentMismatch, OutputSliceWouldHaveSlop, SizeMismatch,
            TargetAlignmentGreaterAndInputNotAligned,
        };
        match err {
            SizeMismatch | OutputSliceWouldHaveSlop => Self::SizeMismatch {
                expected: 0,
                actual: 0,
            },
            TargetAlignmentGreaterAndInputNotAligned | AlignmentMismatch => Self::Alignment,
        }
    }
}

impl From<postcard::Error> for DecodeError {
    fn from(err: postcard::Error) -> Self {
        Self::Postcard(err)
    }
}

/// Encode a single POD value to its native byte layout.
pub fn encode<T: Kind + bytemuck::NoUninit>(value: &T) -> Vec<u8> {
    bytemuck::bytes_of(value).to_vec()
}

/// Encode a slice of POD values as a contiguous byte buffer. The
/// substrate's `count` field is `items.len()` when using this helper.
pub fn encode_slice<T: Kind + bytemuck::NoUninit>(items: &[T]) -> Vec<u8> {
    bytemuck::cast_slice(items).to_vec()
}

/// Decode a single POD value. The input must match `size_of::<T>()`
/// exactly and meet `T`'s alignment requirement.
pub fn decode<T: Kind + bytemuck::AnyBitPattern + Copy>(bytes: &[u8]) -> Result<T, DecodeError> {
    if bytes.len() != size_of::<T>() {
        return Err(DecodeError::SizeMismatch {
            expected: size_of::<T>(),
            actual: bytes.len(),
        });
    }
    // `pod_read_unaligned` sidesteps the alignment requirement, which is
    // the common shape on wire buffers pulled out of a Vec<u8>.
    Ok(bytemuck::pod_read_unaligned(bytes))
}

/// Decode a POD slice in place. Zero-copy: the returned slice borrows
/// from `bytes`. Input length must be a multiple of `size_of::<T>()`
/// and aligned for `T`.
pub fn decode_slice<T: Kind + bytemuck::AnyBitPattern>(bytes: &[u8]) -> Result<&[T], DecodeError> {
    bytemuck::try_cast_slice(bytes).map_err(DecodeError::from)
}

/// Encode a structural value via postcard.
///
/// # Panics
/// Panics if postcard encoding of `value` fails — fail-fast per ADR-0063:
/// `postcard::to_allocvec` into a growable `Vec` cannot fail for the
/// `Serialize` types this is used with, so a failure indicates the
/// caller passed a type whose serializer is observably broken.
pub fn encode_struct<T: Kind + serde::Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("postcard encode to Vec is infallible")
}

/// Decode a structural value via postcard. Returns owned `T`.
pub fn decode_struct<T: Kind + DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    postcard::from_bytes(bytes).map_err(DecodeError::from)
}

/// Marker payload for signal-only kinds with no bytes on the wire.
/// Implementors need nothing but a `Kind` impl; use `encode_empty` on
/// the sender side and ignore the payload on the receiver side.
#[must_use]
pub fn encode_empty<T: Kind>() -> Vec<u8> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use bytemuck::{Pod, Zeroable};
    use serde::{Deserialize, Serialize};

    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, Pod, Zeroable)]
    struct TestPod {
        a: u32,
        b: f32,
    }
    // Tests exercise encode/decode, not routing, so the exact ID
    // values don't matter — they only need to be stable and distinct
    // across the four test kinds. Hand-picked sentinels are clearer
    // than hashing a name through a domain-mismatched helper.
    impl Kind for TestPod {
        const NAME: &'static str = "test.pod";
        const ID: KindId = KindId(0xDEAD_BEEF_0000_0001);

        fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
            __derive_runtime::decode_cast::<Self>(bytes)
        }

        fn encode_into_bytes(&self) -> Vec<u8> {
            __derive_runtime::encode_cast::<Self>(self)
        }
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, Pod, Zeroable)]
    struct Vertex {
        x: f32,
        y: f32,
    }
    impl Kind for Vertex {
        const NAME: &'static str = "test.vertex";
        const ID: KindId = KindId(0xDEAD_BEEF_0000_0002);
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct TestStruct {
        tag: u32,
        label: String,
    }
    impl Kind for TestStruct {
        const NAME: &'static str = "test.struct";
        const ID: KindId = KindId(0xDEAD_BEEF_0000_0003);

        fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
            __derive_runtime::decode_postcard::<Self>(bytes)
        }

        fn encode_into_bytes(&self) -> Vec<u8> {
            __derive_runtime::encode_postcard::<Self>(self)
        }
    }

    #[test]
    fn pod_roundtrip_single() {
        let v = TestPod { a: 42, b: 1.5 };
        let bytes = encode(&v);
        assert_eq!(bytes.len(), 8);
        let back: TestPod = decode(&bytes).expect("test setup: cast round-trip decodes");
        assert_eq!(back, v);
    }

    #[test]
    fn pod_roundtrip_slice_is_zero_copy() {
        let verts = [Vertex { x: 0.0, y: 0.5 }, Vertex { x: 1.0, y: -0.5 }];
        let bytes = encode_slice(&verts);
        assert_eq!(bytes.len(), 16);
        let decoded: &[Vertex] =
            decode_slice(&bytes).expect("test setup: aligned slice decodes zero-copy");
        assert_eq!(decoded, &verts);
    }

    #[test]
    fn pod_decode_size_mismatch_rejected() {
        let bytes = [0u8; 7]; // TestPod is 8 bytes
        let err =
            decode::<TestPod>(&bytes).expect_err("test setup: short buffer must fail size check");
        assert!(matches!(
            err,
            DecodeError::SizeMismatch {
                expected: 8,
                actual: 7
            }
        ));
    }

    #[test]
    fn struct_roundtrip() {
        let v = TestStruct {
            tag: 7,
            label: String::from("hello"),
        };
        let bytes = encode_struct(&v);
        let back: TestStruct =
            decode_struct(&bytes).expect("test setup: postcard round-trip decodes TestStruct");
        assert_eq!(back, v);
    }

    #[test]
    fn struct_decode_malformed_rejected() {
        let err = decode_struct::<TestStruct>(&[0x00])
            .expect_err("test setup: single-byte buffer must fail postcard decode");
        assert!(matches!(err, DecodeError::Postcard(_)));
    }

    #[test]
    fn mailbox_id_is_deterministic_and_name_specific() {
        let a = mailbox_id_from_name("hub.claude.broadcast");
        let b = mailbox_id_from_name("hub.claude.broadcast");
        let c = mailbox_id_from_name("render");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            mailbox_id_from_name(""),
            MailboxId(with_tag(
                Tag::Mailbox,
                fnv1a_64_prefixed(MAILBOX_DOMAIN, &[]),
            )),
        );
        assert_eq!(tagged_id::tag_of(a.0), Some(Tag::Mailbox));
        assert_ne!(mailbox_id_from_name(""), MailboxId(0xcbf2_9ce4_8422_2325));
    }

    #[test]
    fn ref_handle_pulls_kind_id_from_type_param() {
        let r: Ref<TestStruct> = Ref::handle(42);
        match r {
            Ref::Handle { id, kind_id } => {
                assert_eq!(id, 42);
                assert_eq!(kind_id, TestStruct::ID.0);
            }
            Ref::Inline(_) => panic!("expected Handle variant"),
        }
    }

    #[test]
    fn ref_inline_wraps_value() {
        let v = TestStruct {
            tag: 7,
            label: String::from("hi"),
        };
        let r = Ref::inline(v.clone());
        match r {
            Ref::Inline(inner) => assert_eq!(inner, v),
            Ref::Handle { .. } => panic!("expected Inline variant"),
        }
    }

    #[test]
    fn ref_predicates_and_handle_id() {
        let inline: Ref<TestStruct> = Ref::Inline(TestStruct {
            tag: 1,
            label: String::from("a"),
        });
        let handle: Ref<TestStruct> = Ref::handle(99);
        assert!(inline.is_inline());
        assert!(!inline.is_handle());
        assert_eq!(inline.handle_id(), None);
        assert!(!handle.is_inline());
        assert!(handle.is_handle());
        assert_eq!(handle.handle_id(), Some(99));
    }

    #[test]
    fn ref_inline_postcard_roundtrip() {
        let v = TestStruct {
            tag: 42,
            label: String::from("hello"),
        };
        let r = Ref::Inline(v);
        let bytes = postcard::to_allocvec(&r).expect("test setup: postcard encodes Inline Ref");
        let back: Ref<TestStruct> =
            postcard::from_bytes(&bytes).expect("test setup: postcard decodes Inline Ref");
        assert_eq!(back, r);
    }

    #[test]
    fn ref_handle_postcard_roundtrip() {
        let r: Ref<TestStruct> = Ref::Handle {
            id: 0xdead_beef_cafe_babe,
            kind_id: 0x1234_5678_9abc_def0,
        };
        let bytes = postcard::to_allocvec(&r).expect("test setup: postcard encodes Handle Ref");
        let back: Ref<TestStruct> =
            postcard::from_bytes(&bytes).expect("test setup: postcard decodes Handle Ref");
        assert_eq!(back, r);
    }

    #[test]
    fn ref_inline_and_handle_have_distinct_wire_discriminants() {
        let inline: Ref<TestStruct> = Ref::Inline(TestStruct {
            tag: 1,
            label: String::from("x"),
        });
        let handle: Ref<TestStruct> = Ref::Handle { id: 1, kind_id: 1 };
        let inline_bytes =
            postcard::to_allocvec(&inline).expect("test setup: postcard encodes Inline Ref");
        let handle_bytes =
            postcard::to_allocvec(&handle).expect("test setup: postcard encodes Handle Ref");
        assert_eq!(inline_bytes[0], 0, "Inline discriminant must be 0");
        assert_eq!(handle_bytes[0], 1, "Handle discriminant must be 1");
    }

    #[test]
    fn ref_is_cast_ineligible() {
        const { assert!(!<Ref<TestStruct> as CastEligible>::ELIGIBLE) };
    }

    #[test]
    fn ref_inline_cast_kind_body_is_cast_image() {
        // ADR-0100: a cast kind's inline body is the raw cast image,
        // not a varint-postcard image. `TestPod` carries a `u32` field
        // (`a`), whose cast bytes differ from postcard's varint.
        let v = TestPod {
            a: 0x1234_5678,
            b: 1.5,
        };
        let r: Ref<TestPod> = Ref::Inline(v);
        let bytes =
            postcard::to_allocvec(&r).expect("test setup: postcard encodes Inline Ref<TestPod>");
        assert_eq!(bytes[0], 0, "Inline discriminant");
        assert_eq!(bytes[1], 8, "varint length prefix of the 8-byte cast image");
        assert_eq!(
            &bytes[2..],
            bytemuck::bytes_of(&v),
            "inline body is the cast image, not a postcard varint image"
        );
        let back: Ref<TestPod> =
            postcard::from_bytes(&bytes).expect("test setup: postcard decodes Inline Ref<TestPod>");
        assert_eq!(back, r);
    }

    #[test]
    fn ref_handle_cast_kind_roundtrip() {
        let r: Ref<TestPod> = Ref::Handle {
            id: 7,
            kind_id: TestPod::ID.0,
        };
        let bytes =
            postcard::to_allocvec(&r).expect("test setup: postcard encodes Handle Ref<TestPod>");
        assert_eq!(bytes[0], 1, "Handle discriminant");
        let back: Ref<TestPod> =
            postcard::from_bytes(&bytes).expect("test setup: postcard decodes Handle Ref<TestPod>");
        assert_eq!(back, r);
    }

    #[test]
    fn mailbox_and_kind_domains_disjoin_identical_payloads() {
        let payload = b"collision.test";
        let as_mailbox = fnv1a_64_prefixed(MAILBOX_DOMAIN, payload);
        let as_kind = fnv1a_64_prefixed(KIND_DOMAIN, payload);
        assert_ne!(as_mailbox, as_kind);
    }
}
