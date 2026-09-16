//! Shared vocabulary of bloomery kinds: digests, typed citations, leaf kinds, the tree, programs, and heads.
//!
//! `#![no_std]` + `alloc`. The journal, the Git projection, and WASM programs
//! cite these types without linking `SQLite`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod artifact;
mod digest;
mod head;
mod program;
mod reference;
mod tree;

pub use artifact::{OpaqueBytes, Utf8Text, artifact_blob, artifact_digest, artifact_prefix, hash_bytes};
pub use digest::Digest;
pub use head::{HeadMoved, RecordedHeadMove, RecordedSymbol, Symbol, SymbolError};
pub use program::{
    Detail, DetailError, ExecutorName, ExecutorNameError, Fault, FaultReason, Mode, Program, ProgramHeadMoved,
    ProgramName, ProgramNameError, Transition,
};
pub use reference::Ref;
pub use tree::{Name, NameError, Node, Path, PathError, Tree, TreeError};
