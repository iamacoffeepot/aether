//! aether-chassis-bloomery: the Bloomery coordinator chassis and its `SQLite`
//! journal store (ADR-0149, third slice).
//!
//! The ADR-0149 control core ([`aether_bloomery`]) is a pure
//! `reduce(snapshot, event, configs) -> decisions` function; the journal plus the
//! content-addressed artifact bytes are its only truth. This crate gives that
//! truth a durable single-writer home and a host to run it:
//!
//! - [`store`] — the native `aether.store` capability. `SQLite` in WAL mode holds
//!   the append-only journal (with inbox dedup by idempotency key), a
//!   transactional outbox **partitioned by topic** (so disjoint reactors drain
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
//! - [`session`] — the native `aether.session` capability, the executor
//!   session-reuse pool. An executor-lane optimization (not an ADR-0149
//!   control-core port): it pools headless-Claude sessions for the model-driven
//!   runner lane (#3511), keyed by `(model, effort, task)`, age-bounded by the
//!   prompt-cache TTL (#3264) and gated on a static-prefix head-hash freshness
//!   check (#3422), so a resumed attempt reuses the cached prefix instead of
//!   re-writing it. It holds pool metadata + the lease in its own small `SQLite`
//!   table; session transcript bytes live in [`artifacts`], content-addressed.
//! - [`source`] — the native `aether.source` capability. A mail front over the
//!   existing `SourceShell` (`bloomery/source.rs`): the guest sends
//!   `aether.source.*` transact mail for the port operations — snapshot /
//!   checkpoint / checkpoints / integrate / land — and the capability, which
//!   holds the token and network client, runs them and replies. ADR-0149 bars
//!   the wasm guest from touching tokens, shells, or the network directly, the
//!   same gap the store port already closed with `StoreCapability`.
//! - [`signing`] — the native `aether.signing` capability, the single
//!   statement-signature custody point (ADR-0149 step 3, ADR-0150/ADR-0151). It
//!   loads the authorized-signer allowlist host-local (credentials never leave
//!   the machine), constructs the real `Ed25519KeyProvider`, and serves an
//!   `aether.signing.verify` request the live answer gate dials — replacing the
//!   fake always-valid provider the gate verified against inline. Who may sign
//!   in a person's stead is the allowlist, capability key policy, never reducer
//!   logic.
//! - [`api`] — the native `aether.bloomery.api` capability, a REST control
//!   ingress mounted on the `aether.http.server` cap (ADR-0108) that drives the
//!   bloom lifecycle from `curl` — stage / shape / seal / supersede and read
//!   the live blooms, view document, journal, and artifacts (ADR-0149
//!   §Packaging, #3498).
//! - [`bloomery`] — [`BloomeryChassis`], a
//!   coordinator-shaped chassis (no render/audio surface) that registers the
//!   store, artifacts, source, trace, RPC, HTTP (REST control api), and the
//!   `aether.bloomery.mirror` outbox reactor capability behind a
//!   signal-blocking driver. It also holds the GitHub port cap shells —
//!   `ProjectionShell` (outward mirror), `SourceShell` (git source), and
//!   `ExecutorShell` (the Actions dispatch backend, ADR-0149 migration step 2)
//!   — each mounting an `aether-bloomery-github` backend behind an
//!   `Arc<dyn …>` so no core module names a GitHub type. The mirror reactor
//!   polls the store outbox and drives the `ProjectionShell` so a live bloomery
//!   continuously projects its journal to GitHub (config-gated off when
//!   unconfigured); `SourceShell` is also wired behind the `aether.source`
//!   capability above.
//!
//! Recovery is journal replay + outbox republish: reopen the same database
//! file, replay the journal through the reducer, and republish undelivered
//! outbox entries — the mirror reactor's first drain pass on boot is that
//! republish, acking each topic's delivered prefix only after the GitHub write
//! succeeds (at-least-once with idempotent reconcile).

#[cfg(feature = "github")]
pub mod app_auth;
pub mod artifacts;
pub mod control;
pub mod session;
pub mod signing;
pub mod source;
pub mod store;
pub use control::ControlCore;

#[cfg(feature = "runtime")]
pub mod api;
#[cfg(feature = "runtime")]
pub mod bloomery;

#[cfg(feature = "runtime")]
pub use api::BloomeryApiCapability;
#[cfg(feature = "runtime")]
pub use bloomery::{BloomeryChassis, BloomeryEnv};
