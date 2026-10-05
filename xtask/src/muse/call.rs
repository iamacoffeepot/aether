//! Call one program through the bundle driver under digest-derived attempt keys.

use aether_bloomery_kinds::{Call, CallOutcome, Head, NativeOrigin, ProgramName};
use aether_bloomery_muse::MUSE;
use aether_bloomery_program::Program;
use aether_data::{Digest, OpaqueBytes, Ref};
use anyhow::{Context, Result, anyhow, bail};

use crate::bloomery::Engine;

/// The origin every call these verbs make names; the driver keys replays by
/// it and the call key together.
const ORIGIN: &str = "xtask.muse";

/// The most attempts one command makes under successive keys before it fails.
pub(super) const MAX_ATTEMPTS: u64 = 32;

/// Ask the driver to run `P` from the bundle [`MUSE`] resolves to over the
/// stored `input`, and return the seq of the run's recorded transition.
///
/// # Errors
/// The driver refused the call, the run faulted, or the transport failed.
pub(super) fn call<P: Program>(engine: &mut Engine, input: Ref<P::Input>) -> Result<u64> {
    let (seq, _) = call_in::<P>(engine, MUSE, input)?;
    Ok(seq)
}

/// Ask the driver to run `P` from the bundle `bundle` resolves to over the
/// stored `input`, and return the seq and result digest of the run's recorded
/// transition.
///
/// A recorded transition is replayed, so a retry after a lost reply does not
/// run twice; a recorded fault is asked again under the next attempt's key, so
/// a rerun after a fault outside the input makes a fresh call.
///
/// # Errors
/// The driver refused the call, the run faulted, or the transport failed.
pub(super) fn call_in<P: Program>(
    engine: &mut Engine,
    bundle: Head<OpaqueBytes>,
    input: Ref<P::Input>,
) -> Result<(u64, Digest)> {
    let digest = input.digest();
    let name = ProgramName::new(P::NAME).map_err(|error| anyhow!("program name {:?}: {error}", P::NAME))?;
    let origin = NativeOrigin::new(ORIGIN).map_err(|error| anyhow!("origin {ORIGIN:?}: {error}"))?;
    let head = engine.read_head()?;

    let mut call = Call { program: bundle, name, input: digest, origin, key: attempt_key(digest, 0)? };
    let outcome = settle(head, |attempt| {
        call.key = attempt_key(digest, attempt)?;
        engine.call_program(&call)
    })?;

    match outcome {
        CallOutcome::Transition { seq, transition, .. } => Ok((seq, transition.result)),
        CallOutcome::Fault { seq, fault, .. } => bail!("{} faulted at seq {seq}: {:?}", P::NAME, fault.reason),
        CallOutcome::Refused { reason, .. } => bail!("the driver refused {}: {reason:?}", P::NAME),
    }
}

/// The key for `attempt`: the digest's first eight bytes as a little-endian
/// `u64`, plus the attempt number, wrapping.
///
/// # Errors
/// The digest did not hold eight bytes.
fn attempt_key(digest: Digest, attempt: u64) -> Result<u64> {
    let (bytes, _) = digest.as_bytes().split_first_chunk::<8>().context("a digest holds 32 bytes")?;
    Ok(u64::from_le_bytes(*bytes).wrapping_add(attempt))
}

/// Ask under successive attempt numbers until the outcome is not a fault
/// recorded at or below `head`, and return it.
///
/// A fault at or below the head was recorded before this command started, so
/// it is a replay; anything newer is this command's own outcome. Refusals and
/// transitions return as they are.
///
/// # Errors
/// An ask failed, or every attempt answered with a replayed fault.
pub(super) fn settle(head: u64, mut ask: impl FnMut(u64) -> Result<CallOutcome>) -> Result<CallOutcome> {
    for attempt in 0..MAX_ATTEMPTS {
        let outcome = ask(attempt)?;
        let is_replay = replayed(&outcome, head);
        if !is_replay {
            return Ok(outcome);
        }
    }
    bail!("the driver recorded {MAX_ATTEMPTS} faults for one input")
}

/// Whether `outcome` is a fault recorded at or below `head`.
fn replayed(outcome: &CallOutcome, head: u64) -> bool {
    match outcome {
        CallOutcome::Fault { seq, .. } => *seq <= head,
        CallOutcome::Transition { .. } | CallOutcome::Refused { .. } => false,
    }
}
