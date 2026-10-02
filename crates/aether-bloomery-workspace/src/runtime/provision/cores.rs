//! How many cores a run gets (ADR-0237 decision 9, as amended 2026-10-01).
//!
//! The executor chooses, never the program. Measured on the build host, a
//! leaf-edit clippy over warm layers took 6.6 s on 32 cores alone, 6.6 s each
//! as 2 runs on 16 cores, 8.9 s each as 4 runs on 8, and 15.3 s each as 8
//! runs on 4: past 16 cores a run gains nothing, and below 8 it slows faster
//! than running more at once pays back. So a run gets 8 to 16 cores, the free
//! cores shared among the runs that want them now.

use std::num::NonZeroU32;

/// The fewest cores a run is given when the budget has that many.
pub const MIN_RUN_CORES: NonZeroU32 = NonZeroU32::new(8).expect("8 is nonzero");

/// The most cores a run is given.
pub const MAX_RUN_CORES: NonZeroU32 = NonZeroU32::new(16).expect("16 is nonzero");

/// The range a run's cores are chosen in: [`MIN_RUN_CORES`] to
/// [`MAX_RUN_CORES`], each at most the budget's core count, so a budget of
/// fewer than 8 cores gives each run all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreRange {
    floor: NonZeroU32,
    ceiling: NonZeroU32,
}

impl CoreRange {
    /// The range for a budget of `budget_cores`.
    pub fn for_budget(budget_cores: NonZeroU32) -> Self {
        Self { floor: MIN_RUN_CORES.min(budget_cores), ceiling: MAX_RUN_CORES.min(budget_cores) }
    }

    /// The fewest cores a run starts with: what a waiting run needs free.
    pub fn floor(self) -> NonZeroU32 {
        self.floor
    }

    /// The cores a run gets when `free` cores are free and `sharing` runs,
    /// itself included, want them now: the free cores split evenly, clamped
    /// to the range. `None` when fewer than the floor are free.
    pub fn choose(self, free: u32, sharing: NonZeroU32) -> Option<NonZeroU32> {
        let share = NonZeroU32::new(free / sharing).unwrap_or(NonZeroU32::MIN);
        let cores = share.clamp(self.floor, self.ceiling);
        let enough_free = cores.get() <= free;
        enough_free.then_some(cores)
    }
}
