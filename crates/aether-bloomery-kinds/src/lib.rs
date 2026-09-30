//! Shared vocabulary of bloomery kinds: digests, unit keys, typed citations, leaf kinds, the tree, programs, heads, driver records, driver mail, reactor mail, journal entry envelopes, and bundle constants.
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
mod unit;

pub use artifact::{
    ArtifactHasher, OpaqueBytes, Utf8Text, artifact_blob, artifact_digest, artifact_prefix, hash_bytes,
};
pub use bundle::{BUNDLE_NAMESPACE, PROGRAMS_SECTION};
pub use digest::Digest;
pub use driver::{
    AwaitProcessed, BundleDeclarations, Call, CallInput, CallOutcome, CallProgram, CallRefusal, Declarations,
    DeclarationsResult, HeadChange, LEGACY_CALL_PROGRAM_ID, LEGACY_SET_HEAD_ID, Processed, ProgramDeclaration,
    SetHeads, decode_call_program, decode_set_heads,
};
pub use entry::{DecodeError, Entry, Seq};
pub use head::{Head, HeadMoved, HeadNameError, RecordedHead, RecordedHeadMove};
pub use journal::{
    AppendRecords, AppendRecordsResult, ArtifactCitation, ArtifactDigests, ArtifactDigestsError, ArtifactStorage,
    ClosureLimit, ClosureLimitError, DriverRecord, EncodedArtifact, JournalEntry, MoveHead, MoveHeadResult, Publish,
    PublishResult, ReadArtifact, ReadArtifactResult, ReadArtifacts, ReadArtifactsResult, ReadClosure,
    ReadClosureResult, ReadEvents, ReadEventsResult, ReadHead, ReadHeadResult, Stage, StageResult, WatchHead,
    WatchHeadResult,
};
pub use lifecycle::{Activated, ActivationRejected, LiveFromError, ReactionFailed};
pub use program::{
    ApiCall, ApiCallResult, CLOCK, CLOCK_BUNDLE, ClaimedDigest, ClosureArtifact, Detail, DetailError, DigestMismatch,
    ExecutorFault, Fault, FaultReason, Fired, Invoke, Invoked, MAX_DUE_AHEAD_MILLIS, Mode, NativeOrigin,
    NativeOriginError, Program, ProgramApi, ProgramHeadMoved, ProgramInvoker, ProgramName, ProgramNameError,
    ProgramRef, ProgramRelay, ProgramRoot, ReactorName, ReactorNameError, Refusal, RequestSource, Requested, RuleName,
    RuleNameError, Transition, Until, VerifiedRead,
};
pub use reactor::{
    Evaluated, Event, REACTORS_SECTION, ReactorDeclaration, ReactorDeclarationError, ReactorDeclarationsError,
    ReactorIntent, ReactorRoot, ReactorSet, ReactorSetError, RuleDeclaration, RuleRecord, Status, StatusQuery,
    UncitedArtifact, Warm, WarmEntries, WarmEntriesError, Warmed, reactor_declarations, reactor_record_len,
    write_reactor_record,
};
pub use reference::{ErasedRef, Ref};
pub use tree::{Name, NameError, Node, Path, PathError, Tree};
pub use unit::{UnitKey, UnitKeyError};
