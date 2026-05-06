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
//! - **Kind / Schema / CastEligible traits** (ADR-0030): the binding
//!   between a Rust type and its wire form.
//! - **`Ref<K>`** (ADR-0045): typed handle reference for fields that
//!   may inline a value or carry a handle into the substrate's store.
//! - **Encode / decode helpers**: the `encode` / `decode` family for
//!   POD and postcard kinds.
//! - **`__inventory`** (issue #243): native-only auto-collection of
//!   `#[derive(Kind)]` types into the substrate's descriptor list.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

pub mod canonical;
pub mod hash;
pub mod ids;
pub mod mail;
pub mod schema;
pub mod tag_bits;
pub mod tagged_id;
pub mod wire_id;
pub use hash::{
    KIND_DOMAIN, MAILBOX_DOMAIN, TYPE_DOMAIN, fnv1a_64_bytes, fnv1a_64_prefixed,
    mailbox_id_from_name,
};
pub use ids::{HandleId, KindId, MailboxId, tag_for_type_id, type_name_for_type_id};
pub use mail::{ReplyTarget, ReplyTo};
pub use schema::*;
pub use tagged_id::{Tag, with_tag};
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
    Kind, Schema, Singleton, actor, bridge, capability, fallback, handler, local,
};

/// Identifies a mail kind by a stable, namespaced string name (e.g.
/// `"aether.tick"`, `"hello.npc_health"`) and a `u64` id derived from
/// that name plus the kind's canonical schema bytes (ADR-0030 Phase 2,
/// ADR-0032). Both sides of the FFI compute the id the same way — the
/// substrate from the deserialized schema, the guest from the compile-
/// time const — so routing stays in lockstep without a host-fn resolve.
///
/// `IS_STREAM` marks the kind as a substrate-published input stream
/// (`Tick`, `Key`, `MouseMove`, `MouseButton` — ADR-0021). Defaults
/// to `false`; `#[actor]` auto-subscribes a component's mailbox to
/// every `K::IS_STREAM` handler kind before the user's `init` body
/// runs (ADR-0033 phase 3), so components writing
/// `#[handler] fn on_tick(..., tick: Tick)` don't need to send
/// `subscribe_input` themselves. Non-input kinds never touch this —
/// leave the default alone.
pub trait Kind {
    const NAME: &'static str;
    const ID: KindId;
    /// `true` when the kind is a substrate-published event stream
    /// (Tick, Key, MouseMove, etc.) that components subscribe to via
    /// the per-kind subscriber set. Drives `auto_subscribe_inputs` on
    /// the substrate side: handlers whose kind is a stream get wired
    /// into the subscriber set the moment the component is loaded.
    /// Set by `#[kind(name = "...", stream)]` on the type declaration.
    const IS_STREAM: bool = false;

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
impl CastEligible for alloc::string::String {
    const ELIGIBLE: bool = false;
}
impl<T> CastEligible for alloc::vec::Vec<T> {
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
impl<K, V> CastEligible for alloc::collections::BTreeMap<K, V> {
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    pub const fn handle(id: u64) -> Self {
        Ref::Handle {
            id,
            kind_id: K::ID.0,
        }
    }
}

impl<K> Ref<K> {
    /// Wrap an owned value as `Ref::Inline`. Convenience for call
    /// sites that have the value but want the field shape.
    pub const fn inline(value: K) -> Self {
        Ref::Inline(value)
    }

    /// Returns `true` for `Ref::Inline`, `false` for
    /// `Ref::Handle`. Cheap predicate for call sites that branch
    /// on resolution state.
    pub const fn is_inline(&self) -> bool {
        matches!(self, Ref::Inline(_))
    }

    /// Returns `true` for `Ref::Handle`, `false` for
    /// `Ref::Inline`.
    pub const fn is_handle(&self) -> bool {
        matches!(self, Ref::Handle { .. })
    }

    /// The wire `id` if this is a `Ref::Handle`, `None` for
    /// inline. `kind_id` is recoverable via `<K as Kind>::ID` so
    /// no separate accessor is provided.
    pub const fn handle_id(&self) -> Option<u64> {
        match self {
            Ref::Handle { id, .. } => Some(*id),
            Ref::Inline(_) => None,
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
    use crate::{HandleId, KindId, MailboxId, Schema};

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
            len: N as u32,
        };
        const LABEL: Option<&'static str> = None;
        const LABEL_NODE: LabelNode = LabelNode::Array(LabelCell::Static(&T::LABEL_NODE));
    }

    // ADR-0064 / ADR-0065 typed-id newtypes.
    impl Schema for MailboxId {
        const SCHEMA: SchemaType = SchemaType::TypeId(MailboxId::TYPE_ID);
        const LABEL: Option<&'static str> = Some(MailboxId::TYPE_NAME);
        const LABEL_NODE: LabelNode = LabelNode::Anonymous;
    }

    impl Schema for KindId {
        const SCHEMA: SchemaType = SchemaType::TypeId(KindId::TYPE_ID);
        const LABEL: Option<&'static str> = Some(KindId::TYPE_NAME);
        const LABEL_NODE: LabelNode = LabelNode::Anonymous;
    }

    impl Schema for HandleId {
        const SCHEMA: SchemaType = SchemaType::TypeId(HandleId::TYPE_ID);
        const LABEL: Option<&'static str> = Some(HandleId::TYPE_NAME);
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
    impl<K: Schema + Ord + 'static, V: Schema + 'static> Schema for alloc::collections::BTreeMap<K, V> {
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
    /// every field is `'static` (or a `bool`) so the value is
    /// const-constructible from `inventory::submit!`.
    /// `descriptors::all()` materializes the owned `KindDescriptor`
    /// form at iteration time. `is_stream` carries
    /// `<K as Kind>::IS_STREAM` (ADR-0068) so the native descriptor
    /// list agrees with the wasm `aether.kinds` v0x03 trailing byte
    /// the substrate reads from guest binaries.
    pub struct DescriptorEntry {
        pub name: &'static str,
        pub schema: &'static SchemaType,
        pub is_stream: bool,
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

    /// Cast-shape decode helper. Routes through `bytemuck::pod_read_unaligned`
    /// after a length check so the Kind derive can emit a uniform call
    /// without the user crate needing `bytemuck` in scope. `T` satisfies
    /// `AnyBitPattern` via the user's `#[derive(Pod)]`; the bound is
    /// enforced at the impl site rather than on `Kind` itself so non-
    /// cast kinds aren't poisoned by a trait they can't satisfy.
    pub fn decode_cast<T: bytemuck::AnyBitPattern>(bytes: &[u8]) -> Option<T> {
        if bytes.len() != core::mem::size_of::<T>() {
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
    pub fn decode_cast_slice<T: bytemuck::AnyBitPattern>(bytes: &[u8]) -> Option<&[T]> {
        bytemuck::try_cast_slice(bytes).ok()
    }

    /// Postcard-shape decode helper. Sibling of `decode_cast` for
    /// schema-shaped kinds (anything carrying `Vec` / `String` /
    /// `Option` / a tagged enum). `T` satisfies `DeserializeOwned`
    /// via the user's `#[derive(Deserialize)]`; the bound lives on
    /// this helper rather than on `Kind` so cast kinds stay
    /// independent of `serde`.
    pub fn decode_postcard<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
        postcard::from_bytes(bytes).ok()
    }

    /// Cast-shape encode helper. Mirror of `decode_cast`. Routes
    /// through `bytemuck::bytes_of` so the Kind derive emits a uniform
    /// call without the user crate needing `bytemuck` in scope. The
    /// `NoUninit` bound lives on the helper so non-cast kinds aren't
    /// poisoned by a trait their `#[repr(C)]`-less layout can't satisfy.
    pub fn encode_cast<T: bytemuck::NoUninit>(value: &T) -> alloc::vec::Vec<u8> {
        ::bytemuck::bytes_of(value).to_vec()
    }

    /// Postcard-shape encode helper. Mirror of `decode_postcard`. The
    /// `Serialize` bound lives here, not on `Kind`, so cast kinds stay
    /// independent of `serde`.
    pub fn encode_postcard<T: serde::Serialize>(value: &T) -> alloc::vec::Vec<u8> {
        ::postcard::to_allocvec(value).expect("postcard encode to Vec is infallible")
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
            DecodeError::SizeMismatch { expected, actual } => {
                write!(
                    f,
                    "data payload size mismatch: expected {expected}, got {actual}"
                )
            }
            DecodeError::Alignment => f.write_str("data payload alignment mismatch"),
            DecodeError::Postcard(e) => write!(f, "postcard decode failed: {e}"),
        }
    }
}

impl From<bytemuck::PodCastError> for DecodeError {
    fn from(err: bytemuck::PodCastError) -> Self {
        use bytemuck::PodCastError::*;
        match err {
            SizeMismatch | OutputSliceWouldHaveSlop => DecodeError::SizeMismatch {
                expected: 0,
                actual: 0,
            },
            TargetAlignmentGreaterAndInputNotAligned => DecodeError::Alignment,
            AlignmentMismatch => DecodeError::Alignment,
        }
    }
}

impl From<postcard::Error> for DecodeError {
    fn from(err: postcard::Error) -> Self {
        DecodeError::Postcard(err)
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
    if bytes.len() != core::mem::size_of::<T>() {
        return Err(DecodeError::SizeMismatch {
            expected: core::mem::size_of::<T>(),
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
pub fn encode_struct<T: Kind + serde::Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).expect("postcard encode to Vec is infallible")
}

/// Decode a structural value via postcard. Returns owned `T`.
pub fn decode_struct<T: Kind + serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, DecodeError> {
    postcard::from_bytes(bytes).map_err(DecodeError::from)
}

/// Marker payload for signal-only kinds with no bytes on the wire.
/// Implementors need nothing but a `Kind` impl; use `encode_empty` on
/// the sender side and ignore the payload on the receiver side.
pub fn encode_empty<T: Kind>() -> Vec<u8> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
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
        label: alloc::string::String,
    }
    impl Kind for TestStruct {
        const NAME: &'static str = "test.struct";
        const ID: KindId = KindId(0xDEAD_BEEF_0000_0003);
    }

    struct Signal;
    impl Kind for Signal {
        const NAME: &'static str = "test.signal";
        const ID: KindId = KindId(0xDEAD_BEEF_0000_0004);
    }

    #[test]
    fn pod_roundtrip_single() {
        let v = TestPod { a: 42, b: 1.5 };
        let bytes = encode(&v);
        assert_eq!(bytes.len(), 8);
        let back: TestPod = decode(&bytes).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn pod_roundtrip_slice_is_zero_copy() {
        let verts = [Vertex { x: 0.0, y: 0.5 }, Vertex { x: 1.0, y: -0.5 }];
        let bytes = encode_slice(&verts);
        assert_eq!(bytes.len(), 16);
        let decoded: &[Vertex] = decode_slice(&bytes).unwrap();
        assert_eq!(decoded, &verts);
    }

    #[test]
    fn pod_decode_size_mismatch_rejected() {
        let bytes = [0u8; 7]; // TestPod is 8 bytes
        let err = decode::<TestPod>(&bytes).unwrap_err();
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
            label: alloc::string::String::from("hello"),
        };
        let bytes = encode_struct(&v);
        let back: TestStruct = decode_struct(&bytes).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn struct_decode_malformed_rejected() {
        let err = decode_struct::<TestStruct>(&[0x00]).unwrap_err();
        assert!(matches!(err, DecodeError::Postcard(_)));
    }

    #[test]
    fn empty_kind_encodes_to_zero_bytes() {
        assert!(encode_empty::<Signal>().is_empty());
        assert_eq!(Signal::NAME, "test.signal");
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
        assert_ne!(mailbox_id_from_name(""), MailboxId(0xcbf29ce484222325));
    }

    #[test]
    fn ref_handle_pulls_kind_id_from_type_param() {
        let r: Ref<TestStruct> = Ref::handle(42);
        match r {
            Ref::Handle { id, kind_id } => {
                assert_eq!(id, 42);
                assert_eq!(kind_id, TestStruct::ID.0);
            }
            _ => panic!("expected Handle variant"),
        }
    }

    #[test]
    fn ref_inline_wraps_value() {
        let v = TestStruct {
            tag: 7,
            label: alloc::string::String::from("hi"),
        };
        let r = Ref::inline(v.clone());
        match r {
            Ref::Inline(inner) => assert_eq!(inner, v),
            _ => panic!("expected Inline variant"),
        }
    }

    #[test]
    fn ref_predicates_and_handle_id() {
        let inline: Ref<TestStruct> = Ref::Inline(TestStruct {
            tag: 1,
            label: alloc::string::String::from("a"),
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
            label: alloc::string::String::from("hello"),
        };
        let r = Ref::Inline(v);
        let bytes = postcard::to_allocvec(&r).unwrap();
        let back: Ref<TestStruct> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn ref_handle_postcard_roundtrip() {
        let r: Ref<TestStruct> = Ref::Handle {
            id: 0xdead_beef_cafe_babe,
            kind_id: 0x1234_5678_9abc_def0,
        };
        let bytes = postcard::to_allocvec(&r).unwrap();
        let back: Ref<TestStruct> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn ref_inline_and_handle_have_distinct_wire_discriminants() {
        let inline: Ref<TestStruct> = Ref::Inline(TestStruct {
            tag: 1,
            label: alloc::string::String::from("x"),
        });
        let handle: Ref<TestStruct> = Ref::Handle { id: 1, kind_id: 1 };
        let inline_bytes = postcard::to_allocvec(&inline).unwrap();
        let handle_bytes = postcard::to_allocvec(&handle).unwrap();
        assert_eq!(inline_bytes[0], 0, "Inline discriminant must be 0");
        assert_eq!(handle_bytes[0], 1, "Handle discriminant must be 1");
    }

    #[test]
    fn ref_is_cast_ineligible() {
        const { assert!(!<Ref<TestStruct> as CastEligible>::ELIGIBLE) };
    }

    #[test]
    fn mailbox_and_kind_domains_disjoin_identical_payloads() {
        let payload = b"collision.test";
        let as_mailbox = fnv1a_64_prefixed(MAILBOX_DOMAIN, payload);
        let as_kind = fnv1a_64_prefixed(KIND_DOMAIN, payload);
        assert_ne!(as_mailbox, as_kind);
    }
}
