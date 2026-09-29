//! The reply a held debt answers with when its actor closes first
//! (ADR-0243 §1).
//!
//! An actor that holds a reply (`hold::<R>()`) owes its caller exactly one
//! `R`. When the actor closes before answering while the engine keeps
//! running, the engine sends [`HeldReply::unanswered`] in its place, so the
//! caller's reply handler still runs and frees whatever it stored for the
//! request. An engine teardown sends nothing: every requester is closing
//! with it. Every impl is
//! written by hand next to its kind, because each kind names its own failure
//! arm.
//!
//! This module also hosts the impls for the held kinds `aether-kinds`
//! defines: that crate cannot depend on this one, which depends on it.

use aether_data::ActorMail;
use aether_kinds::{
    CaptureFrameResult, DropResult, LoadResult, MeshLoadResult, PublishResult, ReplaceResult, SpawnEngineResult,
    SpawnResult,
};
use alloc::string::String;
use alloc::vec::Vec;

/// The reply a caller receives when the actor holding its debt closes before
/// answering (ADR-0243 §1). Written by hand per kind, never derived.
pub trait HeldReply: ActorMail {
    /// The failure reply sent in place of the answer the closing actor
    /// never gave.
    fn unanswered() -> Self;
}

impl HeldReply for LoadResult {
    fn unanswered() -> Self {
        Self::Err { error: "component host closed before answering".into() }
    }
}

/// A drop the component host holds while the instance's module republishes
/// (ADR-0241 §7).
impl HeldReply for DropResult {
    fn unanswered() -> Self {
        Self::Err { error: "component host closed before answering".into() }
    }
}

impl HeldReply for ReplaceResult {
    fn unanswered() -> Self {
        Self::Err { error: "component host closed before answering".into() }
    }
}

/// A publish the component host holds until its module is bound, or its
/// republish has answered (ADR-0241 §7, §9).
impl HeldReply for PublishResult {
    fn unanswered() -> Self {
        Self::Err { error: "component host closed before answering".into() }
    }
}

/// A spawn the component host holds until the instance's birth settles.
impl HeldReply for SpawnResult {
    fn unanswered() -> Self {
        Self::Err { error: "component host closed before answering".into() }
    }
}

impl HeldReply for SpawnEngineResult {
    fn unanswered() -> Self {
        Self::Err { engine_id: None, error: "fleet capability closed before the spawn answered".into() }
    }
}

impl HeldReply for CaptureFrameResult {
    fn unanswered() -> Self {
        Self::Err { error: "render capability closed before the capture answered".into() }
    }
}

/// A mesh actor that unloads before its read answers leaves the request's
/// `namespace` and `path` empty: the caller correlates the reply by its
/// correlation id alone.
impl HeldReply for MeshLoadResult {
    fn unanswered() -> Self {
        Self {
            ok: false,
            namespace: String::new(),
            path: String::new(),
            error: Some("mesh actor closed before the load answered".into()),
            warnings: Vec::new(),
        }
    }
}
