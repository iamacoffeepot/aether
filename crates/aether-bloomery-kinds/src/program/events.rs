//! Events that bind a name to a program and that record one execution of it.

use crate::program::Program;
use crate::program::name::{ExecutorName, ProgramName};
use crate::{Digest, Ref};

/// Points the head string `name` at a declaration. Last move wins. First
/// binding, rebinding after a signature change, and pointing back at an
/// older declaration are all this one event.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.program.head_moved")]
pub struct ProgramHeadMoved {
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
