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

/// A `Forward`-the-inbound `FilterOutput` for `bytes` — a passthrough script's
/// output.
pub(crate) fn forward_output(bytes: &[u8]) -> FilterOutput {
    FilterOutput {
        verdict: Verdict::Forward(bytes.to_vec()),
        effects: Vec::new(),
    }
}

/// Assemble a module: a memory + bump allocator, the caller's `filter`, an
/// optional data segment, and the exports custom section for `kinds`.
fn module(filter_body: &str, data: Option<(u32, &[u8])>, kinds: &[KindId]) -> Vec<u8> {
    let data_wat = data.map_or(String::new(), |(offset, bytes)| {
        format!(r#"(data (i32.const {offset}) "{}")"#, byte_string(bytes))
    });
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
    format!(
        "(@custom \"{name}\" \"{hex}\")",
        name = manifest::EXPORTS_SECTION,
        hex = byte_string(bytes),
    )
}

/// Render bytes as a WAT string literal's `\xx` escapes.
fn byte_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\{b:02x}")).collect()
}
