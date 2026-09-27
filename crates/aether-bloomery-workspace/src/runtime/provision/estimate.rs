//! Per-key estimates and the amounts they give a run (ADR-0237 decision 9).
//!
//! The table lives only in the actor's memory: it is never written to the
//! journal or placed in a result, a restart empties it, and it has no
//! eviction, since it holds one small entry per distinct run key.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

use super::key::RunKey;
use crate::runtime::run::{Allotment, Observed};
use crate::{Resource, RunResult};

/// The least memory a seen key's allotment gets: 256 MiB.
pub const MIN_MEMORY_BYTES: u64 = 256 << 20;

/// The shortest deadline a seen key's allotment gets.
pub const MIN_DEADLINE: Duration = Duration::from_mins(1);

/// A later observation's weight in the blend is one part in this many: an
/// EWMA with weight 1/4.
const BLEND_PARTS: u128 = 4;

/// The factor exhaustion grows the resource that ran out by.
const GROWTH: u128 = 2;

/// A percent of at least 100 that scales an observation into an allotment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headroom(u32);

impl Headroom {
    /// `percent` as headroom.
    ///
    /// # Errors
    ///
    /// [`HeadroomError`] when `percent` is below 100, which would hand a run
    /// less than it was seen to use.
    pub const fn new(percent: u32) -> Result<Self, HeadroomError> {
        if percent < 100 {
            Err(HeadroomError)
        } else {
            Ok(Self(percent))
        }
    }

    /// `value` scaled by the headroom.
    fn scale(self, value: u128) -> u128 {
        value.saturating_mul(u128::from(self.0)) / 100
    }
}

/// A headroom below 100 percent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadroomError;

impl fmt::Display for HeadroomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the headroom must be at least 100 percent")
    }
}

impl Error for HeadroomError {}

/// What a run is given before cores are pinned: how many cores, each step's
/// memory, and the deadline its steps share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Amounts {
    pub cores: NonZeroU32,
    pub memory_bytes: NonZeroU64,
    pub deadline: Duration,
}

impl Amounts {
    /// Each amount at most `ceiling`'s.
    fn clamped(self, ceiling: &Self) -> Self {
        Self {
            cores: self.cores.min(ceiling.cores),
            memory_bytes: self.memory_bytes.min(ceiling.memory_bytes),
            deadline: self.deadline.min(ceiling.deadline),
        }
    }
}

/// One key's next allotment before the floor and the clamp, per resource:
/// the blend of its observations times the headroom, or twice what ran out.
/// Held in allotment units so doubling stays exact; since the headroom is a
/// constant factor, blending scaled observations is blending observations,
/// then scaling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Estimate {
    memory_bytes: Option<u128>,
    wall_nanos: Option<u128>,
}

/// The estimate table and the rules that turn it into [`Amounts`].
#[derive(Debug, Clone)]
pub struct Estimates {
    table: HashMap<RunKey, Estimate>,
    /// A key never seen gets these, already clamped to `ceiling`.
    defaults: Amounts,
    headroom: Headroom,
    /// No run gets more: the budget's cores and memory, and the longest
    /// deadline.
    ceiling: Amounts,
}

impl Estimates {
    /// An empty table whose unseen keys get `defaults`, clamped to `ceiling`.
    pub fn new(defaults: Amounts, headroom: Headroom, ceiling: Amounts) -> Self {
        Self { table: HashMap::new(), defaults: defaults.clamped(&ceiling), headroom, ceiling }
    }

    /// What a run under `key` is given now. An unseen resource takes the
    /// default; a seen one its estimate, floored at [`MIN_MEMORY_BYTES`] and
    /// [`MIN_DEADLINE`]; every amount is then clamped to the ceiling, so it
    /// always fits the whole budget. Cores are always the default.
    pub fn amounts(&self, key: &RunKey) -> Amounts {
        let Some(estimate) = self.table.get(key) else {
            return self.defaults;
        };
        let memory_bytes = estimate.memory_bytes.map_or(self.defaults.memory_bytes, |bytes| {
            NonZeroU64::new(saturating_u64(bytes).max(MIN_MEMORY_BYTES)).unwrap_or(self.defaults.memory_bytes)
        });
        let deadline = estimate
            .wall_nanos
            .map_or(self.defaults.deadline, |nanos| Duration::from_nanos(saturating_u64(nanos)).max(MIN_DEADLINE));
        Amounts { cores: self.defaults.cores, memory_bytes, deadline }.clamped(&self.ceiling)
    }

    /// Learn from a finished run given `allotment`.
    ///
    /// - `Ok`, whatever its exit codes: each observed peak memory and wall
    ///   time, scaled by the headroom, seeds the key's estimate the first
    ///   time and blends into it at weight 1/4 after.
    /// - `Exhausted(Memory)`: the next allotment's memory is twice what ran
    ///   out, clamped to the budget.
    /// - `Exhausted(Time)`: the next deadline is twice the one that passed,
    ///   clamped to the longest deadline.
    /// - `Refused` and `Failed` change nothing.
    pub fn observe(&mut self, key: &RunKey, allotment: &Allotment, result: &RunResult, observed: &Observed) {
        match *result {
            RunResult::Ok(_) => {
                let memory = observed.peak_memory_bytes.map(|bytes| self.headroom.scale(u128::from(bytes)));
                let wall = observed.wall.map(|wall| self.headroom.scale(wall.as_nanos()));
                if memory.is_none() && wall.is_none() {
                    return;
                }
                let estimate = self.table.entry(*key).or_default();
                blend(&mut estimate.memory_bytes, memory);
                blend(&mut estimate.wall_nanos, wall);
            }
            RunResult::Exhausted(Resource::Memory) => {
                let grown = u128::from(allotment.memory_bytes).saturating_mul(GROWTH);
                let ceiling = u128::from(self.ceiling.memory_bytes.get());
                self.table.entry(*key).or_default().memory_bytes = Some(grown.min(ceiling));
            }
            RunResult::Exhausted(Resource::Time) => {
                let grown = allotment.deadline.as_nanos().saturating_mul(GROWTH);
                let ceiling = self.ceiling.deadline.as_nanos();
                self.table.entry(*key).or_default().wall_nanos = Some(grown.min(ceiling));
            }
            RunResult::Refused(_) | RunResult::Failed { .. } => {}
        }
    }
}

/// Seed `slot` with `observation`, or blend it in at weight 1/4.
fn blend(slot: &mut Option<u128>, observation: Option<u128>) {
    let Some(observation) = observation else {
        return;
    };
    *slot = Some(slot.map_or(observation, |current| {
        (current.saturating_mul(BLEND_PARTS - 1).saturating_add(observation)) / BLEND_PARTS
    }));
}

fn saturating_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
