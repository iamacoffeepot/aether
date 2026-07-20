//! ADR-0071 Phase 2A: driver-capability traits + chassis builder
//! type-state.
//!
//! Sibling to ADR-0070's [`NativeActor`](crate::actor::native::NativeActor)
//! family (post-issue-525-Phase-2:
//! one struct per cap, `Drop` replaces `RunningCapability::shutdown`).
//! A chassis composes passive capabilities (dispatcher-thread sinks
//! per ADR-0070) plus exactly one [`DriverCapability`] that owns the
//! chassis main thread. The type-state [`Builder`] enforces "exactly
//! one driver" structurally; embedders that drive manually (`SubstrateHarness`,
//! future embedded harnesses) build a [`PassiveChassis`] via the
//! no-driver path.
//!
//! # Phase 2A scope
//!
//! - Trait family + builder + ctx wiring.
//! - [`Chassis::Driver`](crate::chassis::Chassis::Driver) /
//!   [`Chassis::Env`](crate::chassis::Chassis::Env) /
//!   [`Chassis::build`](crate::chassis::Chassis::build) are
//!   not yet on the [`Chassis`](crate::chassis::Chassis) trait — they land
//!   alongside the first real driver extraction (phase 3) so every
//!   chassis can nominate a real driver type rather than a stub.

mod boot_passives;
mod built;
mod driver;
mod native_actor_boot;
mod passive_boot;
mod state;

pub use built::{BuiltChassis, PassiveChassis};
pub use driver::{DriverCapability, DriverCtx, DriverRunning, NeverDriver, NeverDriverRunning, RunError};
pub use state::{Builder, BuilderState, HasDriver, NoDriver};

#[cfg(test)]
// Chassis-level integration tests stage many caps, sender threads,
// and assertions in a single test function so the boot-and-route
// sequence reads top-to-bottom; extracting helpers would either lose
// the staging context or add fixtures that aren't reused elsewhere.
#[allow(clippy::too_many_lines)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests;
