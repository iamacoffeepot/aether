//! Where a native actor drains — the two dispatch homes and the body they share.
//!
//! An actor's inbox has to be pumped by *something*, and the substrate offers
//! two answers:
//!
//! - `dispatcher` — `DispatcherSlot`, the pooled home. Budget-bounded
//!   `run_cycle` calls driven by the chassis worker pool, with a `SlotState`
//!   machine and a seize path because many workers can reach the same slot
//!   (ADR-0087).
//! - [`pumped`] — [`PumpedSlot`](pumped::PumpedSlot), the driver-pumped home
//!   (ADR-0160 §1). One thread, chosen by a chassis driver, drains at a pump
//!   point the driver picks. A strict subset of the pooled slot: no state
//!   machine, no actor mutex, no seize, and deliberately not `Send`.
//!
//! `dispatch` holds the per-envelope body both homes call, as free functions
//! rather than methods on either slot. That is what keeps `describe`, trace
//! hops, and `actor_cost` identical across the two homes instead of drifting
//! into two nearly-equal copies.

pub(crate) mod dispatch;
pub(crate) mod dispatcher;
pub mod pumped;
