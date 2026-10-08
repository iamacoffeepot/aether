//! The guest's linear memory, counted on the engine's memory ledger.
//!
//! wasmtime asks a store's resource limiter before every memory growth,
//! including the growth from nothing to a memory's initial size at
//! instantiate. [`MemoryMeter`] is that limiter: it allows everything and
//! sets its gauge to the size being grown to, so a `memory.grow` pays one
//! relaxed atomic store and mail dispatch pays nothing.

use wasmtime::ResourceLimiter;

use crate::actor::native::binding::NativeBinding;
use crate::memory::MemoryGauge;

/// What the gauge's bytes are, in the engine's memory report.
const LABEL: &str = "linear memory";

/// One guest instance's linear-memory gauge and the limiter that keeps it. A
/// guest and its inline children share one linear memory and so one meter.
/// The row leaves the report when the instance's store drops.
pub(super) struct MemoryMeter {
    gauge: MemoryGauge,
    /// The size the growth in progress started from, restored when wasmtime
    /// reports that growth failed: the limiter is asked before the memory's
    /// own maximum is checked, so an allowed growth can still not happen.
    before_growth: usize,
}

impl MemoryMeter {
    /// A meter at zero, listed under `binding`'s actor.
    pub(super) fn new(binding: &NativeBinding) -> Self {
        Self { gauge: binding.memory_gauge(LABEL), before_growth: 0 }
    }

    /// Set the gauge to `bytes`, the memory's size as read from the store.
    pub(super) fn seed(&self, bytes: usize) {
        self.gauge.set(bytes);
    }
}

impl ResourceLimiter for MemoryMeter {
    fn memory_growing(&mut self, current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        self.before_growth = current;
        self.gauge.set(desired);
        Ok(true)
    }

    fn memory_grow_failed(&mut self, _error: wasmtime::Error) -> wasmtime::Result<()> {
        self.gauge.set(self.before_growth);
        Ok(())
    }

    fn table_growing(&mut self, _current: usize, _desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}
