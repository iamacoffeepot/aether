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
/// `"aether.tick"`, `"hello.npc_health"`). The name is resolved to a
/// runtime `u32` id by the substrate's kind registry at init; see
/// ADR-0005 for the resolution flow.
///
/// `IS_INPUT` marks the kind as a substrate-published input stream
/// (`Tick`, `Key`, `MouseMove`, `MouseButton` — ADR-0021). Defaults
/// to `false`; the guest SDK auto-subscribes a component's mailbox
/// to every input kind in its typelist before the first `receive`
/// fires, so components declaring `type Kinds = (Tick, ...)` don't
/// need to send `subscribe_input` themselves. Non-input kinds never
/// touch this — leave the default alone.
pub trait Kind {
    const NAME: &'static str;
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

/// Re-exported derive macros from `aether-mail-derive`. Behind the
/// `derive` feature so `cargo build` on a guest that hand-writes
/// `impl Kind` doesn't pay the proc-macro compile cost.
#[cfg(feature = "derive")]
pub use aether_mail_derive::{Kind, Schema};

/// ADR-0019 schema producer. The substrate (and tooling that builds
/// hub descriptors) calls `T::schema()` to learn how a kind's payload
/// is laid out. Wasm guests typically don't need this — they send and
/// receive bytes — so the trait sits behind the `descriptors` feature.
///
/// Blanket impls cover the leaf types in the schema vocabulary
/// (primitives, `String`, `[u8]`-shaped `Vec`s, fixed arrays,
/// `Option`, generic `Vec`). User structs reach the trait via
/// `#[derive(Schema)]`.
#[cfg(feature = "descriptors")]
pub use schema::Schema;

/// Internal helpers the `#[derive(Schema)]` macro uses to construct
/// `String` and `Vec` values without forcing every consumer crate to
/// `extern crate alloc;`. Not part of the public API; the macro is the
/// only intended caller.
#[cfg(feature = "descriptors")]
#[doc(hidden)]
pub mod __derive_runtime {
    use alloc::string::String;
    use alloc::vec::Vec;

    pub fn string_from(s: &str) -> String {
        String::from(s)
    }

    pub fn vec_from<T, const N: usize>(arr: [T; N]) -> Vec<T> {
        Vec::from(arr)
    }
}

#[cfg(feature = "descriptors")]
mod schema {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;

    use aether_hub_protocol::{Primitive, SchemaType};

    /// Produces the `SchemaType` describing how this type lays out as
    /// a mail payload. Implemented by `#[derive(Schema)]` on user
    /// structs, and by blanket impl on the schema-vocabulary leaves.
    pub trait Schema {
        fn schema() -> SchemaType;
    }

    macro_rules! scalar {
        ($t:ty, $p:ident) => {
            impl Schema for $t {
                fn schema() -> SchemaType {
                    SchemaType::Scalar(Primitive::$p)
                }
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
        fn schema() -> SchemaType {
            SchemaType::Bool
        }
    }

    impl Schema for String {
        fn schema() -> SchemaType {
            SchemaType::String
        }
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
    impl<T: Schema> Schema for Vec<T> {
        fn schema() -> SchemaType {
            SchemaType::Vec(Box::new(T::schema()))
        }
    }

    impl<T: Schema> Schema for Option<T> {
        fn schema() -> SchemaType {
            SchemaType::Option(Box::new(T::schema()))
        }
    }

    impl<T: Schema, const N: usize> Schema for [T; N] {
        fn schema() -> SchemaType {
            SchemaType::Array {
                element: Box::new(T::schema()),
                len: N as u32,
            }
        }
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
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, Pod, Zeroable)]
    struct Vertex {
        x: f32,
        y: f32,
    }
    impl Kind for Vertex {
        const NAME: &'static str = "test.vertex";
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct TestStruct {
        tag: u32,
        label: alloc::string::String,
    }
    impl Kind for TestStruct {
        const NAME: &'static str = "test.struct";
    }

    struct Signal;
    impl Kind for Signal {
        const NAME: &'static str = "test.signal";
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
}
