//! Events that bind a name to a program and that record one execution of it.

use crate::program::Program;
use crate::program::name::{ExecutorName, ProgramName};
use crate::{Digest, Ref};

/// From this seq on, `name` means `program`. First binding, rebinding after a
/// signature change, and pointing back at an older declaration are all this
/// one event. The fold takes the last per name.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.program.name_moved")]
pub struct ProgramNameMoved {
    pub name: ProgramName,
    pub program: Ref<Program>,
}

/// One execution, recorded. Written only by the driver.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.transition")]
pub struct Transition {
    pub program: Ref<Program>,
    /// An artifact whose kind is `program.input`. Untyped because that kind is
    /// known only from the cited [`Program`] at runtime.
    pub input: Digest,
    /// An artifact whose kind is `program.result`. Untyped because that kind is
    /// known only from the cited [`Program`] at runtime.
    pub result: Digest,
    pub executor: ExecutorName,
}
