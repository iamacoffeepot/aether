//! `InputsRecord` const-fn encoders (ADR-0033). The `#[actor]`
//! macro emits one postcard-compatible byte array per handler /
//! fallback / component-doc record, length-prefixed with the section
//! version tag, and drops the bytes into the `aether.kinds.inputs`
//! custom section. Writing at const-eval time keeps everything in
//! `#[link_section]` statics with no runtime serializer on the
//! guest — the wire shape matches `postcard(InputsRecord)`
//! byte-for-byte so the substrate/hub reader decodes via
//! `postcard::take_from_bytes` symmetrically.

use super::primitives::{
    option_borrowed_str_len, str_len, varint_u64_len, write_option_borrowed_str, write_str,
    write_varint_u64,
};

/// Byte length of a `Handler` record's postcard encoding. One-byte
/// enum-variant tag (`0x00`) + `varint(id)` + `postcard(name)` +
/// `option_str(doc)`.
#[must_use]
pub const fn inputs_handler_len(id: u64, name: &str, doc: Option<&str>) -> usize {
    1 + varint_u64_len(id) + str_len(name) + option_borrowed_str_len(doc)
}

/// Serialize an `InputsRecord::Handler` into a fixed-size array sized
/// by `inputs_handler_len`. Exact postcard wire shape for
/// `InputsRecord::Handler { id, name, doc }`.
#[must_use]
pub const fn write_inputs_handler<const N: usize>(
    id: u64,
    name: &str,
    doc: Option<&str>,
) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = 0usize;
    out[pos] = 0; // variant tag: Handler
    pos += 1;
    pos = write_varint_u64(id, &mut out, pos);
    pos = write_str(name, &mut out, pos);
    pos = write_option_borrowed_str(doc, &mut out, pos);
    // Silence "assigned but never read" warning on the final write.
    let _ = pos;
    out
}

/// Byte length of a `Fallback` record's postcard encoding.
#[must_use]
pub const fn inputs_fallback_len(doc: Option<&str>) -> usize {
    1 + option_borrowed_str_len(doc)
}

/// Serialize an `InputsRecord::Fallback` into a fixed-size array.
#[must_use]
pub const fn write_inputs_fallback<const N: usize>(doc: Option<&str>) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = 0usize;
    out[pos] = 1; // variant tag: Fallback
    pos += 1;
    pos = write_option_borrowed_str(doc, &mut out, pos);
    let _ = pos;
    out
}

/// Byte length of a `Component` record's postcard encoding.
#[must_use]
pub const fn inputs_component_len(doc: &str) -> usize {
    1 + str_len(doc)
}

/// Serialize an `InputsRecord::Component` into a fixed-size array.
#[must_use]
pub const fn write_inputs_component<const N: usize>(doc: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = 0usize;
    out[pos] = 2; // variant tag: Component
    pos += 1;
    pos = write_str(doc, &mut out, pos);
    let _ = pos;
    out
}

/// Byte length of a `Config` record's postcard encoding (ADR-0090 /
/// issue 1257). One-byte enum-variant tag (`0x03`) + `varint(id)` +
/// `postcard(name)`.
#[must_use]
pub const fn inputs_config_len(id: u64, name: &str) -> usize {
    1 + varint_u64_len(id) + str_len(name)
}

/// Serialize an `InputsRecord::Config` into a fixed-size array sized by
/// `inputs_config_len`. Exact postcard wire shape for
/// `InputsRecord::Config { id, name }`.
#[must_use]
pub const fn write_inputs_config<const N: usize>(id: u64, name: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = 0usize;
    out[pos] = 3; // variant tag: Config
    pos += 1;
    pos = write_varint_u64(id, &mut out, pos);
    pos = write_str(name, &mut out, pos);
    let _ = pos;
    out
}

/// Byte length of an `ActorBoundary` record's postcard encoding
/// (ADR-0096). One-byte enum-variant tag (`0x04`) + `postcard(namespace)`.
#[must_use]
pub const fn inputs_actor_boundary_len(namespace: &str) -> usize {
    1 + str_len(namespace)
}

/// Serialize an `InputsRecord::ActorBoundary` into a fixed-size array
/// sized by `inputs_actor_boundary_len`. Exact postcard wire shape for
/// `InputsRecord::ActorBoundary { namespace }` — the per-actor group
/// marker `export!(A, B, …)` writes ahead of each type's records.
#[must_use]
pub const fn write_inputs_actor_boundary<const N: usize>(namespace: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = 0usize;
    out[pos] = 4; // variant tag: ActorBoundary
    pos += 1;
    pos = write_str(namespace, &mut out, pos);
    let _ = pos;
    out
}
