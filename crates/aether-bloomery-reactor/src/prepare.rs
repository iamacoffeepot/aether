//! One-shot preparation and the owned pair result.

use aether_bloomery_kinds::Entry;

use crate::direct::Direct;
use crate::error::PrepareError;
use crate::guard::Guard;
use crate::owner::Owner;
use crate::params::{GuardArg, ViewArg};
use crate::trigger::Trigger;

/// Owned trigger, direct view, and named guard at one prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared<T, D, G> {
    trigger: T,
    direct: D,
    guard: G,
}

impl<T, D, G> Prepared<T, D, G> {
    /// The decoded trigger entry.
    #[must_use]
    pub const fn trigger(&self) -> &T {
        &self.trigger
    }

    /// Direct view parameter, owned at this prefix.
    #[must_use]
    pub const fn direct(&self) -> &D {
        &self.direct
    }

    /// Named guard, owned at this prefix.
    #[must_use]
    pub const fn guard(&self) -> &G {
        &self.guard
    }

    /// Split into owned parts.
    #[must_use]
    pub fn into_parts(self) -> (T, D, G) {
        (self.trigger, self.direct, self.guard)
    }
}

impl Owner {
    /// Convenience for one direct published view and one named guard.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] when the trigger, catch-up, or a poisoned view fails.
    pub fn prepare_pair<T, D, G>(&mut self) -> Result<Option<Prepared<T, D, G>>, PrepareError>
    where
        T: Trigger,
        D: Direct,
        G: Guard<T>,
    {
        match self.prepare::<T, ViewArg<D, GuardArg<G>>>()? {
            None => Ok(None),
            Some((trigger, (direct, (guard, ())))) => Ok(Some(Prepared { trigger, direct, guard })),
        }
    }
}

/// One-shot helper: push `entries` into a fresh [`Owner`] and prepare a pair.
///
/// Retained catch-up across calls needs [`Owner`]. [`None`] means the guard
/// declined.
///
/// # Errors
///
/// [`PrepareError`] when the prefix, trigger, or a view fold fails.
pub fn prepare<T, D, G>(entries: &[Entry]) -> Result<Option<Prepared<T, D, G>>, PrepareError>
where
    T: Trigger,
    D: Direct,
    G: Guard<T>,
{
    let mut owner = Owner::new();
    owner.push(entries)?;
    owner.prepare_pair()
}
