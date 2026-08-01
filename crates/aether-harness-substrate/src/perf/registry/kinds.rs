//! Which `KindId` each reader looks up (iamacoffeepot/aether#4276).
//!
//! # What this isolates
//!
//! `Registry::route_lookup` resolves the route by recipient and then attaches
//! the kind's name:
//!
//! ```ignore
//! kind_name: self.kinds.load().table().kinds
//!     .get(&kind)
//!     .map_or_else(|| Arc::clone(&self.empty_kind_name), |slot| Arc::clone(&slot.name)),
//! ```
//!
//! For a fixed kind that is the **same `Arc<str>` for every reader on every
//! lookup**, so its refcount is one cacheline that every reader increments and
//! decrements twice per lookup. The endpoint's own two `Arc`s are per target
//! and spread across the populated routes; this one does not spread.
//!
//! [`KindMix`] is the cut. `Shared` is what the sweep has always measured — all
//! readers on one kind. `PerReader` gives reader *w* its own registered kind, so
//! every reader clones a **different** `Arc<str>` with its own control block.
//! Nothing else changes: same `resolve_route_state`, same populated table, same
//! windows, same route resolution (the kind never affects which route is found,
//! only the name attached to it).
//!
//! Read the two arms against each other:
//!
//! - `PerReader` scales and `Shared` does not → the shared kind-name clone is
//!   the serializer.
//! - Both stay pinned → it is the per-target endpoint clones, and the next cut
//!   is readers walking disjoint target sets.
//!
//! # Why these are real registered kinds
//!
//! An *unregistered* id would miss the table and take the `empty_kind_name`
//! branch — which is also a single shared `Arc`, so the arm would measure the
//! same contention under a different name and read as if it had exonerated the
//! clone. The probe kinds are `#[derive(Kind)]` types, so the link-time
//! descriptor inventory registers them at boot exactly as it registers every
//! other kind, and each gets its own `KindSlot` with its own name allocation.

use aether_data::{Kind, KindId};
use aether_substrate::Registry;

use crate::perf::harness::Ping;

/// How readers are assigned the kind they look up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KindMix {
    /// Every reader looks up the same kind — the mailer's shape when one kind
    /// dominates traffic, and what every earlier reading of this sweep measured.
    Shared,
    /// Each reader looks up its own kind, so no two readers touch the same
    /// kind-name refcount.
    PerReader,
}

impl KindMix {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::PerReader => "per-reader",
        }
    }

    /// The kind reader `worker` looks up under this mix.
    ///
    /// `PerReader` wraps once the reader count exceeds the probe vocabulary,
    /// which would quietly re-share a kind between two readers — so
    /// [`covers`](Self::covers) is what a caller checks before trusting the arm,
    /// rather than this silently degrading into the `Shared` arm.
    #[must_use]
    pub fn kind_for(self, worker: usize) -> KindId {
        match self {
            Self::Shared => <Ping as Kind>::ID,
            Self::PerReader => PROBE_KIND_IDS[worker % PROBE_KIND_IDS.len()],
        }
    }

    /// Whether this mix can give `threads` readers distinct kinds. Always true
    /// for `Shared`, which wants them shared.
    #[must_use]
    pub fn covers(self, threads: usize) -> bool {
        match self {
            Self::Shared => true,
            Self::PerReader => threads <= PROBE_KIND_IDS.len(),
        }
    }
}

/// Whether every probe kind resolves in `registry` — the `PerReader` arm's
/// positive control, and it is not optional.
///
/// The arm's whole claim is that each reader clones a *different* `Arc<str>`.
/// An unregistered id gets `empty_kind_name` instead, which is one shared `Arc`
/// for all of them — so an arm built on unregistered kinds re-creates exactly
/// the contention it was supposed to remove, and then reports "still pinned" as
/// if it had ruled the clone out. That failure is invisible in the numbers: it
/// looks like a clean negative result. Checking is the only way to tell a real
/// negative from a broken arm, so the sweep checks and says so.
#[must_use]
pub fn per_reader_kinds_registered(registry: &Registry) -> bool {
    let missing: Vec<_> = PROBE_KIND_IDS.iter().filter(|id| registry.kind_name(**id).is_none()).collect();
    if !missing.is_empty() {
        tracing::warn!(
            target: "aether_perf",
            missing = missing.len(),
            total = PROBE_KIND_IDS.len(),
            "probe kinds are not registered; the per-reader arm shares `empty_kind_name` and cannot exonerate the kind-name clone",
        );
    }
    missing.is_empty()
}

/// Declare the probe kinds and collect their ids in declaration order.
///
/// One `#[derive(Kind)]` type per reader slot. A loop cannot produce these —
/// a `KindId` is derived at compile time from the kind's name and schema, which
/// is exactly the property that makes each one a distinct registered kind with
/// its own name allocation rather than a runtime-tagged copy of one kind.
macro_rules! probe_kinds {
    ($($ident:ident => $name:tt),* $(,)?) => {
        $(
            /// One reader slot's kind. Carries a field only to stay a
            /// well-formed `Pod`; the id is the whole signal.
            #[repr(C)]
            #[derive(
                Copy,
                Clone,
                Debug,
                Default,
                PartialEq,
                Eq,
                bytemuck::Pod,
                bytemuck::Zeroable,
                aether_data::Kind,
                aether_data::Schema,
            )]
            #[kind(name = $name)]
            pub struct $ident {
                pub seq: u32,
            }
        )*

        /// Every probe kind's id, indexed by reader slot.
        pub static PROBE_KIND_IDS: &[KindId] = &[$(<$ident as Kind>::ID),*];
    };
}

probe_kinds! {
    Probe0 => "aether.perf.registry.probe.0",
    Probe1 => "aether.perf.registry.probe.1",
    Probe2 => "aether.perf.registry.probe.2",
    Probe3 => "aether.perf.registry.probe.3",
    Probe4 => "aether.perf.registry.probe.4",
    Probe5 => "aether.perf.registry.probe.5",
    Probe6 => "aether.perf.registry.probe.6",
    Probe7 => "aether.perf.registry.probe.7",
    Probe8 => "aether.perf.registry.probe.8",
    Probe9 => "aether.perf.registry.probe.9",
    Probe10 => "aether.perf.registry.probe.10",
    Probe11 => "aether.perf.registry.probe.11",
    Probe12 => "aether.perf.registry.probe.12",
    Probe13 => "aether.perf.registry.probe.13",
    Probe14 => "aether.perf.registry.probe.14",
    Probe15 => "aether.perf.registry.probe.15",
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Tripwire (iamacoffeepot/aether#4276): the `PerReader` arm hands every
    /// reader a **distinct** id. Two readers sharing one would put them back on
    /// the same refcount, and the arm would report "no contention here" for the
    /// contention it failed to remove — a false exoneration that reads exactly
    /// like a real result.
    #[test]
    fn per_reader_kinds_are_distinct() {
        let ids: HashSet<KindId> = (0..PROBE_KIND_IDS.len()).map(|w| KindMix::PerReader.kind_for(w)).collect();
        assert_eq!(ids.len(), PROBE_KIND_IDS.len(), "two probe kinds collided into one id");
    }

    /// Tripwire: the vocabulary covers the whole swept reader range. The arm
    /// wraps past its end, so a `READER_THREADS` that outgrew the probe list
    /// would silently re-share kinds between readers instead of failing.
    #[test]
    fn the_probe_vocabulary_covers_every_swept_reader_count() {
        let widest = super::super::read::READER_THREADS.iter().copied().max().expect("a swept reader count");
        assert!(KindMix::PerReader.covers(widest), "{widest} readers but only {} probe kinds", PROBE_KIND_IDS.len());
    }

    /// The `Shared` arm must genuinely share — it is the control the other arm
    /// is read against.
    #[test]
    fn the_shared_arm_gives_every_reader_the_same_kind() {
        let first = KindMix::Shared.kind_for(0);
        assert!((0..16).all(|w| KindMix::Shared.kind_for(w) == first));
    }
}
