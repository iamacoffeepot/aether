//! Shared vocabulary of bloomery kinds: digests, typed citations, leaf kinds, the tree, programs, heads, driver records, driver mail, reactor mail, journal entry envelopes, and bundle constants.
//!
//! `#![no_std]` + `alloc`. The journal, the Git projection, and WASM programs
//! cite these types without linking `SQLite`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod artifact;
mod bundle;
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

pub use artifact::{
    ArtifactHasher, OpaqueBytes, Utf8Text, artifact_blob, artifact_digest, artifact_prefix, hash_bytes,
};
pub use bundle::{BUNDLE_NAMESPACE, PROGRAMS_SECTION};
pub use digest::Digest;
pub use driver::{AwaitProcessed, Call, CallOutcome, CallProgram, CallRefusal, Processed, SetHead};
pub use entry::{DecodeError, Entry, Seq};
pub use head::{Head, HeadMoved, HeadNameError, RecordedHead, RecordedHeadMove};
pub use journal::{
    AppendRecords, AppendRecordsResult, ArtifactCitation, ClosureLimit, ClosureLimitError, DriverRecord,
    EncodedArtifact, JournalEntry, MoveHead, MoveHeadResult, Publish, PublishResult, ReadArtifact, ReadArtifactResult,
    ReadClosure, ReadClosureResult, ReadEvents, ReadEventsResult, ReadHead, ReadHeadResult, WatchHead, WatchHeadResult,
};
pub use lifecycle::{Activated, ActivationRejected, LiveFromError, ReactionFailed};
pub use program::{
    ClaimedDigest, ClosureArtifact, Detail, DetailError, DigestMismatch, Fault, FaultReason, Invoke, Invoked, Mode,
    NativeOrigin, NativeOriginError, Program, ProgramHeadMoved, ProgramName, ProgramNameError, ProgramRef, ReactorName,
    ReactorNameError, Refusal, RequestSource, Requested, RuleName, RuleNameError, Transition,
};
pub use reactor::{
    Evaluated, Event, REACTORS_SECTION, ReactorDeclaration, ReactorDeclarationError, ReactorDeclarationsError,
    ReactorIntent, ReactorSet, ReactorSetError, RuleDeclaration, RuleRecord, Status, StatusQuery, Warm, WarmEntries,
    WarmEntriesError, Warmed, reactor_declarations, reactor_record_len, write_reactor_record,
};
pub use reference::Ref;
pub use tree::{Name, NameError, Node, Path, PathError, Tree};
