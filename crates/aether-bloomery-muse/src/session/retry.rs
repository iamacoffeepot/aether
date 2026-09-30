//! How the loop retries a turn the vendor refused as transient (ADR-0234):
//! it waits on the driver's clock (ADR-0245), then sends the same stored turn
//! input again.
//!
//! The wait is due `Retry-After` seconds after the refused turn was recorded
//! when the vendor sent one, and otherwise after a backoff that doubles with
//! each retry of the same turn, spread by the turn's seq so sessions refused
//! together do not retry together. A turn is retried at most [`MAX_RETRIES`]
//! times.

use aether_bloomery_kinds::{CLOCK, CallInput, CallProgram, EncodedArtifact, Until};
use aether_bloomery_program::{At, ClockUntil, wait, wait_spread};

use crate::session::tools::program_name;

/// The most times the loop retries one turn: four attempts in all.
pub const MAX_RETRIES: u32 = 3;

/// The first retry's wait when the vendor sent no `Retry-After`, doubled for
/// each retry after it.
const BACKOFF_MILLIS: u64 = 1_000;

/// The width of the deterministic offset added to a backoff.
const SPREAD_MILLIS: u64 = 1_000;

/// When to retry the turn recorded at `at`: `retry_after_secs` after it when
/// the vendor sent a delay, and otherwise a backoff doubled for each of the
/// `retries` already made for the turn.
pub fn retry_wait(at: At, retry_after_secs: Option<u32>, retries: u32) -> Until {
    retry_after_secs.map_or_else(
        || wait_spread(at, BACKOFF_MILLIS << retries, SPREAD_MILLIS),
        |secs| wait(at, u64::from(secs) * 1_000),
    )
}

/// The request for the driver's clock to fire at `until`.
pub fn wait_call(until: Until) -> Option<CallProgram> {
    Some(CallProgram {
        program: CLOCK,
        name: program_name::<ClockUntil>(),
        input: CallInput::Value(EncodedArtifact::new(&until).ok()?),
    })
}
