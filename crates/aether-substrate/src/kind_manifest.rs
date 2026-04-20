// ADR-0028: read a component's embedded kind manifest from the
// `aether.kinds` wasm custom section. Each section entry is a
// concatenation of records emitted by `#[derive(Kind)]` at
// compile time, one record per kind type reachable from the
// component binary.
//
// Record format (v1):
//   [0x01] [postcard(KindDescriptor)]
//
// The section is parsed sequentially: read one version byte, then
// decode a `KindDescriptor` from the bytes that follow. `postcard`
// stops decoding exactly at the descriptor's end, so the next byte
// is the next record's version tag. Decoders for retired versions
// would lift to the canonical `KindDescriptor` shape here; today
// there's only v1 so lifting is the identity.
//
// Unknown version bytes abort the parse. Silently skipping would
// let a kind missing from the caller's build show up much later as
// a `resolve_kind → KIND_NOT_FOUND` panic; surfacing the version
// mismatch at load time produces a much clearer failure.
//
// Wasmtime 30 doesn't expose custom sections on `Module`, so we
// walk the raw bytes via `wasmparser` before compilation. The
// section data lives in the binary's original bytes anyway —
// compilation isn't a prerequisite for reading it, and parsing
// the raw bytes lets us fail on an unknown manifest version
// before we've spent cycles compiling.

use aether_hub_protocol::KindDescriptor;
use wasmparser::{Parser, Payload};

/// Section name the derive writes to. Must match
/// `aether-mail-derive`'s `#[link_section = "aether.kinds"]`.
pub const MANIFEST_SECTION: &str = "aether.kinds";

/// Record-format versions this build understands. A record tagged
/// with a version not in this set aborts the load.
const SUPPORTED_VERSIONS: &[u8] = &[0x01];

/// Decode every record in the component's `aether.kinds` custom
/// section(s). Components without the section decode to an empty
/// vec — matches the behavior of a LoadComponent with empty
/// `kinds` and lets WAT-only tests keep working without change.
pub fn read_from_bytes(wasm: &[u8]) -> Result<Vec<KindDescriptor>, String> {
    let mut descriptors = Vec::new();
    // `Parser::parse_all` walks top-level payloads without
    // decoding the code section, so the cost is linear in the
    // binary header and section table — cheap even for a few-MB
    // component. Multiple `(@custom "aether.kinds" ...)` entries
    // across link units arrive as separate payloads; concatenate
    // in document order.
    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|e| format!("wasmparser: {e}"))?;
        let Payload::CustomSection(reader) = payload else {
            continue;
        };
        if reader.name() != MANIFEST_SECTION {
            continue;
        }
        let mut cursor: &[u8] = reader.data();
        while !cursor.is_empty() {
            let version = cursor[0];
            if !SUPPORTED_VERSIONS.contains(&version) {
                return Err(format!(
                    "{MANIFEST_SECTION}: record version {version:#x} not understood by this substrate build"
                ));
            }
            let body = &cursor[1..];
            match postcard::take_from_bytes::<KindDescriptor>(body) {
                Ok((descriptor, rest)) => {
                    descriptors.push(descriptor);
                    cursor = rest;
                }
                Err(e) => {
                    return Err(format!(
                        "{MANIFEST_SECTION}: postcard decode failed at record {}: {e}",
                        descriptors.len() + 1
                    ));
                }
            }
        }
    }
    Ok(descriptors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_hub_protocol::{NamedField, Primitive, SchemaType};

    /// Build a record by hand (same shape the derive emits) and
    /// confirm the reader recovers the descriptor. Uses a WAT
    /// module with `(@custom "aether.kinds" ...)` so the real
    /// parser path exercises without needing a compiled wasm.
    fn wasm_with_section(section: &[u8]) -> Vec<u8> {
        let escaped: String = section.iter().map(|b| format!("\\{b:02x}")).collect();
        let wat =
            format!(r#"(module (@custom "aether.kinds" "{escaped}") (func (export "noop")))"#);
        wat::parse_str(wat).unwrap()
    }

    #[test]
    fn reads_single_record_round_trip() {
        let desc = KindDescriptor {
            name: "test.kind".to_string(),
            schema: SchemaType::Struct {
                fields: vec![NamedField {
                    name: "x".to_string(),
                    ty: SchemaType::Scalar(Primitive::U32),
                }],
                repr_c: true,
            },
        };
        let mut section = vec![0x01u8];
        section.extend(postcard::to_allocvec(&desc).unwrap());
        let wasm = wasm_with_section(&section);
        let descs = read_from_bytes(&wasm).unwrap();
        assert_eq!(descs, vec![desc]);
    }

    #[test]
    fn reads_multiple_records_concatenated() {
        let d1 = KindDescriptor {
            name: "a".into(),
            schema: SchemaType::Unit,
        };
        let d2 = KindDescriptor {
            name: "b".into(),
            schema: SchemaType::Scalar(Primitive::U8),
        };
        let mut section = Vec::new();
        for d in [&d1, &d2] {
            section.push(0x01u8);
            section.extend(postcard::to_allocvec(d).unwrap());
        }
        let wasm = wasm_with_section(&section);
        let descs = read_from_bytes(&wasm).unwrap();
        assert_eq!(descs, vec![d1, d2]);
    }

    #[test]
    fn absent_section_returns_empty() {
        let wasm = wat::parse_str(r#"(module (func (export "noop")))"#).unwrap();
        let descs = read_from_bytes(&wasm).unwrap();
        assert!(descs.is_empty());
    }

    #[test]
    fn unknown_version_errors() {
        let wasm = wasm_with_section(&[0xff, 0x00]);
        let err = read_from_bytes(&wasm).unwrap_err();
        assert!(err.contains("0xff"), "err was: {err}");
    }
}
