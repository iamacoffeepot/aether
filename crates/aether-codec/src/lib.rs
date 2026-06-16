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
//!   **`Bytes` JSON projection.** The postcard `&[u8]` wire format
//!   (`varint(len)` + raw bytes) maps to three accepted encode shapes
//!   and two canonical decode shapes:
//!
//!   Encode accepts:
//!   - `[u8, ...]` — array of integer byte values (original form).
//!   - `"…"` — bare JSON string, stored as its UTF-8 bytes.
//!   - `{ "base64": "…" }` — standard-alphabet base64 for binary that
//!     isn't valid UTF-8. A bare string is never auto-decoded as base64;
//!     only the explicit object triggers base64 decoding.
//!
//!   Decode emits:
//!   - `"…"` — when the bytes are valid UTF-8 (the common case).
//!   - `[u8, ...]` — when the bytes are not valid UTF-8.
//!
//!   The postcard wire bytes are identical across all encode shapes; the
//!   decode output is a fixed point under encode (string → UTF-8 round-
//!   trips as string; non-UTF-8 array round-trips as array).
//!
//! - **Stream framing** ([`frame`]): length-prefixed postcard for
//!   serde-derived enum types. The hub channel (`aether_hub::wire`)
//!   is the first consumer; ADR-0072 placed framing here because the
//!   helpers are codec-shaped and generic over `<T: Serialize>`.
//!
//! Future formats (msgpack, protobuf, save-format adapters) land as
//! sibling modules. Future framing variants subdivide [`frame`] under
//! `frame::postcard` / `frame::protobuf`.

mod cast;
mod decode;
mod encode;
pub mod frame;
#[cfg(test)]
mod proptest_roundtrip;
#[cfg(test)]
mod test_fixtures;

pub use decode::{DecodeError, decode_schema};
pub use encode::{EncodeError, encode_schema};
