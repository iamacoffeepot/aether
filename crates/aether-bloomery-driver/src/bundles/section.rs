//! Bundle section reading: concatenate every `aether.bloomery.programs` and
//! `aether.bloomery.reactors` custom section, then decode the declared roles.
//!
//! wasm-ld concatenates same-named custom sections, but a module may still
//! carry several, so every payload with either section name contributes.

use aether_bloomery_kinds::{Detail, PROGRAMS_SECTION, REACTORS_SECTION, reactor_declarations};
use aether_bloomery_program::declarations;

use wasmparser::{Parser, Payload};

use super::DeclaredRoles;

/// Decode the roles a bundle declares.
///
/// Concatenates the payloads of every custom section named
/// `aether.bloomery.programs` and every one named
/// `aether.bloomery.reactors`, decoding the first with
/// [`declarations`](aether_bloomery_program::declarations) and the second
/// with [`reactor_declarations`](aether_bloomery_kinds::reactor_declarations).
/// An absent section decodes to an empty list, so that role isn't declared.
/// A bundle that does not parse, whose section does not decode, or that
/// declares neither role is refused with the reason.
///
/// # Errors
///
/// A [`Detail`] naming the failure: unparsable wasm, an undecodable
/// section, or zero declared roles.
pub fn declared_roles(wasm: &[u8]) -> Result<DeclaredRoles, Detail> {
    let mut programs_section = Vec::new();
    let mut reactors_section = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        match payload {
            Err(error) => return Err(Detail::new(format!("bundle wasm does not parse: {error}"))),
            Ok(Payload::CustomSection(reader)) if reader.name() == PROGRAMS_SECTION => {
                programs_section.extend_from_slice(reader.data());
            }
            Ok(Payload::CustomSection(reader)) if reader.name() == REACTORS_SECTION => {
                reactors_section.extend_from_slice(reader.data());
            }
            Ok(_) => {}
        }
    }
    let programs = match declarations(&programs_section) {
        Err(error) => return Err(Detail::new(format!("bundle programs section does not decode: {error}"))),
        Ok(programs) => programs,
    };
    let reactors = match reactor_declarations(&reactors_section) {
        Err(error) => return Err(Detail::new(format!("bundle reactors section does not decode: {error}"))),
        Ok(reactors) => reactors,
    };
    DeclaredRoles::new(programs, &reactors).ok_or_else(|| Detail::new("bundle declares neither programs nor reactors"))
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{OpaqueBytes, ProgramName, Utf8Text};
    use aether_data::Kind;

    use super::declared_roles;

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

    fn reactor_record(name: &[u8]) -> Vec<u8> {
        let rule = b"on_event";
        let mut out = vec![1];
        out.extend_from_slice(&u16::try_from(name.len()).expect("test name fits").to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&u16::try_from(rule.len()).expect("test rule fits").to_le_bytes());
        out.extend_from_slice(rule);
        out.extend_from_slice(&OpaqueBytes::ID.0.to_le_bytes());
        out.extend_from_slice(&OpaqueBytes::ID.0.to_le_bytes());
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

    fn program_name(name: &str) -> ProgramName {
        ProgramName::new(name).expect("valid test program name")
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

        let decoded = declared_roles(&wasm).expect("both sections decode");
        let programs = decoded.programs().expect("the program role is declared");
        let one = programs.find(&program_name("test.program.one")).expect("first program declared");
        assert_eq!(one.input, OpaqueBytes::ID);
        assert_eq!(one.result, Utf8Text::ID);
        let two = programs.find(&program_name("test.program.two")).expect("second program declared");
        assert_eq!(two.intent, "second");
        assert!(!decoded.declares_reactors());
    }

    #[test]
    fn a_module_declaring_neither_role_is_refused() {
        // Catches a bundle with no roles treated as usable, which would load a root nothing can address.
        let mut wasm = module();
        custom_section(b"some.other.section", b"not programs", &mut wasm);
        assert!(declared_roles(&wasm).is_err(), "a module with neither section is refused");
        assert!(declared_roles(&module()).is_err(), "a bare module is refused");
    }

    #[test]
    fn a_reactor_only_module_declares_only_reactors() {
        // Catches a missing programs section refusing the whole bundle instead of only the program role.
        let mut wasm = module();
        custom_section(b"aether.bloomery.reactors", &reactor_record(b"test.reactor"), &mut wasm);

        let decoded = declared_roles(&wasm).expect("a reactor-only module declares its role");
        assert!(decoded.programs().is_none());
        assert!(decoded.declares_reactors());
    }

    #[test]
    fn a_mixed_module_declares_both_roles() {
        // Catches the second section being ignored, which would refuse one role of a mixed bundle.
        let mut wasm = module();
        custom_section(
            b"aether.bloomery.programs",
            &record(b"test.program", OpaqueBytes::ID.0, Utf8Text::ID.0, b"run it"),
            &mut wasm,
        );
        custom_section(b"aether.bloomery.reactors", &reactor_record(b"test.reactor"), &mut wasm);

        let decoded = declared_roles(&wasm).expect("a mixed module declares both roles");
        let programs = decoded.programs().expect("the program role is declared");
        assert!(programs.find(&program_name("test.program")).is_some());
        assert!(decoded.declares_reactors());
    }

    #[test]
    fn an_undecodable_reactors_section_refuses_the_bundle() {
        // Catches a malformed section trusted for the other role.
        let mut wasm = module();
        custom_section(
            b"aether.bloomery.programs",
            &record(b"test.program", OpaqueBytes::ID.0, Utf8Text::ID.0, b"run it"),
            &mut wasm,
        );
        custom_section(b"aether.bloomery.reactors", &[9], &mut wasm);

        let Err(reason) = declared_roles(&wasm) else {
            panic!("a module with an undecodable reactors section is refused");
        };
        assert!(reason.as_str().contains("does not decode"), "unexpected reason: {reason:?}");
    }
}
