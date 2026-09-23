//! Reactor scenario scripting: seeders, collectors, and observation over [`World`](crate::support::World).
//!
//! This module is declared only by the reactor scenario targets
//! (`routing.rs`, `activation.rs`, `restart.rs`), so every item here is used
//! by all three and the dead-code gate stays green. Items a single target
//! needs live in that target's file instead.

use aether_bloomery_driver::Command;
use aether_bloomery_kinds::{
    Activated, ActivationRejected, Digest, DriverRecord, Head, OpaqueBytes, ReactionFailed, ReactorSet, RecordedHead,
    RecordedHeadMove, Requested,
};
use aether_data::{Kind, Storage, StorageData};

use crate::support::{World, bundle_wasm};

/// One reactor set over the given member heads, in canonical order.
///
/// # Panics
///
/// Panics if the members are not already canonical.
#[must_use]
pub fn reactor_set(members: &[&'static str]) -> ReactorSet {
    let mut heads: Vec<Head<OpaqueBytes>> = members.iter().map(|name| Head::new(name)).collect();
    heads.sort();
    ReactorSet::new(heads).expect("canonical test set")
}

/// Every `Requested` the core appended: cause and record, in append order.
#[must_use]
pub fn requested_records(world: &World) -> Vec<(Option<u64>, Requested)> {
    let mut out = Vec::new();
    for append in &world.appends {
        for record in append.records() {
            if let DriverRecord::Requested { cause, record } = record {
                out.push((*cause, record.clone()));
            }
        }
    }
    out
}

/// Every `Activated` the core appended: cause and record, in append order.
#[must_use]
pub fn activated_records(world: &World) -> Vec<(u64, Activated)> {
    let mut out = Vec::new();
    for append in &world.appends {
        for record in append.records() {
            if let DriverRecord::Activated { cause, record } = record {
                out.push((*cause, record.clone()));
            }
        }
    }
    out
}

/// Every `ActivationRejected` the core appended: cause and record, in order.
#[must_use]
pub fn rejected_records(world: &World) -> Vec<(u64, ActivationRejected)> {
    let mut out = Vec::new();
    for append in &world.appends {
        for record in append.records() {
            if let DriverRecord::ActivationRejected { cause, record } = record {
                out.push((*cause, record.clone()));
            }
        }
    }
    out
}

/// Every `ReactionFailed` the core appended: cause and record, in order.
#[must_use]
pub fn failed_records(world: &World) -> Vec<(u64, ReactionFailed)> {
    let mut out = Vec::new();
    for append in &world.appends {
        for record in append.records() {
            if let DriverRecord::ReactionFailed { cause, record } = record {
                out.push((*cause, record.clone()));
            }
        }
    }
    out
}

/// Every `HeadMoved` the core appended: cause and move, in append order.
#[must_use]
pub fn head_moves(world: &World) -> Vec<(u64, RecordedHeadMove)> {
    let mut out = Vec::new();
    for append in &world.appends {
        for record in append.records() {
            if let DriverRecord::HeadMoved { cause, record } = record {
                out.push((*cause, record.clone()));
            }
        }
    }
    out
}

impl World {
    /// Store one reactor-set artifact the core can read back.
    #[must_use]
    pub fn store_set(&mut self, set: &ReactorSet) -> Digest {
        let bytes = ReactorSet::encode_storage(&StorageData::from_value(set.clone())).expect("encode set");
        self.store(ReactorSet::ID, &bytes)
    }

    /// Seed the reactor-set root head move into the journal truth.
    pub fn seed_set_root(&mut self, set: Digest) {
        let moved = RecordedHeadMove::new(RecordedHead::from(&ReactorSet::ROOT), set);
        self.seed(None, &moved);
    }

    /// Store one reactor-only wasm bundle, labelled for a distinct digest, and answer its loads.
    #[must_use]
    pub fn store_reactor(&mut self, label: &[u8]) -> Digest {
        let wasm = bundle_wasm(&[], &["test.reactor"], label);
        let digest = self.store(OpaqueBytes::ID, &wasm);
        self.loads.insert(digest, Ok(()));
        digest
    }

    /// Append one typed record as another writer, waking parked watches.
    ///
    /// # Panics
    ///
    /// Panics if the record does not storage-encode.
    pub fn append_external<K: Kind + Storage + Clone>(&mut self, cause: Option<u64>, value: &K) -> Vec<Command> {
        self.seed(cause, value);
        self.wake_watches(self.head())
    }

    /// Warm ranges the core sent for `bundle`, in order.
    #[must_use]
    pub fn warm_ranges_for(&self, bundle: Digest) -> Vec<(u64, u64)> {
        self.reactors.get(&bundle).map(|reactor| reactor.warms.clone()).unwrap_or_default()
    }

    /// Event seqs the core sent for `bundle`, in order.
    #[must_use]
    pub fn events_for(&self, bundle: Digest) -> Vec<u64> {
        self.reactors.get(&bundle).map(|reactor| reactor.events.clone()).unwrap_or_default()
    }
}
