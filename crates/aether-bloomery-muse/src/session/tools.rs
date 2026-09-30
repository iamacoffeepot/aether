//! How the loop calls a program.
//!
//! The loop runs a call by the program name the call recorded, over the
//! session's tree and the arguments the call decoded, and folds any
//! program's run as that call's output, so it links no tool's types. The
//! tools it binds are in [`crate::tools`].

use aether_bloomery_kinds::{CallInput, CallProgram, Head, OpaqueBytes, ProgramName};
use aether_bloomery_program::Program;

/// The head every program the loop calls resolves through: the bundle this
/// crate builds.
pub const MUSE: Head<OpaqueBytes> = Head::new("muse");

/// A call to the loop's own program `P` in the bundle [`MUSE`] resolves to,
/// over `input`.
pub fn call<P: Program>(input: CallInput) -> CallProgram {
    CallProgram { program: MUSE, name: program_name::<P>(), input }
}

/// `P`'s name as a program name.
pub fn program_name<P: Program>() -> ProgramName {
    ProgramName::new(P::NAME).expect("`#[program]` refuses an invalid NAME at compile time")
}
