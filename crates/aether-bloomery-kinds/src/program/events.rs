//! Events that name a program and that record one execution of it.

use crate::program::Program;
use crate::program::name::{ExecutorName, ProgramName};
use crate::{Digest, Ref};

/// The mutable handle. "trim" now means declaration X. The current value of a
/// name is the fold of these in seq order; the last one wins.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.program.named")]
pub struct ProgramNamed {
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
