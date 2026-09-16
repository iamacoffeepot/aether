//! The erased table the driver holds.

use std::marker::PhantomData;

use aether_bloomery_journal::{Batch, GetError};
use aether_bloomery_kinds::{Digest, ExecutorName};

use crate::execute::{Execute, Refusal};
use crate::read::{ReadArtifacts, ReadError};
use crate::{Program, digest};

/// Registered executors, claimed by program digest.
pub struct Executors {
    entries: Vec<Entry>,
}

struct Entry {
    name: ExecutorName,
    program: Digest,
    erased: Box<dyn Erased>,
}

impl Executors {
    /// Empty table.
    #[must_use]
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Register `executor` for program `P` under `name`. One call per (executor, program).
    pub fn register<P: Program + 'static, E: Execute<P> + 'static>(&mut self, name: ExecutorName, executor: E) {
        self.entries.push(Entry {
            name,
            program: digest::<P>().digest(),
            erased: Box::new(Typed { executor, _program: PhantomData::<fn() -> P> }),
        });
    }

    /// Names that claim `program`, in registration order.
    pub fn claims(&self, program: &Digest) -> impl Iterator<Item = &ExecutorName> {
        self.entries.iter().filter(move |entry| entry.program == *program).map(|entry| &entry.name)
    }

    pub(crate) fn execute(&self, program: &Digest, input: &Digest, store: &dyn ReadArtifacts) -> Option<ErasedOutcome> {
        Some(self.entries.iter().find(|entry| entry.program == *program)?.erased.execute(input, store))
    }
}

/// Three-way result of an erased execute. Store failures are not refusals.
pub enum ErasedOutcome {
    Executed(Batch, Digest),
    Refused(Refusal),
    Store(ReadError),
}

impl Default for Executors {
    fn default() -> Self {
        Self::new()
    }
}

trait Erased {
    fn execute(&self, input: &Digest, store: &dyn ReadArtifacts) -> ErasedOutcome;
}

struct Typed<P, E> {
    executor: E,
    _program: PhantomData<fn() -> P>,
}

impl<P: Program, E: Execute<P>> Erased for Typed<P, E> {
    fn execute(&self, input: &Digest, store: &dyn ReadArtifacts) -> ErasedOutcome {
        let input = match store.get::<P::Input>(input) {
            Ok(None) => return ErasedOutcome::Refused(Refusal::InputMissing),
            Ok(Some(value)) => value,
            Err(ReadError::Get(GetError::Decode(_) | GetError::PrefixMismatch { .. })) => {
                return ErasedOutcome::Refused(Refusal::InputDecode);
            }
            Err(error) => return ErasedOutcome::Store(error),
        };
        match self.executor.execute(input, store) {
            Ok(execution) => {
                let (batch, digest) = execution.into_erased();
                ErasedOutcome::Executed(batch, digest)
            }
            Err(refusal) => ErasedOutcome::Refused(refusal),
        }
    }
}
