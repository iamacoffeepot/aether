//! Digest-loaded reactor root: one owner, contiguous Warm/Event, replies to the caller.

use alloc::vec::Vec;
use alloc::{format, vec};

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
        let stored = entries.into_vec().iter().map(JournalEntry::to_entry).collect();
        if let Err(error) = self.push_and_fold(stored, last_trusted) {
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
        if let Err(error) = self.push_and_fold(vec![entry.to_entry()], last_trusted) {
            return Evaluated::Poisoned { seq, last_trusted, reason: fold_reason(&error) };
        }
        match L::evaluate_all(&mut self.owner, &self.names) {
            Ok(intents) => Evaluated::Completed { seq, intents },
            Err(fail) => Evaluated::Failed { seq, reactor: fail.reactor, reason: fail.reason },
        }
    }

    /// Push then fold; any failure poisons the root, so a `Poisoned` reply always matches its state.
    /// A successful fold releases every entry but the trigger: `warm_all` has built and folded every
    /// view a rule names, so nothing reads older entries again.
    fn push_and_fold(&mut self, entries: Vec<Entry>, last_trusted: u64) -> Result<(), PrepareError> {
        let result = self.owner.push_owned(entries).and_then(|()| L::warm_all(&mut self.owner));
        match &result {
            Ok(()) => self.owner.release_folded(),
            Err(error) => self.health = Health::Poisoned { last_trusted, reason: fold_reason(error) },
        }
        result
    }
}

fn fold_reason(error: &PrepareError) -> Detail {
    Detail::new(format!("{error}"))
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use aether_bloomery_kinds::{Evaluated, Event, JournalEntry, Warm, WarmEntries, Warmed};
    use aether_data::KindId;

    use super::Root;
    use crate::params::Nil;

    fn entry(seq: u64) -> JournalEntry {
        JournalEntry { seq, kind: KindId(1), cause: None, recorded_at_millis: 0, bytes: Vec::new() }
    }

    #[test]
    fn a_warmed_root_retains_only_its_trigger() {
        // Catches a root that keeps every entry it was sent after folding it.
        let mut root = Root::<Nil>::new().expect("names");
        let warm = Warm::new(WarmEntries::new((1..=300).map(entry).collect()).expect("dense"));
        let warmed = root.warm(warm);
        assert!(matches!(warmed, Warmed::Folded { through: 300 }), "{warmed:?}");

        let evaluated = root.event(Event::new(entry(301)));
        assert!(matches!(evaluated, Evaluated::Completed { seq: 301, .. }), "{evaluated:?}");
        assert_eq!(root.owner.retained(), 1);
        assert_eq!(root.status().cursor(), 301);
    }
}
