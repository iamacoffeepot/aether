//! Bundle section reading: concatenate every `aether.bloomery.programs`
//! custom section, then decode the program declarations.
//!
//! wasm-ld concatenates same-named custom sections, but a module may still
//! carry several, so every payload with the section name contributes.

use aether_bloomery_kinds::{Detail, Program};

use wasmparser::{Parser, Payload};

const SECTION_NAME: &str = "aether.bloomery.programs";

/// Decode the programs a bundle declares.
///
/// Concatenates the payloads of every custom section named
/// `aether.bloomery.programs` and decodes them with
/// [`declarations`](aether_bloomery_program::declarations). A bundle that
/// does not parse, whose section does not decode, or that declares nothing
/// is refused with the reason: every one of those is a failure before
/// `Invoke`, never an unknown program.
///
/// # Errors
///
/// A [`Detail`] naming the failure: unparsable wasm, an undecodable
/// section, or zero declarations.
pub fn programs(wasm: &[u8]) -> Result<Vec<Program>, Detail> {
    let mut section = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        match payload {
            Err(error) => return Err(Detail::new(format!("bundle wasm does not parse: {error}"))),
            Ok(Payload::CustomSection(reader)) if reader.name() == SECTION_NAME => {
                section.extend_from_slice(reader.data());
            }
            Ok(_) => {}
        }
    }
    match aether_bloomery_program::declarations(&section) {
        Err(error) => Err(Detail::new(format!("bundle programs section does not decode: {error}"))),
        Ok(programs) if programs.is_empty() => Err(Detail::new("bundle declares no programs")),
        Ok(programs) => Ok(programs),
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{OpaqueBytes, Program, Utf8Text};
    use aether_data::Kind;

    use super::programs;

    fn leb(mut value: u32, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn record(name: &[u8], input: u64, result: u64, intent: &[u8]) -> Vec<u8> {
        let mut out = vec![1];
        out.extend_from_slice(&u16::try_from(name.len()).expect("test name fits").to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&input.to_le_bytes());
        out.extend_from_slice(&result.to_le_bytes());
        out.push(0);
        out.extend_from_slice(&u16::try_from(intent.len()).expect("test intent fits").to_le_bytes());
        out.extend_from_slice(intent);
        out
    }

    fn custom_section(name: &[u8], data: &[u8], module: &mut Vec<u8>) {
        let mut section = Vec::new();
        leb(u32::try_from(name.len()).expect("test name fits"), &mut section);
        section.extend_from_slice(name);
        section.extend_from_slice(data);
        module.push(0);
        leb(u32::try_from(section.len()).expect("test section fits"), module);
        module.extend_from_slice(&section);
    }

    fn module() -> Vec<u8> {
        let mut module = b"\0asm".to_vec();
        module.extend_from_slice(&1u32.to_le_bytes());
        module
    }

    fn names(programs: &[Program]) -> Vec<&str> {
        programs.iter().map(|program| program.name.as_str()).collect()
    }

    #[test]
    fn every_programs_section_is_concatenated() {
        // Catches reading only the first section when a module carries several.
        let mut wasm = module();
        custom_section(
            b"aether.bloomery.programs",
            &record(b"test.program.one", OpaqueBytes::ID.0, Utf8Text::ID.0, b"first"),
            &mut wasm,
        );
        custom_section(
            b"aether.bloomery.programs",
            &record(b"test.program.two", OpaqueBytes::ID.0, Utf8Text::ID.0, b"second"),
            &mut wasm,
        );

        let decoded = programs(&wasm).expect("both sections decode");
        assert_eq!(names(&decoded), ["test.program.one", "test.program.two"]);
        assert_eq!(decoded[0].input, OpaqueBytes::ID);
        assert_eq!(decoded[0].result, Utf8Text::ID);
        assert_eq!(decoded[1].intent, "second");
    }

    #[test]
    fn a_module_without_programs_is_refused() {
        // Catches treating an empty bundle as usable, which would misreport
        // every call as an unknown program instead of an unavailable bundle.
        let mut wasm = module();
        custom_section(b"some.other.section", b"not programs", &mut wasm);
        assert!(programs(&wasm).is_err(), "a module with no programs section is refused");
        assert!(programs(&module()).is_err(), "a bare module is refused");
    }
}
