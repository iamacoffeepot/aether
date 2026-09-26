//! The host budget: the cores and memory the actor may hand out, and what of
//! them is free.

use std::num::NonZeroU64;

use super::cpuset::{CpuSet, FreeCores};
use super::estimate::Amounts;
use crate::runtime::run::Allotment;

/// The configured cores and memory, and the part of each no running run
/// holds.
#[derive(Debug, Clone)]
pub struct Budget {
    memory_bytes: NonZeroU64,
    free_cores: FreeCores,
    free_memory_bytes: u64,
}

impl Budget {
    /// A budget of `cpus` and `memory_bytes`, all of it free.
    pub fn new(cpus: &CpuSet, memory_bytes: NonZeroU64) -> Self {
        Self { memory_bytes, free_cores: FreeCores::all(cpus), free_memory_bytes: memory_bytes.get() }
    }

    /// Hand out `amounts` pinned to the lowest-numbered free cores, or
    /// `None`, taking nothing, when the free cores or free memory fall short.
    pub fn take(&mut self, amounts: &Amounts) -> Option<Allotment> {
        let memory_bytes = amounts.memory_bytes.get();
        if self.free_memory_bytes < memory_bytes {
            return None;
        }
        let cpus = self.free_cores.take_lowest(amounts.cores)?;
        self.free_memory_bytes -= memory_bytes;
        Some(Allotment { cpus, memory_bytes, deadline: amounts.deadline })
    }

    /// Return a finished run's cores and memory.
    pub fn release(&mut self, allotment: &Allotment) {
        self.free_cores.insert_all(&allotment.cpus);
        self.free_memory_bytes =
            self.free_memory_bytes.saturating_add(allotment.memory_bytes).min(self.memory_bytes.get());
    }
}
