//! Mail kinds and adapters that turn a retained [`Owner`] into cluster input.
//!
//! Authors declare reactors; generated views and peer actors speak these kinds.
//! Journal payloads stay on the storage codec. Guards are never mailed. View
//! snapshots use [`aether_bloomery_view::Publish`] codecs keyed by
//! [`crate::BundledView::NAME`], never [`core::any::TypeId`].

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use aether_bloomery_kinds::{Entry, Seq};

use crate::error::PrepareError;
use crate::evaluate::{ArmVisitor, Output, Reactor};
use crate::owner::Owner;
use crate::params::Params;
use crate::trigger::Trigger;
use crate::views::{PublishCtor, PublishSet};

/// Boot configuration for a generated views owner and its reactor peers.
///
/// `output` is the external mailbox that receives typed arm outputs. An empty
/// path drops outputs rather than guessing a destination.
#[aether_data::kind(name = "aether.bloomery.reactor.config", default, eq)]
pub struct ClusterConfig {
    /// Runtime-name address of the external output mailbox.
    pub output: String,
}

/// Portable journal envelope carried over mail. `bytes` are storage-codec
/// payload bytes, never positional [`aether_data::Kind::encode_into_bytes`].
#[derive(Clone, Debug, PartialEq, Eq, aether_data::Schema, serde::Serialize, serde::Deserialize)]
pub struct JournalEntry {
    /// Dense sequence assigned by the store.
    pub seq: u64,
    /// Stored kind name.
    pub kind: String,
    /// Optional causing sequence.
    pub cause: Option<u64>,
    /// Wall clock at insert; folds ignore it.
    pub recorded_at_millis: u64,
    /// Verbatim storage-codec payload.
    #[serde(with = "aether_data::bytes")]
    pub bytes: Vec<u8>,
}

impl JournalEntry {
    /// Copy one retained [`Entry`] into the mail envelope.
    #[must_use]
    pub fn from_entry(entry: &Entry) -> Self {
        Self {
            seq: entry.seq.0,
            kind: entry.kind.clone(),
            cause: entry.cause.map(|seq| seq.0),
            recorded_at_millis: entry.recorded_at_millis,
            bytes: entry.bytes.clone(),
        }
    }

    /// Rebuild the portable [`Entry`]. Payload bytes stay storage-encoded.
    #[must_use]
    pub fn to_entry(&self) -> Entry {
        Entry {
            seq: Seq(self.seq),
            kind: self.kind.clone(),
            cause: self.cause.map(Seq),
            recorded_at_millis: self.recorded_at_millis,
            bytes: self.bytes.clone(),
        }
    }
}

/// Contiguous entries pushed to a views owner. Each entry is prepared as its
/// own prefix so later live Event/EventBatch admission can keep per-event
/// boundaries.
#[aether_data::kind(name = "aether.bloomery.reactor.push_entries")]
pub struct PushEntries {
    /// Ordered journal envelopes.
    pub entries: Vec<JournalEntry>,
}

impl PushEntries {
    /// Wrap retained entries for mail.
    #[must_use]
    pub fn from_entries(entries: &[Entry]) -> Self {
        Self { entries: entries.iter().map(JournalEntry::from_entry).collect() }
    }
}

/// Aggregation outcome for one [`PushEntries`] request. Settlement of the
/// request is not proof of successful reactor evaluation.
#[aether_data::kind(name = "aether.bloomery.reactor.push_result")]
pub enum PushResult {
    /// Views advanced through `cursor`.
    Ok {
        /// Last retained sequence after the push.
        cursor: u64,
    },
    /// Prefix or fold failed before peers were prepared.
    Err {
        /// Display of the [`PrepareError`].
        message: String,
    },
}

impl PushResult {
    /// Convert a prepare/push outcome into the reply kind.
    #[must_use]
    pub fn from_prepare(result: Result<u64, PrepareError>) -> Self {
        match result {
            Ok(cursor) => Self::Ok { cursor },
            Err(error) => Self::Err { message: alloc::format!("{error}") },
        }
    }
}

/// Ask a views owner for its current aggregation cursor.
#[aether_data::kind(name = "aether.bloomery.reactor.cluster_status_query", default)]
pub struct ClusterStatusQuery;

/// Cursor of one cluster's bundled views. Not a durable execution checkpoint.
#[aether_data::kind(name = "aether.bloomery.reactor.cluster_status", eq)]
pub struct ClusterStatus {
    /// Last retained sequence, or `0` when empty.
    pub cursor: u64,
}

/// One published view snapshot. `bytes` are [`aether_bloomery_view::Publish::encode`] output.
#[derive(Clone, Debug, PartialEq, Eq, aether_data::Schema, serde::Serialize, serde::Deserialize)]
pub struct PublishedView {
    /// [`crate::BundledView::NAME`], never a [`core::any::TypeId`].
    pub name: String,
    /// Publish-codec payload.
    #[serde(with = "aether_data::bytes")]
    pub bytes: Vec<u8>,
}

/// Owned prepared prefix mailed to a reactor peer.
///
/// Carries the trigger entry's storage bytes plus a deduplicated list of
/// published view snapshots at that cursor. Named guards are resolved in the
/// peer against those snapshots; they are not serialized.
#[aether_data::kind(name = "aether.bloomery.reactor.prepared_prefix")]
pub struct PreparedPrefix {
    /// Trigger sequence.
    pub seq: u64,
    /// Stored trigger kind name.
    pub kind: String,
    /// Optional causing sequence.
    pub cause: Option<u64>,
    /// Wall clock at insert.
    pub recorded_at_millis: u64,
    /// Storage-codec trigger payload.
    #[serde(with = "aether_data::bytes")]
    pub bytes: Vec<u8>,
    /// Deduplicated published snapshots at `seq`.
    pub views: Vec<PublishedView>,
}

impl PreparedPrefix {
    /// Snapshot `entry` together with already-encoded published views.
    #[must_use]
    pub fn from_parts(entry: &Entry, views: Vec<PublishedView>) -> Self {
        Self {
            seq: entry.seq.0,
            kind: entry.kind.clone(),
            cause: entry.cause.map(|seq| seq.0),
            recorded_at_millis: entry.recorded_at_millis,
            bytes: entry.bytes.clone(),
            views,
        }
    }

    /// Rebuild the trigger envelope. Payload bytes stay storage-encoded.
    #[must_use]
    pub fn to_entry(&self) -> Entry {
        Entry {
            seq: Seq(self.seq),
            kind: self.kind.clone(),
            cause: self.cause.map(Seq),
            recorded_at_millis: self.recorded_at_millis,
            bytes: self.bytes.clone(),
        }
    }

    /// Owner that decodes this trigger and installs `R`'s required snapshots.
    ///
    /// Duplicate, missing, malformed, or wrong-prefix snapshots fail. The peer
    /// must not re-fold history from the trigger entry alone.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] when the prefix cannot reconstruct `R`'s views.
    pub fn into_owner<R: Reactor>(self) -> Result<Owner, PrepareError> {
        reject_duplicate_snapshots(&self.views)?;
        let entry = self.to_entry();
        let views = self.views;
        let mut owner = Owner::from_prepared(entry);
        install_reactor::<R>(&mut owner, &views)?;
        Ok(owner)
    }
}

/// Fold every view named by `R`'s arms to the owner's current cursor.
///
/// # Errors
///
/// [`PrepareError`] from [`Owner::warm`].
pub fn warm_reactor<R: Reactor>(owner: &mut Owner) -> Result<(), PrepareError> {
    struct Warm<'a> {
        owner: &'a mut Owner,
        error: Result<(), PrepareError>,
    }

    impl ArmVisitor for Warm<'_> {
        fn visit<T, L, O>(&mut self, _name: &'static str)
        where
            T: Trigger,
            L: Params<T>,
            L::Views: PublishSet,
            O: Output,
        {
            if self.error.is_ok() {
                self.error = self.owner.warm::<L::Views>();
            }
        }
    }

    let mut warm = Warm { owner, error: Ok(()) };
    R::visit_arms(&mut warm);
    warm.error
}

/// Encode every unique published view `R` needs at the owner's current cursor.
///
/// # Errors
///
/// [`PrepareError`] when a required view is missing or cannot encode.
pub fn snapshot_reactor<R: Reactor>(owner: &Owner) -> Result<Vec<PublishedView>, PrepareError> {
    struct Snap<'a> {
        owner: &'a Owner,
        views: Vec<PublishedView>,
        seen: BTreeSet<&'static str>,
        error: Result<(), PrepareError>,
    }

    impl ArmVisitor for Snap<'_> {
        fn visit<T, L, O>(&mut self, _name: &'static str)
        where
            T: Trigger,
            L: Params<T>,
            L::Views: PublishSet,
            O: Output,
        {
            if self.error.is_ok() {
                self.error = collect_published::<L::Views>(self.owner, &mut self.views, &mut self.seen);
            }
        }
    }

    let mut snap = Snap { owner, views: Vec::new(), seen: BTreeSet::new(), error: Ok(()) };
    R::visit_arms(&mut snap);
    snap.error?;
    Ok(snap.views)
}

/// Union `src` into `dst` by published name. Equal duplicates are dropped;
/// unequal duplicates fail.
///
/// # Errors
///
/// [`PrepareError::DuplicateSnapshot`] when the same name carries different bytes.
pub fn extend_snapshots(dst: &mut Vec<PublishedView>, src: Vec<PublishedView>) -> Result<(), PrepareError> {
    for snap in src {
        if let Some(existing) = dst.iter().find(|existing| existing.name == snap.name) {
            if existing.bytes != snap.bytes {
                return Err(PrepareError::DuplicateSnapshot { view: snap.name });
            }
            continue;
        }
        dst.push(snap);
    }
    Ok(())
}

fn collect_published<S: PublishSet>(
    owner: &Owner,
    out: &mut Vec<PublishedView>,
    seen: &mut BTreeSet<&'static str>,
) -> Result<(), PrepareError> {
    let mut error = Ok(());
    S::each_published(|ctor| {
        if error.is_ok() {
            error = collect_one(owner, ctor, out, seen);
        }
    });
    error
}

fn collect_one(
    owner: &Owner,
    ctor: PublishCtor,
    out: &mut Vec<PublishedView>,
    seen: &mut BTreeSet<&'static str>,
) -> Result<(), PrepareError> {
    if !seen.insert(ctor.name) {
        return Ok(());
    }
    let any = owner
        .slot_ref(ctor.id)
        .ok_or(PrepareError::Poisoned { view: ctor.name, last_trusted_cursor: owner.cursor() })?;
    let bytes = (ctor.encode)(any)?;
    out.push(PublishedView { name: String::from(ctor.name), bytes });
    Ok(())
}

fn install_reactor<R: Reactor>(owner: &mut Owner, snapshots: &[PublishedView]) -> Result<(), PrepareError> {
    struct Install<'a> {
        owner: &'a mut Owner,
        snapshots: &'a [PublishedView],
        error: Result<(), PrepareError>,
        seen: BTreeSet<&'static str>,
    }

    impl ArmVisitor for Install<'_> {
        fn visit<T, L, O>(&mut self, _name: &'static str)
        where
            T: Trigger,
            L: Params<T>,
            L::Views: PublishSet,
            O: Output,
        {
            if self.error.is_ok() {
                self.error = install_published::<L::Views>(self.owner, self.snapshots, &mut self.seen);
            }
        }
    }

    let mut install = Install { owner, snapshots, error: Ok(()), seen: BTreeSet::new() };
    R::visit_arms(&mut install);
    install.error
}

fn install_published<S: PublishSet>(
    owner: &mut Owner,
    snapshots: &[PublishedView],
    seen: &mut BTreeSet<&'static str>,
) -> Result<(), PrepareError> {
    let mut error = Ok(());
    S::each_published(|ctor| {
        if error.is_ok() {
            error = install_one(owner, ctor, snapshots, seen);
        }
    });
    error
}

fn install_one(
    owner: &mut Owner,
    ctor: PublishCtor,
    snapshots: &[PublishedView],
    seen: &mut BTreeSet<&'static str>,
) -> Result<(), PrepareError> {
    if !seen.insert(ctor.name) {
        return Ok(());
    }
    let snap = snapshots
        .iter()
        .find(|snap| snap.name == ctor.name)
        .ok_or(PrepareError::MissingSnapshot { view: ctor.name })?;
    let boxed = (ctor.decode)(&snap.bytes)?;
    owner.install_erased(ctor.id, ctor.name, boxed)
}

fn reject_duplicate_snapshots(views: &[PublishedView]) -> Result<(), PrepareError> {
    let mut seen = BTreeSet::new();
    for snap in views {
        if !seen.insert(snap.name.as_str()) {
            return Err(PrepareError::DuplicateSnapshot { view: snap.name.clone() });
        }
    }
    Ok(())
}
