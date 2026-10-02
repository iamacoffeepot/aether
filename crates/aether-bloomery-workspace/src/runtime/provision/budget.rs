//! The host budget: the cores and memory the actor may hand out, and what of
//! them is free.

use std::num::{NonZeroU32, NonZeroU64};

use super::cores::CoreRange;
use super::cpuset::{CpuSet, FreeCores};
use super::estimate::Amounts;
use crate::runtime::run::Allotment;

/// The configured cores and memory, the part of each no running run holds,
/// and the range each run's cores are chosen in.
#[derive(Debug, Clone)]
pub struct Budget {
    memory_bytes: NonZeroU64,
    cores: CoreRange,
    free_cores: FreeCores,
    free_memory_bytes: u64,
}

impl Budget {
    /// A budget of `cpus` and `memory_bytes`, all of it free.
    pub fn new(cpus: &CpuSet, memory_bytes: NonZeroU64) -> Self {
        Self {
            memory_bytes,
            cores: CoreRange::for_budget(cpus.count()),
            free_cores: FreeCores::all(cpus),
            free_memory_bytes: memory_bytes.get(),
        }
    }

    /// Hand out `amounts` with the cores [`CoreRange::choose`] gives a run
    /// `sharing` the free cores, pinned to the lowest-numbered free ones, or
    /// `None`, taking nothing, when the free cores or free memory fall short.
    pub fn take(&mut self, amounts: &Amounts, sharing: NonZeroU32) -> Option<Allotment> {
        let memory_bytes = amounts.memory_bytes.get();
        let short_of_memory = self.free_memory_bytes < memory_bytes;
        if short_of_memory {
            return None;
        }
        let cores = self.cores.choose(self.free_cores.count(), sharing)?;
        let cpus = self.free_cores.take_lowest(cores)?;
        self.free_memory_bytes -= memory_bytes;
        Some(Allotment { cpus, memory_bytes, deadline: amounts.deadline })
    }

    /// Return a finished run's cores and memory.
    pub fn release(&mut self, allotment: &Allotment) {
        self.free_cores.insert_all(&allotment.cpus);
        self.free_memory_bytes =
            self.free_memory_bytes.saturating_add(allotment.memory_bytes).min(self.memory_bytes.get());
    }

    /// How many cores are free.
    pub fn free_cores(&self) -> u32 {
        self.free_cores.count()
    }

    /// How much memory is free.
    pub fn free_memory_bytes(&self) -> u64 {
        self.free_memory_bytes
    }

    /// The fewest cores a run starts with.
    pub fn floor_cores(&self) -> NonZeroU32 {
        self.cores.floor()
    }
}
