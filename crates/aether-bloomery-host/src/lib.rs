//! aether-bloomery-host: the Bloomery coordinator chassis and its `SQLite`
//! journal store (ADR-0149, third slice).
//!
//! The ADR-0149 control core ([`aether_bloomery`]) is a pure
//! `reduce(snapshot, event) -> decisions` function; the journal plus the
//! content-addressed artifact bytes are its only truth. This crate gives that
//! truth a durable single-writer home and a host to run it:
//!
//! - [`store`] — the native `aether.store` capability. `SQLite` in WAL mode holds
//!   the append-only journal (with inbox dedup by idempotency key), a
//!   transactional outbox **partitioned by topic** (so disjoint consumers drain
//!   and ack their own rows without racing on the shared `delivered` flag), and
//!   the active-membership table whose uniqueness constraint makes bloom sealing
//!   all-or-nothing. The guest sees typed `aether.store.*` transact mail, never
//!   SQL.
//! - [`artifacts`] — the native `aether.artifacts` capability. An eviction-free
//!   consumer of the extracted content-address core
//!   ([`aether_substrate::content_store`]) holding the canonical
//!   digest-addressed artifact bytes and their derivation-DAG parents; a
//!   second consumer of the one addressing core (ADR-0116 reuse-not-rival),
//!   never evicting.
//! - [`bloomery`] — [`BloomeryChassis`], a
//!   coordinator-shaped chassis (no render/audio surface) that registers the
//!   store, artifacts, trace, RPC, and the `aether.bloomery.mirror`
//!   outbox-consumer capability behind a signal-blocking driver. The mirror
//!   driver polls the store outbox and drives the `ProjectionShell` /
//!   `SourceShell` so a live bloomery continuously projects its journal to
//!   GitHub (config-gated off when unconfigured).
//!
//! Recovery is journal replay + outbox republish: reopen the same database
//! file, replay the journal through the reducer, and republish undelivered
//! outbox entries — the mirror driver's first drain pass on boot is that
//! republish, acking each topic's delivered prefix only after the GitHub write
//! succeeds (at-least-once with idempotent reconcile).

pub mod artifacts;
pub mod store;

#[cfg(feature = "runtime")]
pub mod bloomery;

#[cfg(feature = "runtime")]
pub use bloomery::{BloomeryChassis, BloomeryEnv};
