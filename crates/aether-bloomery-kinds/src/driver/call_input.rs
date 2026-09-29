//! Compatibility decoding for the first `CallProgram` wire generation.

use aether_data::{Kind, KindId};

use crate::{CallInput, CallProgram, Digest, Head, OpaqueBytes, ProgramName};

#[aether_data::kind(name = "aether.bloomery.driver.call_program", eq, no_serde)]
struct LegacyCallProgram {
    program: Head<OpaqueBytes>,
    name: ProgramName,
    input: Digest,
}

/// Pinned kind id for the digest-only `CallProgram` wire generation.
pub const LEGACY_CALL_PROGRAM_ID: KindId = LegacyCallProgram::ID;

/// Decode either `CallProgram` wire generation selected by its kind id.
///
/// The kind id chooses exactly one schema; malformed bytes never fall back
/// to the other generation.
#[must_use]
pub fn decode_call_program(kind: KindId, bytes: &[u8]) -> Option<CallProgram> {
    if kind == CallProgram::ID {
        return CallProgram::decode_from_bytes(bytes);
    }
    if kind == LegacyCallProgram::ID {
        return LegacyCallProgram::decode_from_bytes(bytes).map(|legacy| CallProgram {
            program: legacy.program,
            name: legacy.name,
            input: CallInput::Stored(legacy.input),
        });
    }
    None
}
