//! The protocols the program role's references are typed by (ADR-0231 §4):
//! each one names exactly the rows its target publishes, so a holder casts an
//! arriving reference to it once, at receipt, and sends through the typed
//! reference after.

use aether_actor::Undeclared;

use crate::{ApiCall, ApiCallResult, Invoke, Invoked, ReadArtifact, ReadArtifactResult};

/// A bundle root that runs programs.
///
/// The driver holds one per loaded bundle that declares the program role,
/// cast from the root's spawn reply, and sends it each `Invoke`. A generated
/// root publishes this row only when its bundle declares programs.
#[aether_actor::protocol]
pub trait ProgramRoot {
    /// Run one named program on a per-seq invocation.
    fn invoke(mail: Invoke) -> Invoked;
}

/// What a program root relays an invocation's fetch and API call to: the
/// driver that invoked it.
///
/// A generated root casts each `Invoke`'s sender to it once, when the `Invoke`
/// arrives, and relays that invocation's requests through the typed reference.
#[aether_actor::protocol]
pub trait ProgramInvoker {
    /// Fetch one artifact the invocation missed.
    fn fetch(mail: ReadArtifact) -> ReadArtifactResult;
    /// Run one program API call the invocation captured.
    fn call(mail: ApiCall) -> ApiCallResult;
}

/// What an invocation sends to its parent root.
///
/// A generated invocation casts its `Invoke`'s sender, the root that spawned
/// it, to this once, and sends its fetches, API calls, and final `Invoked`
/// through it. The root relays the requests, so their rows are unchecked.
#[aether_actor::protocol]
pub trait ProgramRelay {
    /// The invocation's one final report.
    fn invoked(mail: Invoked);
    /// A fetch-on-miss for the root to relay.
    fn fetch(mail: ReadArtifact) -> Undeclared;
    /// A program API call for the root to relay.
    fn call(mail: ApiCall) -> Undeclared;
}
