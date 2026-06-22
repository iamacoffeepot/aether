//! Scheduled-batch support (ADR-0104). The synth's pending min-heap entry
//! and the millis-to-frames conversion that places each event on the
//! sample clock, plus the batch validation caps.

use std::cmp::Ordering;

use aether_data::MailboxId;

use super::kinds::ScheduledNote;

/// Maximum note events one `aether.audio.schedule` batch may carry
/// (ADR-0104). A batch crosses the event queue as a single slot, so
/// this bounds the synth's pending heap rather than the queue; 8192
/// events is several minutes of a dense melody (note-on + note-off per
/// note). An over-cap batch rejects atomically in the handler reply.
pub const SCHEDULE_MAX_EVENTS: usize = 8192;

/// Furthest future a scheduled event may be parked, in milliseconds
/// (ADR-0104). The horizon bounds how much future a sender can hold in
/// the pending heap; ten minutes is generous for a tune dispatched in
/// one call. An over-horizon `at_millis` rejects the whole batch.
pub const SCHEDULE_MAX_MILLIS: u32 = 600_000;

/// One pending scheduled note in the synth's min-heap (ADR-0104).
/// Ordered by `(due_frame, seq)` only — `seq` is a monotonic stamp in
/// batch-arrival order, so events that fall on the same frame fire in
/// the order they were sent. The note payload takes no part in
/// ordering, which is why the ordering impls are hand-written rather
/// than derived (`ScheduledNote` is not `Ord`).
pub struct ScheduledEntry {
    pub due_frame: u64,
    pub seq: u64,
    pub sender_mailbox: MailboxId,
    pub note: ScheduledNote,
}

impl PartialEq for ScheduledEntry {
    fn eq(&self, other: &Self) -> bool {
        (self.due_frame, self.seq) == (other.due_frame, other.seq)
    }
}

impl Eq for ScheduledEntry {}

impl PartialOrd for ScheduledEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.due_frame, self.seq).cmp(&(other.due_frame, other.seq))
    }
}

/// Convert a play-at offset in milliseconds to a frame count at the
/// device rate (ADR-0104). Added to the frame clock at receipt to land
/// the absolute due frame.
pub fn millis_to_frames(at_millis: u32, sample_rate: f32) -> u64 {
    // `at_millis` is bounded by `SCHEDULE_MAX_MILLIS` and the device
    // rate is a small positive integer, so the product is well within
    // u64 and non-negative.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let frames = (f64::from(at_millis) / 1000.0 * f64::from(sample_rate)) as u64;
    frames
}
