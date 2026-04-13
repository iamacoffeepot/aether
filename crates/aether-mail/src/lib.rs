// aether-mail: shared machinery for the mail typing system described in
// ADR-0005. No concrete kinds live here — each actor owns its kinds in
// its own crate (substrate in `aether-substrate-mail`, components in
// `{component}-mail` crates as they define their own).
//
// Two payload tiers:
//   - POD: #[repr(C)] types implementing `bytemuck::NoUninit` /
//     `AnyBitPattern`. Encoded as their native byte layout; decoded
//     zero-copy to `&T` or `&[T]`. Used for vertex streams, fixed-layout
//     structs, anything where throughput or zero-copy matters.
//   - Structural: `serde::Serialize + DeserializeOwned` types. Encoded
//     with postcard (Rust-native, varint-compact, no_std-friendly).
//     Used for small control messages with Option/Vec/enum shape.
//
// A type picks one tier — not both — as part of its contract.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

/// Identifies a mail kind by a stable, namespaced string name (e.g.
/// `"aether.tick"`, `"hello.npc_health"`). The name is resolved to a
/// runtime `u32` id by the substrate's kind registry at init; see
/// ADR-0005 for the resolution flow.
pub trait Kind {
    const NAME: &'static str;
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
