//! Shared vocabulary of bloomery kinds: digests, typed citations, leaf kinds, the tree, programs, heads, driver records, driver mail, and journal entry envelopes.
//!
//! `#![no_std]` + `alloc`. The journal, the Git projection, and WASM programs
//! cite these types without linking `SQLite`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod artifact;
mod digest;
mod driver;
mod entry;
mod head;
mod journal;
mod lifecycle;
mod program;
mod reactor;
mod reference;
mod tree;

pub use artifact::{OpaqueBytes, Utf8Text, artifact_blob, artifact_digest, artifact_prefix, hash_bytes};
pub use digest::Digest;
pub use driver::{AwaitProcessed, Call, CallOutcome, CallProgram, CallRefusal, Processed, SetHead};
pub use entry::{DecodeError, Entry, Seq};
pub use head::{Head, HeadMoved, HeadNameError, RecordedHead, RecordedHeadMove};
pub use journal::{
    ArtifactCitation, EncodedArtifact, JournalEntry, MoveHead, MoveHeadResult, Publish, PublishResult, ReadArtifact,
    ReadArtifactResult, ReadEvents, ReadEventsResult, ReadHead, ReadHeadResult,
};
pub use lifecycle::{Activated, ActivationRejected, LiveFromError, ReactionFailed};
pub use program::{
    ClosureArtifact, Detail, DetailError, Fault, FaultReason, Invoke, Invoked, Mode, NativeOrigin, NativeOriginError,
    Program, ProgramHeadMoved, ProgramName, ProgramNameError, ProgramRef, ReactorName, ReactorNameError, Refusal,
    RequestSource, Requested, RuleName, RuleNameError, Transition,
};
pub use reactor::{ReactorSet, ReactorSetError};
pub use reference::Ref;
pub use tree::{Name, NameError, Node, Path, PathError, Tree, TreeError};
