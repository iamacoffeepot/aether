//! Byte encoding toolkit. Two layers, both pure functions / no async:
//!
//! - **Schema-driven encode/decode** ([`encode_schema`] / [`decode_schema`]):
//!   walks an `aether_data::SchemaType` descriptor to encode agent-
//!   supplied JSON params into the wire bytes the substrate's decode
//!   path is happy with (and back out). Two wire shapes, picked per
//!   descriptor (ADR-0019, ADR-0020):
//!
//!   1. Cast-shaped (`Struct { repr_c: true }` and the recursive tree
//!      under it): `#[repr(C)]` byte layout. Decode is `bytemuck::cast`
//!      on the substrate side; encode walks the schema and writes the
//!      same layout.
//!   2. Postcard (everything else): postcard 1.x wire format, written
//!      and read directly to match the format byte-for-byte.
//!
//! - **Stream framing** ([`frame`]): length-prefixed postcard for
//!   serde-derived enum types. The hub channel (`aether_hub::wire`)
//!   is the first consumer; ADR-0072 placed framing here because the
//!   helpers are codec-shaped and generic over `<T: Serialize>`.
//!
//! Future formats (msgpack, protobuf, save-format adapters) land as
//! sibling modules. Future framing variants subdivide [`frame`] under
//! `frame::postcard` / `frame::protobuf`.

mod decode;
mod encode;
pub mod frame;

pub use decode::{DecodeError, decode_schema};
pub use encode::{EncodeError, encode_schema};
