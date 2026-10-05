//! A module file's code: the file without its asset sections.
//!
//! An asset bundle is a module whose `aether.asset.*` custom sections carry
//! payload the compiler never reads (ADR-0163 §2). Two bundles packed from
//! one build differ only in those sections, so the module cache compiles and
//! keys the compile by what [`code_bytes`] returns, and every bundle over one
//! build shares one compile (ADR-0241 §2).
//!
//! Only asset sections are removed. Every other custom section stays: the
//! name section feeds trap symbolication, and the `aether.*` manifest
//! sections differ between modules that are different modules.

use std::borrow::Cow;
use std::ops::Range;

use wasmparser::{Chunk, Encoding, Parser, Payload};

use crate::actor::wasm::asset_manifest::ASSET_SECTION_PREFIX;

/// `wasm` with every asset section removed whole: its id byte, its length
/// and its name go with its payload, and every other byte stays in place.
/// A file that carries no asset section is its own code and is returned
/// borrowed.
///
/// # Errors
///
/// `wasmparser: …` for bytes the section walk cannot read.
pub(super) fn code_bytes(wasm: &[u8]) -> Result<Cow<'_, [u8]>, String> {
    let assets = asset_sections(wasm)?;
    if assets.is_empty() {
        return Ok(Cow::Borrowed(wasm));
    }

    let mut code = Vec::with_capacity(wasm.len() - assets.iter().map(Range::len).sum::<usize>());
    let mut kept_from = 0;
    for asset in &assets {
        code.extend_from_slice(&wasm[kept_from..asset.start]);
        kept_from = asset.end;
    }
    code.extend_from_slice(&wasm[kept_from..]);
    Ok(Cow::Owned(code))
}

/// The byte range of each asset section in `wasm`, header included, in file
/// order.
///
/// The walk drives the parser one payload at a time, because the count of
/// bytes a custom section's payload consumed is the section's whole extent,
/// however its length is encoded; a section reader's own range starts after
/// the header.
fn asset_sections(wasm: &[u8]) -> Result<Vec<Range<usize>>, String> {
    let mut parser = Parser::new(0);
    let mut assets = Vec::new();
    let mut offset = 0;

    loop {
        let chunk = parser.parse(&wasm[offset..], true).map_err(|error| format!("wasmparser: {error}"))?;
        let Chunk::Parsed { consumed, payload } = chunk else {
            return Err(format!("wasmparser: the module ends inside a section at byte {offset}"));
        };
        let section = offset..offset + consumed;
        offset = section.end;

        match payload {
            Payload::Version { encoding: Encoding::Module, .. } => {}
            // A component nests whole modules, which this flat walk does not
            // descend into. It is no core module, so the compile refuses it.
            Payload::Version { .. } => return Ok(Vec::new()),
            Payload::CustomSection(reader) if is_asset_section(reader.name()) => assets.push(section),
            Payload::End(_) => return Ok(assets),
            _ => {}
        }
    }
}

fn is_asset_section(name: &str) -> bool {
    name.starts_with(ASSET_SECTION_PREFIX)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::code_bytes;

    const HEADER: &[u8] = b"\0asm\x01\0\0\0";
    const TYPE: u8 = 1;
    const FUNCTION: u8 = 3;
    const CODE: u8 = 10;

    /// One section: its id, its length in one byte, its body.
    fn section(id: u8, body: &[u8]) -> Vec<u8> {
        [&[id, u8::try_from(body.len()).expect("a fixture section under 128 bytes")], body].concat()
    }

    fn custom_body(name: &str, data: &[u8]) -> Vec<u8> {
        [&[u8::try_from(name.len()).expect("a fixture name under 128 bytes")], name.as_bytes(), data].concat()
    }

    fn custom(name: &str, data: &[u8]) -> Vec<u8> {
        section(0, &custom_body(name, data))
    }

    /// A custom section whose length is spelled in two bytes where one would
    /// do, as the binary format allows.
    fn custom_with_padded_length(name: &str, data: &[u8]) -> Vec<u8> {
        let body = custom_body(name, data);
        let length = u8::try_from(body.len()).expect("a fixture section under 128 bytes");
        [&[0, length | 0x80, 0][..], &body].concat()
    }

    fn module(sections: &[Vec<u8>]) -> Vec<u8> {
        [HEADER, &sections.concat()].concat()
    }

    /// Asset sections before, between and after the other sections, one with
    /// a padded length, must leave exactly the file built without them. It
    /// catches a strip that leaves a section's header behind, one that
    /// measures the header instead of reading its extent, one that takes a
    /// neighbouring section or a custom section that is no asset, and one
    /// that stops at the first asset.
    #[test]
    fn the_code_is_the_file_without_its_asset_sections() {
        let types = section(TYPE, &[1, 0x60, 0, 0]);
        let functions = section(FUNCTION, &[1, 0]);
        let notes = custom("aether.assets.notes", b"kept: the prefix needs its dot");
        let bodies = section(CODE, &[1, 2, 0, 0x0b]);
        let bundle = module(&[
            custom("aether.asset.first", b"one"),
            types.clone(),
            custom_with_padded_length("aether.asset.padded", b"two"),
            functions.clone(),
            notes.clone(),
            custom("aether.asset.adjacent", b""),
            bodies.clone(),
            custom("aether.asset.last", b"four"),
        ]);

        let code = code_bytes(&bundle).expect("walk the bundle");

        assert_eq!(code.as_ref(), module(&[types, functions, notes, bodies]).as_slice());
    }

    /// A file with no asset section is its own code, borrowed: the cache
    /// reuses the file's hash as its code hash on that signal, so a copy here
    /// would cost every assetless module a second pass over its bytes.
    #[test]
    fn a_file_without_assets_is_its_own_code_borrowed() {
        let file = module(&[section(TYPE, &[1, 0x60, 0, 0]), custom("name", b"kept")]);

        let code = code_bytes(&file).expect("walk the module");

        assert!(matches!(code, Cow::Borrowed(_)), "no asset section means no copy");
        assert_eq!(code.as_ref(), file.as_slice());
    }
}
