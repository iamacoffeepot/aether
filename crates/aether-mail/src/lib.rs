//! aether-mail: shared machinery for the mail typing system described in
//! ADR-0005. No concrete kinds live here — each actor owns its kinds in
//! its own crate (substrate in `aether-kinds`, components in
//! `{component}-mail` crates as they define their own).
//!
//! Two payload tiers:
//!   - POD: `#[repr(C)]` types implementing `bytemuck::NoUninit` /
//!     `AnyBitPattern`. Encoded as their native byte layout; decoded
//!     zero-copy to `&T` or `&[T]`. Used for vertex streams, fixed-layout
//!     structs, anything where throughput or zero-copy matters.
//!   - Structural: `serde::Serialize + DeserializeOwned` types. Encoded
//!     with postcard (Rust-native, varint-compact, no_std-friendly).
//!     Used for small control messages with Option/Vec/enum shape.
//!
//! A type picks one tier — not both — as part of its contract.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

/// Identifies a mail kind by a stable, namespaced string name (e.g.
/// `"aether.tick"`, `"hello.npc_health"`) and a `u64` id derived from
/// that name plus the kind's canonical schema bytes (ADR-0030 Phase 2,
/// ADR-0032). Both sides of the FFI compute the id the same way — the
/// substrate from the deserialized schema, the guest from the compile-
/// time const — so routing stays in lockstep without a host-fn resolve.
///
/// `IS_INPUT` marks the kind as a substrate-published input stream
/// (`Tick`, `Key`, `MouseMove`, `MouseButton` — ADR-0021). Defaults
/// to `false`; `#[handlers]` auto-subscribes a component's mailbox to
/// every `K::IS_INPUT` handler kind before the user's `init` body
/// runs (ADR-0033 phase 3), so components writing
/// `#[handler] fn on_tick(..., tick: Tick)` don't need to send
/// `subscribe_input` themselves. Non-input kinds never touch this —
/// leave the default alone.
pub trait Kind {
    const NAME: &'static str;
    const ID: u64;
    const IS_INPUT: bool = false;
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

/// Deterministic 64-bit hash of a mailbox name (ADR-0029). Both the
/// substrate registry and the guest SDK compute mailbox ids from names
/// this way, which is how ids end up meaningful across processes and
/// sessions without needing a host-fn resolve.
///
/// FNV-1a 64 is chosen for: (a) no dependencies — the algorithm is
/// ~8 lines; (b) determinism across builds, platforms, and rust
/// versions, without having to pin a third-party hasher; (c) the
/// distribution is more than good enough at mailbox cardinality.
/// At 64 bits the birthday bound is far past realistic mailbox
/// counts — see ADR-0029 "Consequences" for the table.
///
/// The returned `u64` is the raw id wrapped into
/// `aether_substrate::mail::MailboxId` on the substrate side; guests
/// use it directly as the `recipient` on `send_mail`. `0` is reserved
/// as the no-sender sentinel — callers should reject on the astronomical
/// chance of a collision with it.
pub const fn mailbox_id_from_name(name: &str) -> u64 {
    fnv1a_64_prefixed(MAILBOX_DOMAIN, name.as_bytes())
}

/// Domain tag prefixed to every mailbox-name hash so the `MailboxId`
/// space is disjoint from `Kind::ID`. Both ids are 64-bit FNV-1a
/// outputs; without a prefix the spaces overlap and a future bug that
/// feeds a mailbox id into a kind-id slot (or vice versa) would
/// misattribute silently. Prefixing makes the mis-attribution
/// statistically impossible rather than relying on positional
/// discipline at every call site.
pub const MAILBOX_DOMAIN: &[u8] = b"mailbox:";

/// Domain tag prefixed to every kind-id hash. See `MAILBOX_DOMAIN` for
/// the rationale; the derive macro and `kind_id_from_parts` both
/// prepend this before the canonical schema bytes.
pub const KIND_DOMAIN: &[u8] = b"kind:";

/// FNV-1a 64 over a byte slice (ADR-0032). Retained for the few
/// call sites that hash neither a mailbox name nor a kind schema.
/// New callers should prefer `fnv1a_64_prefixed` with an explicit
/// domain so the output id space doesn't collide with an existing
/// domain by accident.
pub const fn fnv1a_64_bytes(bytes: &[u8]) -> u64 {
    fnv1a_64_prefixed(&[], bytes)
}

/// FNV-1a 64 over `prefix ++ payload` without allocating. Equivalent
/// to `fnv1a_64_bytes(&[prefix, payload].concat())` but `const`-safe.
/// Used by `mailbox_id_from_name` (prefix `MAILBOX_DOMAIN`) and by
/// `#[derive(Kind)]` through the macro (prefix `KIND_DOMAIN`).
pub const fn fnv1a_64_prefixed(prefix: &[u8], payload: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut i = 0;
    while i < prefix.len() {
        hash ^= prefix[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }
    let mut i = 0;
    while i < payload.len() {
        hash ^= payload[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }
    hash
}

/// Re-exported derive macros from `aether-mail-derive`. Behind the
/// `derive` feature so `cargo build` on a guest that hand-writes
/// `impl Kind` doesn't pay the proc-macro compile cost. The
/// `#[handlers]` / `#[handler]` / `#[fallback]` attribute macros
/// (ADR-0033) ride in the same crate because adding a second proc-
/// macro crate would double consumer compile cost for no separation
/// gain — both derives and attributes expand into the same runtime
/// surface.
#[cfg(feature = "derive")]
pub use aether_mail_derive::{Kind, Schema, fallback, handler, handlers};

/// ADR-0019 schema producer. The substrate (and tooling that builds
/// hub descriptors) reads `<T as Schema>::SCHEMA` — a compile-time
/// const — to learn how a kind's payload is laid out. Wasm guests
/// typically don't need this, so the trait sits behind the
/// `descriptors` feature.
///
/// Blanket impls cover the leaf types in the schema vocabulary
/// (primitives, `String`, `[u8]`-shaped `Vec`s, fixed arrays,
/// `Option`, generic `Vec`). User structs reach the trait via
/// `#[derive(Schema)]`.
pub use schema::Schema;

/// Internal re-exports the `#[derive(Schema)]` and `#[derive(Kind)]`
/// macros point at so their output compiles in no_std + alloc
/// consumer crates without those consumers needing `extern crate
/// alloc;` or a direct `aether-hub-protocol` dep at the site.
/// Not part of the public API; the macros are the only intended
/// callers.
#[doc(hidden)]
pub mod __derive_runtime {
    pub use aether_hub_protocol::{
        EnumVariant, KindLabels, LabelCell, LabelNode, NamedField, SchemaType, VariantLabel,
        canonical,
    };
    pub use alloc::borrow::Cow;
}

mod schema {
    use alloc::string::String;
    use alloc::vec::Vec;

    use aether_hub_protocol::{LabelCell, LabelNode, Primitive, SchemaCell, SchemaType};

    /// Produces the `SchemaType` describing how this type lays out as
    /// a mail payload. Implemented by `#[derive(Schema)]` on user
    /// structs, and by blanket impl on the schema-vocabulary leaves.
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
                    "mail payload size mismatch: expected {expected}, got {actual}"
                )
            }
            DecodeError::Alignment => f.write_str("mail payload alignment mismatch"),
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
    impl Kind for TestPod {
        const NAME: &'static str = "test.pod";
        // Tests exercise encode/decode, not routing, so the exact ID
        // doesn't matter. `mailbox_id_from_name` gives us a stable
        // derivation without pulling Schema into tests that don't
        // use it.
        const ID: u64 = mailbox_id_from_name(Self::NAME);
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, Pod, Zeroable)]
    struct Vertex {
        x: f32,
        y: f32,
    }
    impl Kind for Vertex {
        const NAME: &'static str = "test.vertex";
        const ID: u64 = mailbox_id_from_name(Self::NAME);
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct TestStruct {
        tag: u32,
        label: alloc::string::String,
    }
    impl Kind for TestStruct {
        const NAME: &'static str = "test.struct";
        const ID: u64 = mailbox_id_from_name(Self::NAME);
    }

    struct Signal;
    impl Kind for Signal {
        const NAME: &'static str = "test.signal";
        const ID: u64 = mailbox_id_from_name(Self::NAME);
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
        // Alignment-preserving slice: we built `bytes` from `&[Vertex]`
        // so it's aligned; use try_cast_slice for the real test.
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
        // A deliberately truncated payload — postcard will reject.
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
        // Empty name hashes the domain prefix alone. Pins the
        // prefixing convention — a regression that drops the prefix
        // would collapse to `0xcbf29ce484222325` (the raw FNV offset
        // basis) here.
        assert_eq!(
            mailbox_id_from_name(""),
            fnv1a_64_prefixed(MAILBOX_DOMAIN, &[]),
        );
        assert_ne!(mailbox_id_from_name(""), 0xcbf29ce484222325);
    }

    #[test]
    fn mailbox_and_kind_domains_disjoin_identical_payloads() {
        // The whole point of prefixing: even if a mailbox name and a
        // kind's canonical bytes happened to be the same byte string,
        // the resulting ids differ.
        let payload = b"collision.test";
        let as_mailbox = fnv1a_64_prefixed(MAILBOX_DOMAIN, payload);
        let as_kind = fnv1a_64_prefixed(KIND_DOMAIN, payload);
        assert_ne!(as_mailbox, as_kind);
    }
}
