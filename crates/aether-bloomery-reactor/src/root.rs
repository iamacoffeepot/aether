//! Digest-loaded reactor root: one owner, contiguous Warm/Event, replies to the caller.

use alloc::format;
use alloc::vec::Vec;
use core::slice;

use aether_bloomery_kinds::{Detail, Entry, Evaluated, Event, JournalEntry, Status, Warm, Warmed};

use crate::error::PrepareError;
use crate::owner::Owner;
use crate::reactors::ReactorList;

enum Health {
    Healthy,
    Poisoned { last_trusted: u64, reason: Detail },
}

/// One shared views owner answering [`Warm`], [`Event`], and [`aether_bloomery_kinds::StatusQuery`].
pub struct Root<L: ReactorList> {
    owner: Owner,
    health: Health,
    names: L::Names,
}

impl<L: ReactorList> Root<L> {
    /// Convert every reactor and rule name, keep the converted names, then start empty and healthy.
    ///
    /// # Errors
    ///
    /// [`Detail`] when a name is not a valid dotted name. The generated actor
    /// refuses `init` on this error; `#[reactor]` const assertions make it
    /// unreachable for authored reactors.
    pub fn new() -> Result<Self, Detail> {
        Ok(Self { owner: Owner::new(), health: Health::Healthy, names: L::names()? })
    }

    /// Cursor and poison flag.
    #[must_use]
    pub fn status(&self) -> Status {
        match &self.health {
            Health::Healthy => Status::new(self.owner.cursor().0, false),
            Health::Poisoned { last_trusted, .. } => Status::new(*last_trusted, true),
        }
    }

    /// Fold-only warmup. Does not evaluate rules.
    pub fn warm(&mut self, warm: Warm) -> Warmed {
        if let Health::Poisoned { last_trusted, reason } = &self.health {
            return Warmed::Poisoned { last_trusted: *last_trusted, reason: reason.clone() };
        }
        let entries = warm.into_entries();
        let first = entries.first();
        let expected = self.owner.cursor().0.saturating_add(1);
        if first != expected {
            return Warmed::OutOfSequence { first, expected };
        }
        let through = entries.last();
        let last_trusted = self.owner.cursor().0;
        let stored: Vec<_> = entries.into_vec().iter().map(JournalEntry::to_entry).collect();
        if let Err(error) = self.push_and_fold(&stored, last_trusted) {
            return Warmed::Poisoned { last_trusted, reason: fold_reason(&error) };
        }
        Warmed::Folded { through }
    }

    /// Live event: fold, then evaluate every reactor.
    pub fn event(&mut self, event: Event) -> Evaluated {
        let entry = event.into_entry();
        let seq = entry.seq;
        if let Health::Poisoned { last_trusted, reason } = &self.health {
            return Evaluated::Poisoned { seq, last_trusted: *last_trusted, reason: reason.clone() };
        }
        let expected = self.owner.cursor().0.saturating_add(1);
        if seq != expected {
            return Evaluated::OutOfSequence { seq, expected };
        }
        let last_trusted = self.owner.cursor().0;
        let stored = entry.to_entry();
        if let Err(error) = self.push_and_fold(slice::from_ref(&stored), last_trusted) {
            return Evaluated::Poisoned { seq, last_trusted, reason: fold_reason(&error) };
        }
        match L::evaluate_all(&mut self.owner, &self.names) {
            Ok(intents) => Evaluated::Completed { seq, intents },
            Err(fail) => Evaluated::Failed { seq, reactor: fail.reactor, reason: fail.reason },
        }
    }

    /// Push then fold; any failure poisons the root, so a `Poisoned` reply always matches its state.
    fn push_and_fold(&mut self, entries: &[Entry], last_trusted: u64) -> Result<(), PrepareError> {
        let result = self.owner.push(entries).and_then(|()| L::warm_all(&mut self.owner));
        if let Err(error) = &result {
            self.health = Health::Poisoned { last_trusted, reason: fold_reason(error) };
        }
        result
    }
}

fn fold_reason(error: &PrepareError) -> Detail {
    Detail::new(format!("{error}"))
}
