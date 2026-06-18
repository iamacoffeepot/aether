//! `InputsRecord` const-fn encoders (ADR-0033). The `#[actor]`
//! macro emits one ADR-0118 aether-wire byte array per handler /
//! fallback / component-doc record, length-prefixed with the section
//! version tag, and drops the bytes into the `aether.kinds.inputs`
//! custom section. Writing at const-eval time keeps everything in
//! `#[link_section]` statics with no runtime serializer on the
//! guest — the wire shape matches `wire::to_vec(InputsRecord)`
//! byte-for-byte so the substrate/hub reader decodes via
//! `wire::take_from_bytes` symmetrically.

use super::primitives::{
    U32_WIDTH, U64_WIDTH, option_borrowed_str_len, str_len, write_option_borrowed_str, write_str,
    write_u32_le, write_u64_le,
};

/// Byte length of a [`ReplyContract`](crate::ReplyContract)'s aether-wire
/// encoding from its `(tag, id)` pair. The selector is a fixed `u32`; the
/// `One` / `Stream` arms carry a trailing `KindId` (a bare `u64`), the
/// `None` / `Manual` arms carry nothing. Variant order (`None` = 0,
/// `One` = 1, `Stream` = 2, `Manual` = 3) is the selector, matching the
/// enum's declared order.
#[must_use]
pub const fn reply_contract_len(reply_tag: u8, _reply_id: u64) -> usize {
    match reply_tag {
        // One / Stream carry a trailing `KindId` (bare u64).
        1 | 2 => U32_WIDTH + U64_WIDTH,
        // None / Manual (and any other) carry just the selector.
        _ => U32_WIDTH,
    }
}

/// Serialize a [`ReplyContract`](crate::ReplyContract) into `out` at
/// `pos` from its `(tag, id)` pair, returning the new cursor. Exact
/// `wire::to_vec(ReplyContract)` shape: a `u32` LE selector then, for
/// `One` / `Stream`, the `KindId` as a bare `u64` LE.
#[must_use]
pub const fn write_reply_contract(
    reply_tag: u8,
    reply_id: u64,
    out: &mut [u8],
    mut pos: usize,
) -> usize {
    pos = write_u32_le(reply_tag as u32, out, pos);
    if reply_tag == 1 || reply_tag == 2 {
        pos = write_u64_le(reply_id, out, pos);
    }
    pos
}

/// Byte length of a `Handler` record's aether-wire encoding. A `u32` LE
/// variant selector (`0`) + the `KindId` id (bare `u64`) + `wire(name)` +
/// `option_str(doc)` + `reply_contract(reply_tag, reply_id)` — the
/// ADR-0112 reply class.
#[must_use]
pub const fn inputs_handler_len(
    _id: u64,
    name: &str,
    doc: Option<&str>,
    reply_tag: u8,
    reply_id: u64,
) -> usize {
    U32_WIDTH
        + U64_WIDTH
        + str_len(name)
        + option_borrowed_str_len(doc)
        + reply_contract_len(reply_tag, reply_id)
}

/// Serialize an `InputsRecord::Handler` into a fixed-size array sized
/// by `inputs_handler_len`. Exact aether-wire shape for
/// `InputsRecord::Handler { id, name, doc, reply }`.
#[must_use]
pub const fn write_inputs_handler<const N: usize>(
    id: u64,
    name: &str,
    doc: Option<&str>,
    reply_tag: u8,
    reply_id: u64,
) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = write_u32_le(0, &mut out, 0); // variant selector: Handler
    pos = write_u64_le(id, &mut out, pos);
    pos = write_str(name, &mut out, pos);
    pos = write_option_borrowed_str(doc, &mut out, pos);
    pos = write_reply_contract(reply_tag, reply_id, &mut out, pos);
    // Silence "assigned but never read" warning on the final write.
    let _ = pos;
    out
}

/// Byte length of a `Fallback` record's aether-wire encoding.
#[must_use]
pub const fn inputs_fallback_len(doc: Option<&str>) -> usize {
    U32_WIDTH + option_borrowed_str_len(doc)
}

/// Serialize an `InputsRecord::Fallback` into a fixed-size array.
#[must_use]
pub const fn write_inputs_fallback<const N: usize>(doc: Option<&str>) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = write_u32_le(1, &mut out, 0); // variant selector: Fallback
    pos = write_option_borrowed_str(doc, &mut out, pos);
    let _ = pos;
    out
}

/// Byte length of a `Component` record's aether-wire encoding.
#[must_use]
pub const fn inputs_component_len(doc: &str) -> usize {
    U32_WIDTH + str_len(doc)
}

/// Serialize an `InputsRecord::Component` into a fixed-size array.
#[must_use]
pub const fn write_inputs_component<const N: usize>(doc: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = write_u32_le(2, &mut out, 0); // variant selector: Component
    pos = write_str(doc, &mut out, pos);
    let _ = pos;
    out
}

/// Byte length of a `Config` record's aether-wire encoding (ADR-0090 /
/// issue 1257). A `u32` LE variant selector (`3`) + the `KindId` id (bare
/// `u64`) + `wire(name)`.
#[must_use]
pub const fn inputs_config_len(_id: u64, name: &str) -> usize {
    U32_WIDTH + U64_WIDTH + str_len(name)
}

/// Serialize an `InputsRecord::Config` into a fixed-size array sized by
/// `inputs_config_len`. Exact aether-wire shape for
/// `InputsRecord::Config { id, name }`.
#[must_use]
pub const fn write_inputs_config<const N: usize>(id: u64, name: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = write_u32_le(3, &mut out, 0); // variant selector: Config
    pos = write_u64_le(id, &mut out, pos);
    pos = write_str(name, &mut out, pos);
    let _ = pos;
    out
}

/// Byte length of an `ActorBoundary` record's aether-wire encoding
/// (ADR-0096). A `u32` LE variant selector (`4`) + `wire(namespace)`.
#[must_use]
pub const fn inputs_actor_boundary_len(namespace: &str) -> usize {
    U32_WIDTH + str_len(namespace)
}

/// Serialize an `InputsRecord::ActorBoundary` into a fixed-size array
/// sized by `inputs_actor_boundary_len`. Exact aether-wire shape for
/// `InputsRecord::ActorBoundary { namespace }` — the per-actor group
/// marker `export!(A, B, …)` writes ahead of each type's records.
#[must_use]
pub const fn write_inputs_actor_boundary<const N: usize>(namespace: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = write_u32_le(4, &mut out, 0); // variant selector: ActorBoundary
    pos = write_str(namespace, &mut out, pos);
    let _ = pos;
    out
}
