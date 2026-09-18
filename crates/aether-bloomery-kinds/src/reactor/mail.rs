//! Mail a native driver exchanges with a digest-loaded reactor root.

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{CastEligible, Citations, Cites, LabelNode, Schema, SchemaType};

use crate::{Detail, JournalEntry, ReactorName};

use super::ReactorIntent;

/// Why [`WarmEntries::new`] or wire decode refused a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmEntriesError {
    /// The batch was empty.
    Empty,
    /// The first sequence was 0.
    ZeroFirst,
    /// A later sequence was not exactly one past the previous.
    NotDense,
}

impl WarmEntriesError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::ZeroFirst => "zero-first",
            Self::NotDense => "not-dense",
        }
    }
}

impl aether_data::Invariant for WarmEntriesError {
    fn reason(&self) -> &'static str {
        Self::reason(*self)
    }
}

impl fmt::Display for WarmEntriesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl StdError for WarmEntriesError {}

/// Non-empty, dense journal prefix whose first sequence is at least 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmEntries(Vec<JournalEntry>);

impl WarmEntries {
    /// Accept a non-empty dense batch starting at sequence ≥ 1.
    ///
    /// # Errors
    ///
    /// [`WarmEntriesError`] names which rule failed.
    pub fn new(entries: Vec<JournalEntry>) -> Result<Self, WarmEntriesError> {
        Self::check(&entries)?;
        Ok(Self(entries))
    }

    /// Borrow the accepted entries in sequence order.
    #[must_use]
    pub fn as_slice(&self) -> &[JournalEntry] {
        &self.0
    }

    /// First sequence in the batch.
    #[must_use]
    pub fn first(&self) -> u64 {
        self.0[0].seq
    }

    /// Last sequence in the batch.
    #[must_use]
    pub fn last(&self) -> u64 {
        self.0[self.0.len() - 1].seq
    }

    /// Take the entries.
    #[must_use]
    pub fn into_vec(self) -> Vec<JournalEntry> {
        self.0
    }

    fn check(entries: &[JournalEntry]) -> Result<(), WarmEntriesError> {
        let Some(first) = entries.first() else {
            return Err(WarmEntriesError::Empty);
        };
        if first.seq < 1 {
            return Err(WarmEntriesError::ZeroFirst);
        }
        if entries.windows(2).all(|pair| pair[0].seq.checked_add(1) == Some(pair[1].seq)) {
            Ok(())
        } else {
            Err(WarmEntriesError::NotDense)
        }
    }
}

impl Schema for WarmEntries {
    const SCHEMA: SchemaType = <Vec<JournalEntry> as Schema>::SCHEMA;
    const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::WarmEntries"));
    const LABEL_NODE: LabelNode = <Vec<JournalEntry> as Schema>::LABEL_NODE;
}

impl CastEligible for WarmEntries {
    const ELIGIBLE: bool = false;
}

impl Cites for WarmEntries {
    fn cites(&self, _sink: &mut Citations) {}
}

impl WireEncode for WarmEntries {
    fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
        self.0.encode(out)
    }
}

impl<'de> WireDecode<'de> for WarmEntries {
    fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
        Self::new(Vec::<JournalEntry>::decode(cursor)?)
            .map_err(|error| WireError::Message(String::from(error.reason())))
    }
}

/// Fold-only warmup of a contiguous prefix. The root still checks `first == cursor + 1`.
#[aether_data::kind(name = "aether.bloomery.reactor.warm", eq, no_serde)]
pub struct Warm {
    entries: WarmEntries,
}

impl Warm {
    /// Carry an already-validated batch.
    #[must_use]
    pub fn new(entries: WarmEntries) -> Self {
        Self { entries }
    }

    /// The warmup prefix.
    #[must_use]
    pub const fn entries(&self) -> &WarmEntries {
        &self.entries
    }

    /// Take the warmup prefix.
    #[must_use]
    pub fn into_entries(self) -> WarmEntries {
        self.entries
    }
}

/// Reply to one [`Warm`].
#[aether_data::kind(name = "aether.bloomery.reactor.warmed", eq, no_serde)]
pub enum Warmed {
    /// Views folded through `through`.
    Folded { through: u64 },
    /// The batch did not start at the next sequence.
    OutOfSequence { first: u64, expected: u64 },
    /// A previous fold failed; the instance cannot admit further input.
    Poisoned { last_trusted: u64, reason: Detail },
}

/// One live journal entry. The root checks `seq == cursor + 1`.
#[aether_data::kind(name = "aether.bloomery.reactor.event", eq, no_serde)]
pub struct Event {
    entry: JournalEntry,
}

impl Event {
    /// Carry one journal envelope.
    #[must_use]
    pub fn new(entry: JournalEntry) -> Self {
        Self { entry }
    }

    /// The live entry.
    #[must_use]
    pub const fn entry(&self) -> &JournalEntry {
        &self.entry
    }

    /// Take the live entry.
    #[must_use]
    pub fn into_entry(self) -> JournalEntry {
        self.entry
    }
}

/// Reply to one [`Event`].
#[aether_data::kind(name = "aether.bloomery.reactor.evaluated", eq, no_serde)]
pub enum Evaluated {
    /// Every reactor ran; `intents` are attributed rule outputs.
    Completed { seq: u64, intents: Vec<ReactorIntent> },
    /// The entry was not the next sequence.
    OutOfSequence { seq: u64, expected: u64 },
    /// A previous fold failed; the instance cannot admit further input.
    Poisoned { seq: u64, last_trusted: u64, reason: Detail },
    /// One reactor failed; views advanced and no intents are returned.
    Failed { seq: u64, reactor: ReactorName, reason: Detail },
}

/// Ask the root for its cursor and poison flag.
#[aether_data::kind(name = "aether.bloomery.reactor.status_query", eq, no_serde)]
pub struct StatusQuery;

/// Reply to [`StatusQuery`].
#[aether_data::kind(name = "aether.bloomery.reactor.status", eq, no_serde)]
pub struct Status {
    cursor: u64,
    poisoned: bool,
}

impl Status {
    /// Report `cursor` and whether a fold has poisoned the instance.
    #[must_use]
    pub const fn new(cursor: u64, poisoned: bool) -> Self {
        Self { cursor, poisoned }
    }

    /// Last trusted sequence, or 0 when nothing has been admitted.
    #[must_use]
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Whether a fold failure has poisoned the instance.
    #[must_use]
    pub const fn poisoned(&self) -> bool {
        self.poisoned
    }
}
