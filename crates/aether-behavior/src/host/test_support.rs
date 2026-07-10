//! Hand-built minimal wasm fixtures for the host-side unit tests (issue
//! 2687). Behavior-script fixtures compiled from Rust are #2688's; these are
//! tiny WAT modules parsed inline via `wat`, with no build wiring, so the
//! fail-open state machine and the swap/rehydrate paths are exercisable under
//! plain `cargo test`.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use aether_data::KindId;

use crate::envelope::{self, FilterOutput, Verdict};
use crate::manifest;

/// A module whose `filter` always traps (`unreachable`), declaring `handled`
/// in its exports section. Drives the fail-open trap counter.
pub(crate) fn trapping_wasm(handled: KindId) -> Vec<u8> {
    module(
        r#"(func (export "filter") (param i64 i32 i32) (result i64)
             unreachable)"#,
        None,
        &[handled],
    )
}

/// A module whose `filter` spins until the host fuel budget traps it.
pub(crate) fn fuel_exhausting_wasm(handled: KindId) -> Vec<u8> {
    module(
        r#"(func (export "filter") (param i64 i32 i32) (result i64)
             (loop $spin
               (br $spin))
             (i64.const 0))"#,
        None,
        &[handled],
    )
}

/// A module whose `filter` returns an empty packed buffer. The host rejects
/// this as undecodable output and records a fail-open trap.
pub(crate) fn empty_return_wasm(handled: KindId) -> Vec<u8> {
    module(
        r#"(func (export "filter") (param i64 i32 i32) (result i64)
             (i64.const 0))"#,
        None,
        &[handled],
    )
}

/// A module whose `filter` returns a packed pointer outside linear memory.
pub(crate) fn out_of_bounds_return_wasm(handled: KindId) -> Vec<u8> {
    let packed = (70_000u64 << 32) | 1;
    let body = format!(
        r#"(func (export "filter") (param i64 i32 i32) (result i64)
             (i64.const {packed}))"#,
        packed = packed as i64,
    );
    module(&body, None, &[handled])
}

/// A module whose `filter` returns a fixed, pre-encoded [`FilterOutput`] baked
/// into a data segment (ignoring its inputs). A clean, counter-resetting call.
pub(crate) fn fixed_output_wasm(handled: KindId, output: &FilterOutput) -> Vec<u8> {
    let encoded = envelope::encode(output);
    let packed = (2048u64 << 32) | (encoded.len() as u64);
    let body = format!(
        r#"(func (export "filter") (param i64 i32 i32) (result i64)
             (i64.const {packed}))"#,
        packed = packed as i64,
    );
    module(&body, Some((2048, &encoded)), &[handled])
}

/// A module whose `filter` traps on `trap_kind` and otherwise returns a fixed,
/// pre-encoded [`FilterOutput`]. Lets tests witness a trap before a clean call.
pub(crate) fn conditional_trap_wasm(handled: KindId, trap_kind: KindId, output: &FilterOutput) -> Vec<u8> {
    let encoded = envelope::encode(output);
    let packed = (2048u64 << 32) | (encoded.len() as u64);
    let body = format!(
        r#"(func (export "filter") (param i64 i32 i32) (result i64)
             (if (result i64)
               (i64.eq (local.get 0) (i64.const {trap_kind}))
               (then unreachable)
               (else (i64.const {packed}))))"#,
        trap_kind = trap_kind.0 as i64,
        packed = packed as i64,
    );
    module(&body, Some((2048, &encoded)), &[handled, trap_kind])
}

/// A module whose `state_load` remembers the offered `(ptr, len)` region and
/// whose `state_save` returns that region packed, or a baked default when no
/// prior state was offered yet.
pub(crate) fn stateful_wasm(handled: KindId, default_state: &[u8]) -> Vec<u8> {
    let filter_output = envelope::encode(&forward_output(b"stateful"));
    let filter_packed = (3072u64 << 32) | (filter_output.len() as u64);
    let body = format!(
        r#"(global $state_ptr (mut i32) (i32.const 2048))
           (global $state_len (mut i32) (i32.const {default_len}))
           (func (export "filter") (param i64 i32 i32) (result i64)
             (i64.const {filter_packed}))
           (func (export "state_load") (param $ptr i32) (param $len i32) (result i32)
             (global.set $state_ptr (local.get $ptr))
             (global.set $state_len (local.get $len))
             (i32.const 0))
           (func (export "state_save") (result i64)
             (i64.or
               (i64.shl (i64.extend_i32_u (global.get $state_ptr)) (i64.const 32))
               (i64.extend_i32_u (global.get $state_len))))"#,
        default_len = default_state.len(),
        filter_packed = filter_packed as i64,
    );
    module_with_data(&body, &[(2048, default_state), (3072, &filter_output)], &[handled])
}

/// A `Forward`-the-inbound `FilterOutput` for `bytes` — a passthrough script's
/// output.
pub(crate) fn forward_output(bytes: &[u8]) -> FilterOutput {
    FilterOutput { verdict: Verdict::Forward(bytes.to_vec()), effects: Vec::new() }
}

/// Assemble a module: a memory + bump allocator, the caller's `filter`, an
/// optional data segment, and the exports custom section for `kinds`.
fn module(filter_body: &str, data: Option<(u32, &[u8])>, kinds: &[KindId]) -> Vec<u8> {
    match data {
        Some(data) => module_with_data(filter_body, &[data], kinds),
        None => module_with_data(filter_body, &[], kinds),
    }
}

fn module_with_data(filter_body: &str, data: &[(u32, &[u8])], kinds: &[KindId]) -> Vec<u8> {
    let data_wat = data
        .iter()
        .map(|(offset, bytes)| format!(r#"(data (i32.const {offset}) "{}")"#, byte_string(bytes)))
        .collect::<Vec<_>>()
        .join("\n             ");
    let section = custom_section_wat(&exports_section(kinds));
    let text = format!(
        r#"(module
             (memory (export "memory") 1)
             (global $bump (mut i32) (i32.const 1024))
             (func (export "alloc") (param i32 i32 i32 i32) (result i32)
               (local $p i32)
               (local.set $p (global.get $bump))
               (global.set $bump (i32.add (global.get $bump) (local.get 3)))
               (local.get $p))
             {filter_body}
             {data_wat}
             {section})"#,
    );
    wat::parse_str(&text).expect("test setup: WAT fixture parses")
}

fn exports_section(kinds: &[KindId]) -> Vec<u8> {
    let mut out = vec![manifest::EXPORTS_MANIFEST_VERSION];
    for k in kinds {
        out.extend_from_slice(&k.0.to_le_bytes());
    }
    out
}

fn custom_section_wat(bytes: &[u8]) -> String {
    format!("(@custom \"{name}\" \"{hex}\")", name = manifest::EXPORTS_SECTION, hex = byte_string(bytes),)
}

/// Render bytes as a WAT string literal's `\xx` escapes.
fn byte_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\{b:02x}")).collect()
}
