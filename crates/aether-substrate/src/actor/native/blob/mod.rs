//! The cursor-shared cooperative fan-out (ADR-0087).
//!
//! A handler's outbound mail is grouped by recipient into one shared blob and
//! scheduled once; many workers then drain it cooperatively, each claiming a
//! whole recipient-group and dispatching that group's mail in place. A wide or
//! heavy fan-out parallelises across the pool instead of serialising on the
//! worker that happened to demux it.
//!
//! - [`work`] — [`BlobWork`](work::BlobWork), the work unit itself: grouping,
//!   claiming, seizing a recipient, and the broadcast recruitment.
//! - [`lifecycle`] — the packed `AtomicU64` the workers coordinate through.
//!   Cursor, published length, completed count, and the seal bit share one word
//!   so a claim is a single CAS.
//!
//! Split along the line between the protocol and the state it runs over: the
//! word's bit layout and its single-writer / many-claimer rules are worth
//! reading without the dispatch machinery around them.

pub mod lifecycle;
pub mod work;
