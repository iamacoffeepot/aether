//! Digest-loaded reactor root: one owner, contiguous Warm/Event, and one live fold continuation.

use alloc::boxed::Box;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use core::mem;

use aether_bloomery_kinds::{Detail, Entry, Evaluated, Event, JournalEntry, ReadArtifactResult, Status, Warm, Warmed};
use aether_bloomery_view::{ArtifactResolver, PendingArtifact};

use crate::error::PrepareError;
use crate::owner::Owner;
use crate::reactors::ReactorList;
use crate::resolution::{Resolution, ResolutionPoll};

#[derive(Clone, Copy)]
enum Mode {
    Warm { through: u64 },
    Event { seq: u64 },
}

struct FoldFinished {
    owner: Owner,
    result: Result<(), PrepareError>,
}

enum OperationState {
    Idle(Owner),
    Active { last_trusted: u64, mode: Mode, resolution: Resolution<FoldFinished> },
    Poisoned { last_trusted: u64, reason: Detail },
    Starting { last_trusted: u64 },
}

/// Completed typed reply from a root operation.
pub enum Completion {
    /// Reply to [`Warm`].
    Warmed(Warmed),
    /// Reply to [`Event`].
    Evaluated(Evaluated),
}

/// One step while driving a root operation.
pub enum RootPoll {
    /// The held request can be answered.
    Complete(Completion),
    /// Send the existing `ReadArtifact` to the operation's driver and resume
    /// only after its correlated result arrives.
    NeedArtifact(PendingArtifact),
}

/// One shared views owner answering [`Warm`], [`Event`], and [`aether_bloomery_kinds::StatusQuery`].
pub struct Root<L: ReactorList> {
    state: OperationState,
    names: L::Names,
}

impl<L: ReactorList> Root<L> {
    /// Convert every reactor and rule name, then start empty and healthy.
    ///
    /// # Errors
    ///
    /// [`Detail`] when a name is not a valid dotted name.
    pub fn new() -> Result<Self, Detail> {
        Ok(Self { state: OperationState::Idle(Owner::new()), names: L::names()? })
    }

    /// Last fully folded prefix and poison flag. An active operation keeps
    /// advertising its frozen pre-operation boundary until completion.
    #[must_use]
    pub fn status(&self) -> Status {
        match &self.state {
            OperationState::Idle(owner) => Status::new(owner.cursor().0, false),
            OperationState::Active { last_trusted, .. } | OperationState::Starting { last_trusted } => {
                Status::new(*last_trusted, false)
            }
            OperationState::Poisoned { last_trusted, .. } => Status::new(*last_trusted, true),
        }
    }

    /// Begin fold-only warmup. The caller holds its typed reply while this
    /// returns [`RootPoll::NeedArtifact`].
    pub fn start_warm(&mut self, warm: Warm) -> RootPoll {
        let entries = warm.into_entries();
        let first = entries.first();
        let through = entries.last();
        let expected = self.last_trusted().saturating_add(1);
        match &self.state {
            OperationState::Poisoned { last_trusted, reason } => {
                return RootPoll::Complete(Completion::Warmed(Warmed::Poisoned {
                    last_trusted: *last_trusted,
                    reason: reason.clone(),
                }));
            }
            OperationState::Active { .. } | OperationState::Starting { .. } => {
                // Existing mail has no Busy reply. Preserve the live operation
                // and use its frozen prefix as the protocol rejection boundary.
                return RootPoll::Complete(Completion::Warmed(Warmed::OutOfSequence { first, expected }));
            }
            OperationState::Idle(_) => {}
        }
        if first != expected {
            return RootPoll::Complete(Completion::Warmed(Warmed::OutOfSequence { first, expected }));
        }
        let stored = entries.into_vec().iter().map(JournalEntry::to_entry).collect();
        self.start(Mode::Warm { through }, stored)
    }

    /// Begin one live fold and evaluation.
    pub fn start_event(&mut self, event: Event) -> RootPoll {
        let entry = event.into_entry();
        let seq = entry.seq;
        let expected = self.last_trusted().saturating_add(1);
        match &self.state {
            OperationState::Poisoned { last_trusted, reason } => {
                return RootPoll::Complete(Completion::Evaluated(Evaluated::Poisoned {
                    seq,
                    last_trusted: *last_trusted,
                    reason: reason.clone(),
                }));
            }
            OperationState::Active { .. } | OperationState::Starting { .. } => {
                return RootPoll::Complete(Completion::Evaluated(Evaluated::OutOfSequence { seq, expected }));
            }
            OperationState::Idle(_) => {}
        }
        if seq != expected {
            return RootPoll::Complete(Completion::Evaluated(Evaluated::OutOfSequence { seq, expected }));
        }
        self.start(Mode::Event { seq }, vec![entry.to_entry()])
    }

    /// Apply one correlated artifact result and resume the same live future.
    #[must_use]
    pub fn fulfill(&mut self, result: ReadArtifactResult) -> Option<RootPoll> {
        let OperationState::Active { resolution, .. } = &self.state else {
            return None;
        };
        resolution.fulfill(result);
        Some(self.drive())
    }

    /// Synchronous compatibility helper for roots whose views never request artifacts.
    pub fn warm(&mut self, warm: Warm) -> Warmed {
        match self.start_warm(warm) {
            RootPoll::Complete(Completion::Warmed(warmed)) => warmed,
            RootPoll::Complete(Completion::Evaluated(_)) => unreachable!(),
            RootPoll::NeedArtifact(pending) => {
                let reason = Detail::new(format!("view fold needs artifact {}", pending.digest));
                let (mode, last_trusted) = self.take_active();
                self.state = OperationState::Poisoned { last_trusted, reason: reason.clone() };
                let RootPoll::Complete(Completion::Warmed(warmed)) =
                    Self::poisoned_completion(mode, last_trusted, reason)
                else {
                    unreachable!()
                };
                warmed
            }
        }
    }

    /// Synchronous compatibility helper for roots whose views never request artifacts.
    pub fn event(&mut self, event: Event) -> Evaluated {
        match self.start_event(event) {
            RootPoll::Complete(Completion::Evaluated(evaluated)) => evaluated,
            RootPoll::Complete(Completion::Warmed(_)) => unreachable!(),
            RootPoll::NeedArtifact(pending) => {
                let reason = Detail::new(format!("view fold needs artifact {}", pending.digest));
                let (mode, last_trusted) = self.take_active();
                self.state = OperationState::Poisoned { last_trusted, reason: reason.clone() };
                let RootPoll::Complete(Completion::Evaluated(evaluated)) =
                    Self::poisoned_completion(mode, last_trusted, reason)
                else {
                    unreachable!()
                };
                evaluated
            }
        }
    }

    fn start(&mut self, mode: Mode, entries: Vec<Entry>) -> RootPoll {
        let last_trusted = self.last_trusted();
        let previous = mem::replace(&mut self.state, OperationState::Starting { last_trusted });
        let OperationState::Idle(mut owner) = previous else {
            unreachable!("start is called only from idle")
        };
        let (mut artifacts, driver) = ArtifactResolver::operation();
        let fold_driver = driver.clone();
        let future = Box::pin(async move {
            if let Err(error) = owner.push_owned(entries) {
                return FoldFinished { owner, result: Err(error) };
            }
            let mut result = L::warm_all(&mut owner, &mut artifacts).await;
            if result.is_ok()
                && let Some(error) = fold_driver.finish()
            {
                result = Err(PrepareError::Resolve(error));
            }
            if result.is_ok() {
                owner.release_folded();
            }
            FoldFinished { owner, result }
        });
        self.state = OperationState::Active { last_trusted, mode, resolution: Resolution::new(driver, future) };
        self.drive()
    }

    fn drive(&mut self) -> RootPoll {
        let polled = match &mut self.state {
            OperationState::Active { resolution, .. } => resolution.poll(),
            OperationState::Idle(_) | OperationState::Poisoned { .. } | OperationState::Starting { .. } => {
                unreachable!("only an active operation is driven")
            }
        };
        match polled {
            ResolutionPoll::NeedArtifact(pending) => RootPoll::NeedArtifact(pending),
            ResolutionPoll::Stalled(error) => {
                let (mode, last_trusted) = self.take_active();
                let reason = error.map_or_else(
                    || Detail::new("view fold suspended without requesting an artifact"),
                    |error| fold_reason(&PrepareError::Resolve(error)),
                );
                self.state = OperationState::Poisoned { last_trusted, reason: reason.clone() };
                Self::poisoned_completion(mode, last_trusted, reason)
            }
            ResolutionPoll::Finished(FoldFinished { mut owner, result }) => {
                let (mode, last_trusted) = self.take_active();
                if let Err(error) = result {
                    let reason = fold_reason(&error);
                    self.state = OperationState::Poisoned { last_trusted, reason: reason.clone() };
                    Self::poisoned_completion(mode, last_trusted, reason)
                } else {
                    let completion = match mode {
                        Mode::Warm { through } => Completion::Warmed(Warmed::Folded { through }),
                        Mode::Event { seq } => match L::evaluate_all(&mut owner, &self.names) {
                            Ok(intents) => Completion::Evaluated(Evaluated::Completed { seq, intents }),
                            Err(fail) => Completion::Evaluated(Evaluated::Failed {
                                seq,
                                reactor: fail.reactor,
                                reason: fail.reason,
                            }),
                        },
                    };
                    self.state = OperationState::Idle(owner);
                    RootPoll::Complete(completion)
                }
            }
        }
    }

    fn take_active(&mut self) -> (Mode, u64) {
        let last_trusted = self.last_trusted();
        let previous = mem::replace(&mut self.state, OperationState::Starting { last_trusted });
        let OperationState::Active { last_trusted, mode, .. } = previous else {
            unreachable!("completion belongs to an active operation")
        };
        (mode, last_trusted)
    }

    fn poisoned_completion(mode: Mode, last_trusted: u64, reason: Detail) -> RootPoll {
        match mode {
            Mode::Warm { .. } => RootPoll::Complete(Completion::Warmed(Warmed::Poisoned { last_trusted, reason })),
            Mode::Event { seq } => {
                RootPoll::Complete(Completion::Evaluated(Evaluated::Poisoned { seq, last_trusted, reason }))
            }
        }
    }

    fn last_trusted(&self) -> u64 {
        match &self.state {
            OperationState::Idle(owner) => owner.cursor().0,
            OperationState::Active { last_trusted, .. }
            | OperationState::Poisoned { last_trusted, .. }
            | OperationState::Starting { last_trusted } => *last_trusted,
        }
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

    use super::{OperationState, Root};
    use crate::params::Nil;

    fn entry(seq: u64) -> JournalEntry {
        JournalEntry { seq, kind: KindId(1), cause: None, recorded_at_millis: 0, bytes: Vec::new() }
    }

    #[test]
    fn a_warmed_root_retains_only_its_trigger() {
        let mut root = Root::<Nil>::new().expect("names");
        let warm = Warm::new(WarmEntries::new((1..=300).map(entry).collect()).expect("dense"));
        let warmed = root.warm(warm);
        assert!(matches!(warmed, Warmed::Folded { through: 300 }), "{warmed:?}");

        let evaluated = root.event(Event::new(entry(301)));
        assert!(matches!(evaluated, Evaluated::Completed { seq: 301, .. }), "{evaluated:?}");
        let OperationState::Idle(owner) = &root.state else {
            panic!("completed root must be idle")
        };
        assert_eq!(owner.retained(), 1);
        assert_eq!(root.status().cursor(), 301);
    }
}
