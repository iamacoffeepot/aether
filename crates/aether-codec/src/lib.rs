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
//! [`encode_storage_schema`] and [`decode_storage_schema`] are the same pair
//! for stored artifacts: JSON to and from the ADR-0059 storage encoding
//! (TLV records with content-hashed field tags), byte-identical to what a
//! derived `Storage` impl writes, from the schema alone. The decode is strict
//! and bounded like [`decode_schema_strict`], and refuses any record the
//! schema does not bind.
//!
//! [`inline_blobs`] rewrites an in-process payload's tag-1 `Blob` fields
//! (a hash naming an attached store entry) to tag-0 inline bytes, the form
//! every path out of the process carries (ADR-0238 decisions 3 and 5), and
//! [`blob_hashes`] lists those hashes for the sender-side resolve.
//!
//! [`json_schema()`] renders the JSON Schema of the JSON [`encode_schema`]
//! accepts for a schema, with a type's field docs attached, for a caller that
//! offers a typed input to a model.
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
mod inline;
mod json_schema;
#[cfg(test)]
mod proptest_roundtrip;
mod storage;
#[cfg(test)]
mod test_fixtures;

pub use decode::{DecodeError, decode_schema, decode_schema_strict};
pub use encode::{EncodeError, encode_schema};
pub use inline::{InlineError, MAX_SCHEMA_DEPTH, blob_hashes, inline_blobs};
pub use json_schema::{JsonSchemaError, json_schema};
pub use storage::{decode_storage_schema, encode_storage_schema};
