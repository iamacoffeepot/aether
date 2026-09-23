//! Expect: the actual journal against the values a scenario states.
//!
//! Every read here opens a fresh handle on the journal file the chassis
//! writes, so it sees exactly what the loop committed. Expected values come
//! from the scenario — literals, or the handles its seed returned — and the
//! only fold this module runs reduces the actual journal.

use std::fmt::{Debug, Write};

use aether_bloomery_journal::{Digest, Entry, Journal, Seq};
use aether_bloomery_view::View;
use aether_data::{KindId, Storage};

use crate::BloomeryHarness;

/// How many entries one journal read returns.
const READ_PAGE: usize = 256;

/// A scenario's check over one decoded record.
type Check = Box<dyn Fn(&Entry)>;

/// One record a scenario expects the loop to have appended: its kind, its
/// cause, and optionally a check over its decoded value.
///
/// `recorded_at_millis` is never compared: it is wall clock, for people.
pub struct Record {
    kind: KindId,
    name: &'static str,
    cause: Option<Seq>,
    check: Option<Check>,
}

impl Record {
    /// A `K` record caused by `cause`, whose fields the scenario does not name.
    #[must_use]
    pub fn of<K: Storage>(cause: Option<Seq>) -> Self {
        Self { kind: K::ID, name: K::NAME, cause, check: None }
    }

    /// A `K` record caused by `cause` whose decoded value equals `expected`.
    ///
    /// # Panics
    ///
    /// The record panics when checked, if the recorded value does not decode
    /// as `K` or differs from `expected`.
    #[must_use]
    pub fn equal<K: Storage + PartialEq + Debug + 'static>(cause: Option<Seq>, expected: K) -> Self {
        Self::matching(cause, move |actual: &K| {
            assert_eq!(actual, &expected, "the recorded {} differs from the scenario's literal", K::NAME);
        })
    }

    /// A `K` record caused by `cause` whose decoded value passes `check`,
    /// which asserts the fields the scenario names.
    ///
    /// # Panics
    ///
    /// The record panics when checked, if the recorded value does not decode
    /// as `K` or `check` panics.
    #[must_use]
    pub fn matching<K: Storage + 'static>(cause: Option<Seq>, check: impl Fn(&K) + 'static) -> Self {
        let check = move |entry: &Entry| {
            let value = Journal::decode::<K>(entry)
                .unwrap_or_else(|error| panic!("decode the {} recorded at {}: {error}", K::NAME, entry.seq));
            check(&value);
        };
        Self { kind: K::ID, name: K::NAME, cause, check: Some(Box::new(check)) }
    }
}

impl BloomeryHarness {
    /// Assert the records the loop appended after `after` are exactly
    /// `expected`: the same count, then each record's kind and cause in order,
    /// then each stated value.
    ///
    /// # Panics
    ///
    /// Panics when the appended sequence differs, printing the actual
    /// `(seq, kind, cause)` sequence beside the expected one, or when a
    /// record's value fails its check.
    pub fn assert_appended(&self, after: Seq, expected: &[Record]) {
        let actual = self.entries_after(after);
        let shape_holds = actual.len() == expected.len()
            && actual
                .iter()
                .zip(expected)
                .all(|(entry, record)| entry.kind == record.kind && entry.cause == record.cause);
        assert!(
            shape_holds,
            "the records appended after {after} differ from the scenario's\nactual:\n{}expected:\n{}",
            describe_actual(&actual, expected),
            describe_expected(after, expected),
        );
        for (entry, record) in actual.iter().zip(expected) {
            if let Some(check) = &record.check {
                check(entry);
            }
        }
    }

    /// The record at `seq`, decoded as `K`.
    ///
    /// # Panics
    ///
    /// Panics when the journal holds no record at `seq`, or it is not a `K`.
    #[must_use]
    pub fn record<K: Storage>(&self, seq: Seq) -> K {
        let entry = self
            .open()
            .read(Seq(seq.0.saturating_sub(1)), 1)
            .expect("read the journal")
            .into_iter()
            .find(|entry| entry.seq == seq)
            .unwrap_or_else(|| panic!("the journal holds no record at {seq}"));
        Journal::decode::<K>(&entry)
            .unwrap_or_else(|error| panic!("decode the record at {seq} as {}: {error}", K::NAME))
    }

    /// The journal head.
    ///
    /// # Panics
    ///
    /// Panics when the journal cannot be opened or read.
    #[must_use]
    pub fn head(&self) -> Seq {
        self.open().head().expect("read the journal head")
    }

    /// Whether the journal stores an artifact under `digest`.
    ///
    /// # Panics
    ///
    /// Panics when the journal cannot be opened or read.
    #[must_use]
    pub fn stores(&self, digest: &Digest) -> bool {
        self.open().get_bytes(digest).expect("read an artifact from the journal").is_some()
    }

    /// Fold the whole actual journal through `V`, from its empty state.
    ///
    /// # Panics
    ///
    /// Panics when the journal cannot be read or the view refuses it.
    #[must_use]
    pub fn fold<V: View>(&self) -> V {
        let mut view = V::empty();
        view.advance(&self.entries_after(Seq(0)))
            .unwrap_or_else(|error| panic!("fold the journal through the view: {error}"));
        view
    }

    /// Every entry with `seq > after`, in order.
    fn entries_after(&self, after: Seq) -> Vec<Entry> {
        let journal = self.open();
        let mut entries = Vec::new();
        loop {
            let since = entries.last().map_or(after, |entry: &Entry| entry.seq);
            let page = journal.read(since, READ_PAGE).expect("read the journal");
            let exhausted = page.len() < READ_PAGE;
            entries.extend(page);
            if exhausted {
                return entries;
            }
        }
    }

    /// A fresh handle on the journal the chassis writes.
    fn open(&self) -> Journal {
        Journal::open(&self.journal).expect("open the journal the chassis writes")
    }
}

/// The actual `(seq, kind, cause)` sequence, one per line. A kind the
/// scenario names prints by name; any other by its tagged id.
fn describe_actual(actual: &[Entry], expected: &[Record]) -> String {
    let mut out = String::new();
    for entry in actual {
        let name = expected.iter().find(|record| record.kind == entry.kind).map(|record| record.name);
        let _ = match name {
            Some(name) => writeln!(out, "  {} {name} cause {:?}", entry.seq, entry.cause),
            None => writeln!(out, "  {} {} cause {:?}", entry.seq, entry.kind, entry.cause),
        };
    }
    out
}

/// The expected `(seq, kind, cause)` sequence, one per line.
fn describe_expected(after: Seq, expected: &[Record]) -> String {
    let mut out = String::new();
    for (seq, record) in (after.0 + 1..).zip(expected) {
        let _ = writeln!(out, "  {} {} cause {:?}", Seq(seq), record.name, record.cause);
    }
    out
}
