//! Identical code under a new content hash, for replace tests.
//!
//! A republish of byte-identical wasm answers `Ok` without a swap (ADR-0241
//! §4), so a test that replaces a guest with its own code, to prove what a
//! replace preserves, needs bytes that hash differently and run the same.
//! [`successor_wasm`] appends a custom section the host never reads, which
//! changes the module's hash and nothing else.

/// Name of the custom section [`successor_wasm`] appends.
const SUCCESSOR_SECTION: &str = "test.successor";

/// `wasm` with a `test.successor` custom section appended, whose payload is
/// `generation` as a little-endian `u32`. Distinct generations of one module
/// give distinct bytes, so each is a new content hash for identical code.
#[must_use]
pub fn successor_wasm(wasm: &[u8], generation: u32) -> Vec<u8> {
    let content = [&leb128(SUCCESSOR_SECTION.len()), SUCCESSOR_SECTION.as_bytes(), &generation.to_le_bytes()].concat();

    [wasm, &[0], &leb128(content.len()), &content].concat()
}

/// `value` as an unsigned LEB128, the encoding of a wasm section's size and
/// a custom section's name length.
fn leb128(mut value: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let low = value.to_le_bytes()[0] & 0x7f;
        value >>= 7;
        if value == 0 {
            bytes.push(low);
            return bytes;
        }
        bytes.push(low | 0x80);
    }
}

#[cfg(all(test, feature = "wasm"))]
mod tests {
    use wasmparser::{Parser, Payload, Validator};

    use super::{SUCCESSOR_SECTION, successor_wasm};

    #[test]
    fn a_successor_section_keeps_the_module_valid_and_readable() {
        // Catches: a malformed section header (a wrong id, size, or name
        // length), which leaves the module invalid or the payload unreadable.
        let wasm = wat::parse_str(r#"(module (func (export "noop")))"#).expect("assemble the module");
        let successor = successor_wasm(&wasm, 0x7109_0002);

        Validator::new().validate_all(&successor).expect("the successor is a valid module");
        let payloads: Vec<Vec<u8>> = Parser::new(0)
            .parse_all(&successor)
            .filter_map(|payload| match payload.expect("parse the successor") {
                Payload::CustomSection(reader) if reader.name() == SUCCESSOR_SECTION => Some(reader.data().to_vec()),
                _ => None,
            })
            .collect();
        assert_eq!(payloads, [0x7109_0002_u32.to_le_bytes().to_vec()], "one section carries the generation");
    }
}
