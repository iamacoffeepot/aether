//! `SubstrateHarness::execute` — a declarative op sequence over the
//! settlement-gated `SubstrateHarness` primitives (issue 868).
//!
//! Every `substrate_harness`-driven test is a small state machine: load a
//! component, advance, send a mail, advance, capture, send cleanup.
//! Each step calls a separate [`SubstrateHarness`] method, and each method
//! is independently responsible for waiting on its causal chain to
//! settle (ADR-0080 §6). That per-method settlement glue has been a
//! recurring flake source (issues 834 / 836 / 838 / 860): when a
//! method forgot to wait for the chain it kicked off, parallel CI
//! surfaced the race, and the fix was a one-off patch to the
//! offending method.
//!
//! [`SubstrateHarness::execute`] centralizes the sequencing: it takes a
//! labelled list of [`HarnessOp`]s, dispatches each through the
//! matching settlement-gated primitive, blocks on settlement, then
//! proceeds. When the next timing race surfaces it gets fixed once,
//! inside the op→primitive mapping, rather than N times across the
//! per-method trapdoors. This is the typed-Rust successor to the
//! retired `aether-scenario` YAML `Script` + `Vec<Step>`.

use std::collections::HashMap;
use std::error;
use std::fmt;
use std::marker::PhantomData;
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::{Addressable, HandlesKind, One};
use aether_data::{Kind, KindId};
use aether_kinds::NamedMail;
use aether_window::{InjectWindowEvent, SyntheticWindowCapability, WindowId};

use super::harness::{SubstrateHarness, SubstrateHarnessError};

/// Wall-clock budget a [`HarnessOp::poll_until`] step gives an
/// observation before it is called a failure. Generous against the
/// in-process observations this harness makes — a capability applying a
/// `MonitorNotice`, a teardown turn retiring an id — under a saturated
/// `nextest --workspace` run, so a starved-but-healthy substrate that
/// gets there late still passes while a genuinely broken one still
/// fails within the bound. Reach for
/// [`HarnessOp::poll_until_within`] when a test wants a different one.
pub const DEFAULT_POLL_BUDGET: Duration = Duration::from_secs(10);

/// Pause between [`HarnessOp::poll_until`] probes. Each probe is itself
/// a full request/reply round trip whose wait already backs off and
/// yields the CPU, so this only stops a fast in-process reply from
/// hot-looping the mail queue and starving the very chain being waited
/// on. Matches the pump's own quiet-poll ceiling.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// One atomic step in a [`SubstrateHarness::execute`] sequence. Each variant
/// resolves via an existing settlement-gated primitive on
/// [`SubstrateHarness`]; the sequencer waits for the op's causal chain to
/// drain before proceeding to the next step.
///
/// Build ops with the typed constructors ([`HarnessOp::send_mail`],
/// [`HarnessOp::send_and_await`], [`HarnessOp::poll_until`],
/// [`HarnessOp::advance`], [`HarnessOp::capture`]) — they encode the
/// payload from a typed kind via [`Kind::encode_into_bytes`], so callers
/// never hand-encode.
///
/// Recipients are mailbox *names* (`"aether.fs"`, `"aether.component"`,
/// a loaded component's trampoline address) — mailbox ids are
/// one-way name hashes, so every send resolves by name.
///
/// # Which wait to reach for
///
/// Three ops wait, and they are not interchangeable. Pick by where the
/// effect being asserted on actually lands:
///
/// - **The effect is on the caller's causal chain** — the recipient's
///   handler does it, or a descendant mail it sent does. Use
///   [`HarnessOp::send_mail`]: it blocks on `Settled { root }`, so the
///   handler and every descendant have run by the time the next op
///   starts (ADR-0080 §6). This is the strongest barrier and the
///   default; prefer it whenever lineage carries the effect.
/// - **The effect is genuinely detached** — it lands on a chain the
///   caller never joins, so nothing here can settle it: a `MonitorNotice`
///   pruning a parent's view after a child departs (ADR-0079 §8), a slot's
///   own teardown turn retiring an id. Use [`HarnessOp::poll_until`],
///   which re-probes to a wall-clock budget and reports the last value it
///   saw when the observation never holds.
/// - **A reply correlates the request, and that is all** — use
///   [`HarnessOp::send_and_await`], and assert only on the reply itself.
///   It resolves on the matching correlation id and waits for nothing
///   else, so anything the handler kicked off may still be in flight;
///   asserting past the reply is asserting on the runner's speed.
///
/// What none of them license is an arbitrary sleep or a fixed round-trip
/// count standing in for an ordering. Both hold only while the box is
/// fast enough, and both measure the runner rather than the outcome.
pub enum HarnessOp {
    /// Run `ticks` complete frames. Build with [`HarnessOp::advance`].
    Advance { ticks: u32 },
    /// Fire-and-settle a mail; no reply is awaited. Build with the
    /// typed [`HarnessOp::send_mail`].
    SendMail { recipient: String, kind: KindId, payload: Vec<u8> },
    /// Send a mail and block until a reply arrives, stashing the raw
    /// reply bytes. Build with the typed [`HarnessOp::send_and_await`].
    /// Covers component load / replace / drop and the `aether.fs`
    /// read / write / delete / list round trips uniformly — decode
    /// the stored bytes downstream with [`ExecutionResult::reply`].
    SendAndAwait { recipient: String, kind: KindId, payload: Vec<u8> },
    /// Capture the current frame as PNG bytes. Build with
    /// [`HarnessOp::capture`]. Does not dispatch a tick — sequence a
    /// [`HarnessOp::Advance`] before it if the world must move first.
    Capture,
    /// Capture with pre/after mail bundles dispatched atomically
    /// around the readback (the `CaptureFrame` shape, ADR-0020): `pre`
    /// lands *before* the readback so its effects appear in the PNG,
    /// `after` runs *after* (cleanup). Build with
    /// [`HarnessOp::capture_with_mails`]. Use this rather than
    /// decomposing into separate `SendMail` + `Capture` ops when the
    /// pre-mail's geometry must land in the same frame as the
    /// readback.
    CaptureWithMails { pre: Vec<NamedMail>, after: Vec<NamedMail> },
    /// Re-send a probe mail until its reply satisfies the observation,
    /// or `budget` elapses. The sanctioned wait for an effect that is
    /// genuinely off the caller's chain. Build with
    /// [`HarnessOp::poll_until`] / [`HarnessOp::poll_until_within`] —
    /// `observe` is opaque, so this variant is not hand-constructible.
    ///
    /// The satisfying reply is stored like a [`HarnessOp::SendAndAwait`]
    /// one, so [`ExecutionResult::reply`] decodes the observation that
    /// ended the wait rather than a re-read of it.
    PollUntil {
        recipient: String,
        kind: KindId,
        payload: Vec<u8>,
        budget: Duration,
        observed_kind: &'static str,
        observe: PollObserver,
    },
}

/// What one probe reply told a [`HarnessOp::PollUntil`] step.
enum Observation {
    /// The predicate held — stop, and keep these bytes as the result.
    Satisfied,
    /// Decoded, predicate did not hold. Carries the reply's `Debug`
    /// image so a timeout can report what was actually last seen
    /// instead of only that the wait ran out.
    Pending(String),
    /// The reply did not decode as the observed kind — the probe is
    /// answering with something else entirely, which no amount of
    /// further waiting fixes.
    Undecodable,
}

/// The decode-and-test half of a [`HarnessOp::PollUntil`] step, erased
/// over the observed reply kind so the op stays a single non-generic
/// variant. Built by [`HarnessOp::poll_until`]; opaque to callers.
pub struct PollObserver(ObserveFn);

/// The erased decode-and-test closure a [`PollObserver`] wraps.
type ObserveFn = Box<dyn FnMut(&[u8]) -> Observation>;

/// Typed root-actor sender for declarative harness operations.
///
/// Construct one with [`HarnessOp::actor`]. The actor identity determines the
/// recipient, while [`Self::send`] infers the kind from the borrowed mail and
/// compile-checks that the actor handles it.
pub struct HarnessActor<R>(PhantomData<fn() -> R>);

impl<R> HarnessActor<R>
where
    R: Addressable<Resolver = One>,
{
    /// Build a fire-and-settle operation for a kind handled by `R`.
    #[must_use]
    pub fn send<K>(&self, mail: &K) -> HarnessOp
    where
        K: Kind,
        R: HandlesKind<K>,
    {
        HarnessOp::send_mail(R::NAMESPACE, mail)
    }
}

impl HarnessOp {
    /// Run `ticks` complete frames.
    #[must_use]
    pub fn advance(ticks: u32) -> Self {
        Self::Advance { ticks }
    }

    /// Capture the current frame.
    #[must_use]
    pub fn capture() -> Self {
        Self::Capture
    }

    /// Capture with pre/after mail bundles dispatched atomically
    /// around the readback. See [`HarnessOp::CaptureWithMails`].
    #[must_use]
    pub fn capture_with_mails(pre: Vec<NamedMail>, after: Vec<NamedMail>) -> Self {
        Self::CaptureWithMails { pre, after }
    }

    /// Bind a root actor identity for a compile-checked typed send.
    ///
    /// Only root identities using [`One`] can be constructed because a
    /// declarative operation has no parent carry or instance key with which to
    /// resolve nested or instanced actors.
    ///
    /// Unsupported direct kinds fail at compile time:
    ///
    /// ```compile_fail
    /// use aether_harness_substrate::HarnessOp;
    /// use aether_kinds::Tick;
    /// use aether_window::SyntheticWindowCapability;
    ///
    /// let _ = HarnessOp::actor::<SyntheticWindowCapability>().send(&Tick);
    /// ```
    #[must_use]
    pub fn actor<R>() -> HarnessActor<R>
    where
        R: Addressable<Resolver = One>,
    {
        HarnessActor(PhantomData)
    }

    /// Inject any typed event as originating from `window`.
    ///
    /// `K` is inferred from `event`; the synthetic runtime forwards its
    /// already-encoded payload without a maintained window-event kind list.
    #[must_use]
    pub fn window_event<K: Kind>(window: WindowId, event: &K) -> Self {
        let injection = InjectWindowEvent { window, kind: K::ID, payload: event.encode_into_bytes() };
        Self::actor::<SyntheticWindowCapability>().send(&injection)
    }

    /// Fire-and-settle a typed mail (no reply awaited). Encodes `mail`
    /// via [`Kind::encode_into_bytes`] — works for both cast and
    /// structured kinds.
    #[must_use]
    pub fn send_mail<K: Kind>(recipient: impl Into<String>, mail: &K) -> Self {
        Self::SendMail { recipient: recipient.into(), kind: K::ID, payload: mail.encode_into_bytes() }
    }

    /// Send a typed mail and block until a reply arrives. Decode the
    /// reply downstream with [`ExecutionResult::reply`]. Encodes
    /// `mail` via [`Kind::encode_into_bytes`].
    #[must_use]
    pub fn send_and_await<K: Kind>(recipient: impl Into<String>, mail: &K) -> Self {
        Self::SendAndAwait { recipient: recipient.into(), kind: K::ID, payload: mail.encode_into_bytes() }
    }

    /// Re-send `probe` to `recipient` until its reply satisfies
    /// `observed`, then store that reply the way
    /// [`HarnessOp::send_and_await`] would. Waits up to
    /// [`DEFAULT_POLL_BUDGET`].
    ///
    /// Reach for this only in the detached case — an effect that lands
    /// on a chain the caller never joins, so no settlement here can
    /// order it (see the rule on [`HarnessOp`]). When lineage does carry
    /// the effect, [`HarnessOp::send_mail`] is the stronger and cheaper
    /// barrier.
    ///
    /// The budget is wall clock rather than an iteration count, so the
    /// wait is invariant to how fast the box is: a starved runner takes
    /// more probes and still passes, and a genuine regression still
    /// fails within the bound. `observed` is `FnMut`, so a predicate may
    /// count its own probes or carry state out.
    ///
    /// Annotate the closure's parameter to name the reply kind:
    ///
    /// ```no_run
    /// # use aether_harness_substrate::HarnessOp;
    /// # use aether_window::{ListWindows, ListWindowsResult, WindowCapability};
    /// # use aether_actor::Addressable;
    /// # let surviving = aether_window::WindowId(0);
    /// HarnessOp::poll_until(WindowCapability::NAMESPACE, &ListWindows, move |reply: &ListWindowsResult| {
    ///     matches!(reply, ListWindowsResult::Ok { windows }
    ///         if windows.iter().map(|window| window.id).eq([surviving]))
    /// });
    /// ```
    ///
    /// On timeout the step fails with [`ExecutionError::PollTimeout`],
    /// which reports the last reply the probe actually got — a red that
    /// names the state reached, not only that the wait ran out.
    #[must_use]
    pub fn poll_until<K, R>(recipient: impl Into<String>, probe: &K, observed: impl FnMut(&R) -> bool + 'static) -> Self
    where
        K: Kind,
        R: Kind + fmt::Debug,
    {
        Self::poll_until_within(DEFAULT_POLL_BUDGET, recipient, probe, observed)
    }

    /// [`HarnessOp::poll_until`] with an explicit wall-clock budget, for
    /// a test whose observation is known to resolve far inside
    /// [`DEFAULT_POLL_BUDGET`] or needs longer than it.
    ///
    /// One probe always runs before the budget is consulted, so a zero
    /// budget is a single-shot observation rather than an immediate
    /// failure.
    #[must_use]
    pub fn poll_until_within<K, R>(
        budget: Duration,
        recipient: impl Into<String>,
        probe: &K,
        mut observed: impl FnMut(&R) -> bool + 'static,
    ) -> Self
    where
        K: Kind,
        R: Kind + fmt::Debug,
    {
        Self::PollUntil {
            recipient: recipient.into(),
            kind: K::ID,
            payload: probe.encode_into_bytes(),
            budget,
            observed_kind: R::NAME,
            observe: PollObserver(Box::new(move |bytes| {
                let Some(reply) = R::decode_from_bytes(bytes) else {
                    return Observation::Undecodable;
                };
                if observed(&reply) {
                    Observation::Satisfied
                } else {
                    Observation::Pending(format!("{reply:?}"))
                }
            })),
        }
    }
}

/// One output per executed op, keyed by the op's label in
/// [`ExecutionResult`]. `Replied` and `Captured` carry bytes; the
/// other two are unit markers confirming the op ran.
pub enum HarnessOutput {
    Advanced,
    Mailed,
    Replied(Vec<u8>),
    Captured(Vec<u8>),
}

/// Map of per-op outputs from a successful [`SubstrateHarness::execute`]
/// call, keyed by each op's label. Fetch results by label so tests
/// read by intent (`result.captured("snap")`) and survive step
/// reordering, rather than destructuring a positional array.
#[derive(Default)]
pub struct ExecutionResult {
    inner: HashMap<String, HarnessOutput>,
}

impl ExecutionResult {
    /// Whether a step with `label` ran.
    #[must_use]
    pub fn contains(&self, label: &str) -> bool {
        self.inner.contains_key(label)
    }

    /// Raw output for `label`, if the step ran.
    #[must_use]
    pub fn get(&self, label: &str) -> Option<&HarnessOutput> {
        self.inner.get(label)
    }

    /// PNG bytes from a [`HarnessOp::Capture`] step. `None` if `label`
    /// didn't run or wasn't a `Capture`.
    #[must_use]
    pub fn captured(&self, label: &str) -> Option<&[u8]> {
        match self.inner.get(label)? {
            HarnessOutput::Captured(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Decode the reply from a [`HarnessOp::SendAndAwait`] step — or the
    /// satisfying observation from a [`HarnessOp::PollUntil`] step — as
    /// `R`. `R` is any reply kind (`LoadResult`, `ReplaceResult`,
    /// `WriteResult`, …); the bytes decode through the kind's declared
    /// codec (cast or structured) via `Kind::decode_from_bytes`
    /// (ADR-0100). Errors with [`ExecutionError::NoSuchReply`] if
    /// `label` didn't run a `SendAndAwait` (or didn't run at all), or
    /// [`ExecutionError::ReplyDecode`] if the bytes don't decode as
    /// `R`.
    pub fn reply<R>(&self, label: &str) -> Result<R, ExecutionError>
    where
        R: Kind,
    {
        match self.inner.get(label) {
            Some(HarnessOutput::Replied(bytes)) => {
                R::decode_from_bytes(bytes).ok_or_else(|| ExecutionError::ReplyDecode {
                    label: label.to_owned(),
                    error: "Kind::decode_from_bytes returned None".to_owned(),
                })
            }
            _ => Err(ExecutionError::NoSuchReply(label.to_owned())),
        }
    }
}

/// Failure modes of [`SubstrateHarness::execute`] and its result accessors.
#[derive(Debug)]
pub enum ExecutionError {
    /// Two ops in the same `execute` call shared a label.
    DuplicateLabel(String),
    /// The op at `label` failed mid-sequence; `error` is the
    /// underlying [`SubstrateHarnessError`] (settlement timeout, decode
    /// failure, unknown mailbox, …). Aborts the sequence.
    OpFailed { label: String, error: SubstrateHarnessError },
    /// [`ExecutionResult::reply`] was asked for a label that didn't
    /// run a [`HarnessOp::SendAndAwait`] (or didn't run at all).
    NoSuchReply(String),
    /// [`ExecutionResult::reply`] couldn't decode the stashed reply
    /// bytes as the requested type.
    ReplyDecode { label: String, error: String },
    /// A [`HarnessOp::PollUntil`] step's observation never held within
    /// its wall-clock budget. Aborts the sequence.
    ///
    /// `observed` is the `Debug` image of the last reply the probe
    /// actually got, which is the point: a bounded wait that reports
    /// only "condition not met in 10s" is barely better than the sleep
    /// it replaced, while one that names the state reached turns the red
    /// into a diagnosis.
    PollTimeout {
        label: String,
        recipient: String,
        observed_kind: &'static str,
        budget: Duration,
        probes: u32,
        observed: String,
    },
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateLabel(label) => {
                write!(f, "duplicate step label {label:?} in execute() sequence")
            }
            Self::OpFailed { label, error } => {
                write!(f, "execute() step {label:?} failed: {error}")
            }
            Self::NoSuchReply(label) => {
                write!(f, "no SendAndAwait reply stored under label {label:?}")
            }
            Self::ReplyDecode { label, error } => {
                write!(f, "decode reply for label {label:?}: {error}")
            }
            Self::PollTimeout { label, recipient, observed_kind, budget, probes, observed } => {
                write!(
                    f,
                    "execute() step {label:?} polled {recipient:?} for {budget:?} across {probes} probes \
                     and the observation never held; last {observed_kind} seen: {observed}",
                )
            }
        }
    }
}

impl error::Error for ExecutionError {}

impl SubstrateHarness {
    /// Execute `steps` in order. Each op dispatches via the matching
    /// settlement-gated [`SubstrateHarness`] primitive, blocks until its
    /// causal chain drains (ADR-0080 §6), then proceeds. Outputs are
    /// keyed by each op's label; fetch them from the returned
    /// [`ExecutionResult`] (`captured(label)`, `reply::<R>(label)`).
    ///
    /// Labels must be unique within one call
    /// ([`ExecutionError::DuplicateLabel`]). Any op failure aborts the
    /// sequence and returns [`ExecutionError::OpFailed`] naming the
    /// failing step.
    ///
    /// `execute` composes over the per-op primitives — it does not
    /// replace them. Tests that assert intermediate state between ops
    /// stay imperative, or split into multiple `execute` calls.
    pub fn execute(&mut self, steps: Vec<(&str, HarnessOp)>) -> Result<ExecutionResult, ExecutionError> {
        let mut out = ExecutionResult::default();
        for (label, op) in steps {
            if out.contains(label) {
                return Err(ExecutionError::DuplicateLabel(label.to_owned()));
            }
            let failed = |error| ExecutionError::OpFailed { label: label.to_owned(), error };

            let output = match op {
                HarnessOp::Advance { ticks } => self.advance(ticks).map(|_| HarnessOutput::Advanced).map_err(failed),
                HarnessOp::SendMail { recipient, kind, payload } => {
                    self.send_bytes(&recipient, kind, payload).map(|()| HarnessOutput::Mailed).map_err(failed)
                }
                HarnessOp::SendAndAwait { recipient, kind, payload } => {
                    self.send_bytes_and_await(&recipient, kind, payload).map(HarnessOutput::Replied).map_err(failed)
                }
                HarnessOp::Capture => self.capture().map(HarnessOutput::Captured).map_err(failed),
                HarnessOp::CaptureWithMails { pre, after } => {
                    self.capture_with_mails(pre, after).map(HarnessOutput::Captured).map_err(failed)
                }
                HarnessOp::PollUntil { recipient, kind, payload, budget, observed_kind, observe } => self
                    .poll_until_observed(
                        PollStep { label, recipient: &recipient, kind, payload: &payload, budget, observed_kind },
                        observe,
                    ),
            }?;

            out.inner.insert(label.to_owned(), output);
        }
        Ok(out)
    }

    /// Body of the [`HarnessOp::PollUntil`] step: re-send the probe
    /// until `observe` is satisfied or `budget` elapses.
    ///
    /// The probe rides [`Self::send_bytes_and_await`] — the correlation-only
    /// wait — deliberately. This op exists precisely because the effect
    /// under observation is not on the probe's chain, so the probe needs
    /// only to fetch the current answer; what orders the wait is the
    /// repetition against a wall clock, not any barrier the probe itself
    /// carries.
    fn poll_until_observed(
        &mut self,
        step: PollStep<'_>,
        mut observe: PollObserver,
    ) -> Result<HarnessOutput, ExecutionError> {
        let PollStep { label, recipient, kind, payload, budget, observed_kind } = step;
        let start = Instant::now();
        let mut probes = 0u32;

        loop {
            let bytes = self
                .send_bytes_and_await(recipient, kind, payload.to_vec())
                .map_err(|error| ExecutionError::OpFailed { label: label.to_owned(), error })?;
            probes = probes.saturating_add(1);

            match (observe.0)(&bytes) {
                Observation::Satisfied => return Ok(HarnessOutput::Replied(bytes)),
                Observation::Undecodable => {
                    return Err(ExecutionError::ReplyDecode {
                        label: label.to_owned(),
                        error: format!(
                            "probe reply from {recipient:?} ({} bytes) does not decode as the observed kind \
                             {observed_kind}",
                            bytes.len()
                        ),
                    });
                }
                // The budget is consulted only here, so the value that
                // reaches the failure is always the one the last probe
                // actually saw.
                Observation::Pending(rendered) if start.elapsed() >= budget => {
                    return Err(ExecutionError::PollTimeout {
                        label: label.to_owned(),
                        recipient: recipient.to_owned(),
                        observed_kind,
                        budget,
                        probes,
                        observed: rendered,
                    });
                }
                Observation::Pending(_) => thread::sleep(POLL_INTERVAL),
            }
        }
    }
}

/// The non-closure half of a [`HarnessOp::PollUntil`] step, borrowed out
/// of the variant for the runner.
#[derive(Clone, Copy)]
struct PollStep<'a> {
    label: &'a str,
    recipient: &'a str,
    kind: KindId,
    payload: &'a [u8],
    budget: Duration,
    observed_kind: &'static str,
}

#[cfg(test)]
mod tests {
    use aether_window::{ListWindows, ListWindowsResult, WindowCapability};

    use super::*;

    /// Cast reply kind with a non-`f32` field — its wire image is the
    /// raw cast bytes, which a structured reader would misdecode.
    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
    struct CastReply {
        code: u32,
        flag: u16,
        _pad: u16,
    }

    impl Kind for CastReply {
        const NAME: &'static str = "test.execute_cast_reply";
        const ID: KindId = KindId(0xDEAD_BEEF_000A_0001);

        fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
            (bytes.len() == size_of::<Self>()).then(|| bytemuck::pod_read_unaligned(bytes))
        }

        fn encode_into_bytes(&self) -> Vec<u8> {
            bytemuck::bytes_of(self).to_vec()
        }
    }

    /// `PollUntil` fails with the value its last probe actually saw
    /// rather than a bare "condition not met" — the property that makes
    /// the op worth having over the sleep it replaces (issue 4196). The
    /// predicate never holds, so the run is the timeout path end to end:
    /// the budget is respected, at least one probe ran, and the recorded
    /// `ListWindowsResult` — an empty list, since nothing created a
    /// window — reaches the message.
    #[test]
    fn poll_until_timeout_reports_the_last_observed_value() {
        let mut harness = SubstrateHarness::start().expect("boot harness");
        let budget = Duration::from_millis(200);

        let never =
            HarnessOp::poll_until_within(budget, WindowCapability::NAMESPACE, &ListWindows, |_: &ListWindowsResult| {
                false
            });

        let Err(error) = harness.execute(vec![("never", never)]) else {
            panic!("an observation that never holds fails the step");
        };

        let ExecutionError::PollTimeout { label, probes, observed, .. } = &error else {
            panic!("expected a poll timeout, got {error}");
        };
        assert_eq!(label, "never");
        assert!(*probes >= 1, "the budget is checked after a probe, so one always runs");
        assert_eq!(observed, &format!("{:?}", ListWindowsResult::Ok { windows: Vec::new() }));
        assert!(
            error.to_string().contains(&format!("{:?}", ListWindowsResult::Ok { windows: Vec::new() })),
            "the rendered failure must carry the observed value: {error}",
        );
    }

    /// `PollUntil` re-probes until the observation holds and then stores
    /// *that* reply, so `ExecutionResult::reply` decodes the observation
    /// that ended the wait rather than a stale or re-read one. The
    /// predicate is satisfied only on its third probe, so a run that
    /// returned after one — or that dropped the satisfying bytes —
    /// fails here.
    #[test]
    fn poll_until_returns_the_reply_that_satisfied_the_observation() {
        let mut harness = SubstrateHarness::start().expect("boot harness");
        let mut seen = 0u32;

        let result = harness
            .execute(vec![(
                "settles",
                HarnessOp::poll_until(WindowCapability::NAMESPACE, &ListWindows, move |_: &ListWindowsResult| {
                    seen += 1;
                    seen >= 3
                }),
            )])
            .expect("an observation that comes true inside the budget succeeds");

        assert_eq!(
            result.reply::<ListWindowsResult>("settles").expect("the satisfying observation is stored"),
            ListWindowsResult::Ok { windows: Vec::new() },
        );
    }

    /// ADR-0100: the `SendAndAwait` reply accessor decodes the recorded
    /// bytes through `Kind::decode_from_bytes`, so a cast reply kind
    /// round-trips uncorrupted (its `u32` / `u16` fields survive). A
    /// structured decode would have misread the raw cast image.
    #[test]
    fn execution_result_reply_decodes_cast_kind() {
        let reply = CastReply { code: 0x0A0B_0C0D, flag: 0x1234, _pad: 0 };
        // The substrate reply path encodes via `Kind::encode_into_bytes`
        // (ADR-0100), so the recorded bytes are the cast image.
        let bytes = reply.encode_into_bytes();
        let result = ExecutionResult { inner: HashMap::from([("reply".to_owned(), HarnessOutput::Replied(bytes))]) };

        let decoded: CastReply = result.reply("reply").expect("cast reply decodes");
        assert_eq!(decoded, reply);
    }
}
