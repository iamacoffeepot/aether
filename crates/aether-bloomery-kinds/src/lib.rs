//! Shared vocabulary of bloomery kinds: digests, typed citations, leaf kinds, the tree, programs, heads, and journal entry envelopes.
//!
//! `#![no_std]` + `alloc`. The journal, the Git projection, and WASM programs
//! cite these types without linking `SQLite`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod artifact;
mod digest;
mod entry;
mod head;
mod journal;
mod program;
mod reactor;
mod reference;
mod tree;

pub use artifact::{OpaqueBytes, Utf8Text, artifact_blob, artifact_digest, artifact_prefix, hash_bytes};
pub use digest::Digest;
pub use entry::{DecodeError, Entry, Seq};
pub use head::{Head, HeadMoved, HeadNameError, RecordedHead, RecordedHeadMove};
pub use journal::{
    JournalEntry, ReadArtifact, ReadArtifactResult, ReadEvents, ReadEventsResult, ReadHead, ReadHeadResult,
};
pub use program::{
    Detail, DetailError, ExecutorName, ExecutorNameError, Fault, FaultReason, Mode, Program, ProgramHeadMoved,
    ProgramName, ProgramNameError, Transition,
};
pub use reactor::{ReactorSet, ReactorSetError};
pub use reference::Ref;
pub use tree::{Name, NameError, Node, Path, PathError, Tree, TreeError};
