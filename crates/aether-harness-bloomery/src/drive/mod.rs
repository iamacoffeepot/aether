//! Drive: requests to the mounted journal owner and bundle driver, and the
//! replies the sink forwards back.
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

use aether_actor::ErasedActorRef;
use aether_bloomery_kinds::{AwaitProcessed, Call, CallOutcome, MoveHead, MoveHeadResult, Processed, Seq};
use aether_data::Kind;
use aether_substrate::ReplyTarget;

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
/// [`MoveHeadResult`], or [`Processed`].
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

impl sealed::Sealed for Processed {
    fn take(reply: Reply) -> Result<Self, Reply> {
        match reply {
            Reply::Processed(processed) => Ok(processed),
            other => Err(other),
        }
    }
}

impl Answer for CallOutcome {}
impl Answer for MoveHeadResult {}
impl Answer for Processed {}

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
            self.mounted.driver.erase(),
            Call::ID,
            call.encode_into_bytes(),
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
        let pending = self.request(self.mounted.journal.erase(), move_head);
        self.wait(pending)
    }

    /// Send one `AwaitProcessed { through }` to the bundle driver without
    /// waiting, for a scenario that acts while the barrier is outstanding.
    pub fn await_processed(&mut self, through: Seq) -> Pending<Processed> {
        self.request(self.mounted.driver.erase(), &AwaitProcessed { through: through.0 })
    }

    /// Drive the barrier at `through` to quiescence: re-send `AwaitProcessed`
    /// at each head the driver reports until the head it answers equals the
    /// bound it was asked for, and return that head.
    ///
    /// # Panics
    ///
    /// Panics when a round's `Processed` does not arrive within thirty seconds,
    /// or the head is still moving after eight rounds.
    pub fn settle(&mut self, through: Seq) -> Seq {
        let mut bound = through;
        for _ in 0..SETTLE_ROUNDS {
            let pending = self.await_processed(bound);
            let head = Seq(self.wait(pending).head);
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
        let Pending { correlation, request, .. } = pending;
        let reply = self.early.remove(&correlation).unwrap_or_else(|| self.receive(correlation, &request));
        K::take(reply).unwrap_or_else(|other| panic!("{request} (correlation {correlation}) was answered by {other:?}"))
    }

    /// Send `mail` to `to` with the sink as its reply target, under a fresh
    /// correlation.
    fn request<K: Kind + Debug, A>(&mut self, to: ErasedActorRef, mail: &K) -> Pending<A> {
        let correlation = self.next_correlation();
        self.chassis.send_for_reply(
            to,
            K::ID,
            mail.encode_into_bytes(),
            ReplyTarget::Actor { to: self.sink.erase(), correlation },
        );
        Pending { correlation, request: format!("{} {mail:?}", K::NAME), answer: PhantomData }
    }

    /// Mint a fresh correlation for one request.
    fn next_correlation(&mut self) -> u64 {
        self.correlations += 1;
        self.correlations
    }

    /// Receive arrivals until the reply to `request` under `correlation`,
    /// keeping the others.
    fn receive(&mut self, correlation: u64, request: &str) -> Reply {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            match self.arrivals.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok((arrived, reply)) if arrived == correlation => return reply,
                Ok((arrived, reply)) => {
                    self.early.insert(arrived, reply);
                }
                Err(RecvTimeoutError::Timeout) => panic!(
                    "no reply to {request} (correlation {correlation}) within {} seconds",
                    REPLY_TIMEOUT.as_secs()
                ),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the reply sink went away before {request} (correlation {correlation}) was answered")
                }
            }
        }
    }
}
