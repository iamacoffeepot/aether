//! Byte encoding toolkit. Two layers, both pure functions and free of async.
//!
//! [`encode_schema`] and [`decode_schema`] walk an `aether_data::SchemaType`
//! descriptor to turn caller-supplied JSON params into the wire bytes the
//! substrate decodes, and back out again. The descriptor picks the wire shape
//! (ADR-0019): a cast-shaped `Struct { repr_c: true }`, and the tree under it,
//! is written as its `#[repr(C)]` byte layout, which the substrate decodes
//! with `bytemuck::cast`; everything else is written in the
//! `aether_data::wire` format, byte for byte.
//!
//! [`decode_schema_strict`] is the same walk under a narrower policy, for a
//! caller that forwards decoded bytes across a protocol boundary: non-finite
//! floats and repeated map keys become errors instead of `null` and
//! last-writer-wins, and the caller names the ceiling on projected values.
//!
//! [`frame`] is the second layer, length-prefixed framing for serde-derived
//! message types: a four-byte little-endian body length followed by an
//! `aether_data::wire` body (ADR-0118). `aether-rpc` and the fleet harness are
//! its consumers.

#![forbid(unsafe_code)]

mod cast;
#[cfg(test)]
mod conformance;
mod decode;
mod encode;
pub mod frame;
#[cfg(test)]
mod proptest_roundtrip;
#[cfg(test)]
mod test_fixtures;

pub use decode::{DecodeError, decode_schema, decode_schema_strict};
pub use encode::{EncodeError, encode_schema};
