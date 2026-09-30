//! Drive: requests to the mounted journal owner, the bundle driver, the
//! inspect actor, the component host (loads, code publishes, spawns, and listings), and the
//! workspace, and the replies the sink forwards back.
//!
//! Every request goes out through the embedder's
//! `BuiltChassis::send_for_reply` with the sink as its reply target and a
//! correlation the harness minted; a `Call` goes out through
//! `BuiltChassis::send_tracked` instead, so `call` also waits for the call's
//! causal chain to settle. A reply that arrives for a request the scenario is
//! not waiting on yet is kept until it is, so two requests can be in flight at
//! once.

mod sink;

use std::fmt::Debug;
use std::marker::PhantomData;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use aether_bloomery_kinds::{
    AwaitProcessed, Call, CallOutcome, Declarations, DeclarationsResult, MoveHead, MoveHeadResult, Processed, Publish,
    PublishResult, Seq, WatchHead, WatchHeadResult,
};
use aether_bloomery_workspace::{Import, ImportResult, Run, RunResult, WorkspaceCapability};
use aether_chassis_bloomery::inspect::{InspectArtifact, InspectArtifactResult, InspectEvents, InspectEventsResult};
use aether_component::ComponentHostCapability;
use aether_data::Kind;
use aether_kinds::{ListComponents, ListComponentsResult, LoadComponent, LoadResult, Spawn, SpawnResult};
use aether_substrate::{ChassisTarget, ReplyTarget};

pub use sink::{Arrival, Reply, ReplySink};

use crate::BloomeryHarness;

/// How long one reply may take. The first `Call` on a bundle covers its wasm
/// compile on the load path.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// How many `AwaitProcessed` rounds [`BloomeryHarness::settle`] follows before
/// it calls the loop unquiescent.
const SETTLE_ROUNDS: usize = 8;

/// A request in flight, answered by one `K` once waited on.
#[must_use = "a request's reply is only observed through `BloomeryHarness::wait`"]
#[derive(Debug)]
pub struct Pending<K> {
    correlation: u64,
    /// The request's kind name and value, for a panic that names it.
    request: String,
    answer: PhantomData<fn() -> K>,
}

/// A reply kind the harness's sink receives: [`CallOutcome`],
/// [`MoveHeadResult`], [`PublishResult`], [`Processed`], [`DeclarationsResult`], [`LoadResult`],
/// [`aether_kinds::PublishResult`], [`SpawnResult`], [`ListComponentsResult`],
/// [`WatchHeadResult`], [`ImportResult`], [`RunResult`], [`InspectArtifactResult`], or
/// [`InspectEventsResult`].
pub trait Answer: sealed::Sealed {}

mod sealed {
    use super::Reply;

    /// Take `Self` out of the sink's reply, or hand the reply back when it
    /// carries another kind.
    pub trait Sealed: Sized {
        fn take(reply: Reply) -> Result<Self, Reply>;
    }
}

impl sealed::Sealed for CallOutcome {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::Call(outcome) => Ok(*outcome),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for MoveHeadResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::MoveHead(result) => Ok(result),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for PublishResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::Publish(result) => Ok(result),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for Processed {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::Processed(processed) => Ok(processed),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for LoadResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::Load(result) => Ok(*result),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for aether_kinds::PublishResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::PublishCode(result) => Ok(result),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for SpawnResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::Spawn(result) => Ok(*result),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for ListComponentsResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::List(result) => Ok(result),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for DeclarationsResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::Declarations(result) => Ok(result),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for WatchHeadResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::Watch(result) => Ok(result),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for ImportResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::Import(result) => Ok(result),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for RunResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::Run(result) => Ok(*result),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for InspectArtifactResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::InspectArtifact(result) => Ok(result),
            other => Err(other),
        }
    }
}

impl sealed::Sealed for InspectEventsResult {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::InspectEvents(result) => Ok(result),
            other => Err(other),
        }
    }
}

impl Answer for CallOutcome {}
impl Answer for MoveHeadResult {}
impl Answer for PublishResult {}
impl Answer for Processed {}
impl Answer for DeclarationsResult {}
impl Answer for LoadResult {}
impl Answer for aether_kinds::PublishResult {}
impl Answer for SpawnResult {}
impl Answer for ListComponentsResult {}
impl Answer for WatchHeadResult {}
impl Answer for ImportResult {}
impl Answer for RunResult {}
impl Answer for InspectArtifactResult {}
impl Answer for InspectEventsResult {}

impl BloomeryHarness {
    /// Send one `Call` to the bundle driver as a tracked root, wait for its
    /// outcome, then wait for the call's causal chain to settle.
    ///
    /// # Panics
    ///
    /// Panics when no outcome arrives within thirty seconds, or when the
    /// call's chain has not settled within thirty seconds of its outcome.
    pub fn call(&mut self, call: &Call) -> CallOutcome {
        let correlation = self.next_correlation();
        let (_, settled) = self.chassis.send_tracked(
            self.mounted.driver,
            call,
            Some(ReplyTarget::Actor { to: self.sink.erase(), correlation }),
        );
        let outcome =
            self.wait(Pending { correlation, request: format!("{} {call:?}", Call::NAME), answer: PhantomData });

        assert!(
            settled.recv_timeout(REPLY_TIMEOUT).is_ok(),
            "the Call's causal chain did not settle within {} seconds of its outcome: {call:?}",
            REPLY_TIMEOUT.as_secs()
        );
        outcome
    }

    /// Send one fenced `MoveHead` to the journal owner and wait for its result.
    ///
    /// # Panics
    ///
    /// Panics when no result arrives within thirty seconds.
    pub fn move_head(&mut self, move_head: &MoveHead) -> MoveHeadResult {
        let pending = self.request(self.mounted.journal, move_head);
        self.wait(pending)
    }

    /// Send one fenced `Publish` to the journal owner and wait for its result:
    /// the one way to move a head whose name exists only at run time, since
    /// its moves are `RecordedHeadMove`s.
    ///
    /// # Panics
    ///
    /// Panics when no result arrives within thirty seconds.
    pub fn publish(&mut self, publish: &Publish) -> PublishResult {
        let pending = self.request(self.mounted.journal, publish);
        self.wait(pending)
    }

    /// Send one `LoadComponent` to the component host and wait for its result,
    /// so a scenario loads a wasm component onto the booted engine the way an
    /// operator's `load_component` does.
    ///
    /// # Panics
    ///
    /// Panics when no result arrives within thirty seconds.
    pub fn load(&mut self, load: &LoadComponent) -> LoadResult {
        let pending = self.request(self.chassis.actor_ref::<ComponentHostCapability>(), load);
        self.wait(pending)
    }

    /// Send one module's `code` to the component host as a `Publish` and
    /// wait for its result, which names each namespace the module is bound
    /// to (`NS.<hash>` for a content-addressed module such as a bundle).
    ///
    /// # Panics
    ///
    /// Panics when no result arrives within thirty seconds.
    pub fn publish_code(&mut self, code: Vec<u8>) -> aether_kinds::PublishResult {
        let publish = aether_kinds::Publish { code: code.into(), configs: Vec::new() };
        let pending = self.request(self.chassis.actor_ref::<ComponentHostCapability>(), &publish);
        self.wait(pending)
    }

    /// Send one `Spawn` to the component host and wait for its result:
    /// `Spawned` when the spawn stood the instance up, `Live` when the name
    /// was already live and nothing was stood up.
    ///
    /// # Panics
    ///
    /// Panics when no result arrives within thirty seconds.
    pub fn spawn(&mut self, spawn: &Spawn) -> SpawnResult {
        let pending = self.request(self.chassis.actor_ref::<ComponentHostCapability>(), spawn);
        self.wait(pending)
    }

    /// Ask the component host for every loaded component's name (`NS`,
    /// `NS:key`, or `parent/NS:key`) and wait for the answer.
    ///
    /// # Panics
    ///
    /// Panics when no answer arrives within thirty seconds.
    pub fn list_components(&mut self) -> Vec<String> {
        let pending = self.request(self.chassis.actor_ref::<ComponentHostCapability>(), &ListComponents {});
        let ListComponentsResult { names } = self.wait(pending);
        names
    }

    /// Send one `WatchHead { after }` to the journal owner and wait for its
    /// answer, which comes once a committed write moves the head past `after`.
    ///
    /// # Panics
    ///
    /// Panics when no answer arrives within thirty seconds.
    pub fn watch_head(&mut self, after: Seq) -> WatchHeadResult {
        let pending = self.request(self.mounted.journal, &WatchHead { after: after.0 });
        self.wait(pending)
    }

    /// Send one `Import` to the composed workspace and wait for its result,
    /// which lands once the whole import is done.
    ///
    /// # Panics
    ///
    /// Panics when no result arrives within thirty seconds.
    pub fn import(&mut self, import: &Import) -> ImportResult {
        let pending = self.send_import(import);
        self.wait(pending)
    }

    /// Send one `Import` to the composed workspace without waiting, for a
    /// scenario that acts while it is outstanding.
    pub fn send_import(&mut self, import: &Import) -> Pending<ImportResult> {
        self.request(self.chassis.actor_ref::<WorkspaceCapability>(), import)
    }

    /// Send one `Import` to the composed workspace as a tracked root and block
    /// until its causal chain settles, handing back its reply to wait on:
    /// what a scenario observes between the two is what had happened by the
    /// time the chain settled.
    ///
    /// # Panics
    ///
    /// Panics when the chain has not settled within thirty seconds.
    pub fn settle_import(&mut self, import: &Import) -> Pending<ImportResult> {
        self.settle_request(self.chassis.actor_ref::<WorkspaceCapability>(), import)
    }

    /// [`BloomeryHarness::settle_import`] for a `Run`.
    ///
    /// # Panics
    ///
    /// Panics when the chain has not settled within thirty seconds.
    pub fn settle_run(&mut self, run: &Run) -> Pending<RunResult> {
        self.settle_request(self.chassis.actor_ref::<WorkspaceCapability>(), run)
    }

    /// Send one `Run` to the composed workspace and wait for its result,
    /// which lands once the whole run is done.
    ///
    /// # Panics
    ///
    /// Panics when no result arrives within thirty seconds.
    pub fn run(&mut self, run: &Run) -> RunResult {
        let pending = self.send_run(run);
        self.wait(pending)
    }

    /// Send one `Run` to the composed workspace without waiting, for a
    /// scenario that acts while it is outstanding.
    pub fn send_run(&mut self, run: &Run) -> Pending<RunResult> {
        self.request(self.chassis.actor_ref::<WorkspaceCapability>(), run)
    }

    /// Send one `AwaitProcessed { through }` to the bundle driver without
    /// waiting, for a scenario that acts while the barrier is outstanding.
    pub fn await_processed(&mut self, through: Seq) -> Pending<Processed> {
        self.request(self.mounted.driver, &AwaitProcessed { through: through.0 })
    }

    /// Ask the bundle driver for every decoded bundle's programs, each with its
    /// input and result kinds' names and schemas, and wait for the answer.
    ///
    /// # Panics
    ///
    /// Panics when no answer arrives within thirty seconds.
    pub fn declarations(&mut self) -> DeclarationsResult {
        let pending = self.request(self.mounted.driver, &Declarations);
        self.wait(pending)
    }

    /// Ask the inspect actor for one artifact as JSON and wait for the answer.
    ///
    /// # Panics
    ///
    /// Panics when no answer arrives within thirty seconds.
    pub fn inspect_artifact(&mut self, request: &InspectArtifact) -> InspectArtifactResult {
        let pending = self.request(self.mounted.inspect, request);
        self.wait(pending)
    }

    /// Ask the inspect actor for journal entries with their values decoded and
    /// wait for the answer.
    ///
    /// # Panics
    ///
    /// Panics when no answer arrives within thirty seconds.
    pub fn inspect_events(&mut self, request: &InspectEvents) -> InspectEventsResult {
        let pending = self.request(self.mounted.inspect, request);
        self.wait(pending)
    }

    /// Drive the barrier at `through` to quiescence: re-send `AwaitProcessed`
    /// at each head the driver reports until the head it answers equals the
    /// bound it was asked for, and return that head.
    ///
    /// # Panics
    ///
    /// Panics when a round's `Processed` does not arrive within thirty seconds,
    /// when the driver answers `Processed::Closed` because it closed first, or
    /// when the head is still moving after eight rounds.
    pub fn settle(&mut self, through: Seq) -> Seq {
        let mut bound = through;
        for _ in 0..SETTLE_ROUNDS {
            let pending = self.await_processed(bound);
            let head = match self.wait(pending) {
                Processed::Head { head } => Seq(head),
                Processed::Closed => panic!("the bundle driver closed before the barrier at {bound} was reached"),
            };
            if head == bound {
                return head;
            }
            bound = head;
        }
        panic!("settle did not quiesce within {SETTLE_ROUNDS} rounds from {through}");
    }

    /// Wait for the reply to `pending`, keeping any reply to another request
    /// that arrives first.
    ///
    /// # Panics
    ///
    /// Panics when no reply arrives within thirty seconds, when the sink has
    /// gone, or when the reply is not the kind the request is answered by.
    pub fn wait<K: Answer>(&mut self, pending: Pending<K>) -> K {
        self.wait_within(pending, REPLY_TIMEOUT)
    }

    /// [`wait`](Self::wait) with a caller-chosen hang bound, for a request that
    /// legitimately runs past the default thirty seconds.
    ///
    /// # Panics
    ///
    /// Panics when no reply arrives within `guard`, when the sink has gone, or
    /// when the reply is not the kind the request is answered by.
    pub fn wait_within<K: Answer>(&mut self, pending: Pending<K>, guard: Duration) -> K {
        let Pending { correlation, request, .. } = pending;
        let reply = self.early.remove(&correlation).unwrap_or_else(|| self.receive(correlation, &request, guard));
        K::take(reply).unwrap_or_else(|other| panic!("{request} (correlation {correlation}) was answered by {other:?}"))
    }

    /// Send `mail` to `to` with the sink as its reply target, under a fresh
    /// correlation.
    fn request<K: Kind + Debug, I, A>(&mut self, to: impl ChassisTarget<K, I>, mail: &K) -> Pending<A> {
        let correlation = self.next_correlation();
        self.chassis.send_for_reply(to, mail, ReplyTarget::Actor { to: self.sink.erase(), correlation });
        Pending { correlation, request: format!("{} {mail:?}", K::NAME), answer: PhantomData }
    }

    /// Send `mail` to `to` as a tracked root with the sink as its reply
    /// target, and block until its causal chain settles.
    fn settle_request<K: Kind + Debug, I, A>(&mut self, to: impl ChassisTarget<K, I>, mail: &K) -> Pending<A> {
        let correlation = self.next_correlation();
        let (_, settled) =
            self.chassis.send_tracked(to, mail, Some(ReplyTarget::Actor { to: self.sink.erase(), correlation }));
        let request = format!("{} {mail:?}", K::NAME);

        assert!(
            settled.recv_timeout(REPLY_TIMEOUT).is_ok(),
            "the causal chain of {request} did not settle within {} seconds",
            REPLY_TIMEOUT.as_secs()
        );
        Pending { correlation, request, answer: PhantomData }
    }

    /// Mint a fresh correlation for one request.
    fn next_correlation(&mut self) -> u64 {
        self.correlations += 1;
        self.correlations
    }

    /// Receive arrivals until the reply to `request` under `correlation`,
    /// keeping the others.
    fn receive(&mut self, correlation: u64, request: &str, guard: Duration) -> Reply {
        let deadline = Instant::now() + guard;
        loop {
            match self.arrivals.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok((arrived, reply)) if arrived == correlation => return reply,
                Ok((arrived, reply)) => {
                    self.early.insert(arrived, reply);
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("no reply to {request} (correlation {correlation}) within {} seconds", guard.as_secs())
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the reply sink went away before {request} (correlation {correlation}) was answered")
                }
            }
        }
    }
}
