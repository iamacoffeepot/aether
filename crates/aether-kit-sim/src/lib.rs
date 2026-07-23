//! `aether-kit-sim` — the reference game-loop pair.
//!
//! [`TurnSim`] and [`PlayerClient`] extracted from `aether-kit`
//! (iamacoffeepot/aether#3952). Each is one module under the crate root that
//! co-locates the actor with its own `kinds` submodule (the mail shapes peers
//! send it) — guest code all the way down, so there is no data/runtime split,
//! just one module per actor:
//!
//! - [`TurnSim`] — the deterministic, tick-native reference turn simulation,
//!   selected by the `aether_kit_sim@aether.kit.sim` export (ADR-0096). Its
//!   tick-native intent, trajectory, summary, and catch-up vocabulary lives in
//!   [`sim`].
//! - [`PlayerClient`] — the outbound player-session and authoritative
//!   presentation actor, selected by the `aether_kit_sim@aether.kit.client`
//!   export. Its `aether.kit.client.config` init-config lives in [`client`].
//!
//! The pair consumes the `CellPos` / `WorldPoint` / octimeter position
//! vocabulary of the sibling [`aether-kit-terrain`](aether_kit_terrain) crate
//! (iamacoffeepot/aether#3951) and the tick-native intent / fact vocabulary of
//! `aether-game`. Actor namespaces are unchanged from their `aether-kit` life.
//!
//! `export!` (below) packs the pair into one cdylib (ADR-0096 multi-actor
//! module) with no default entry — each is selector-only (`module@actor`,
//! ADR-0138) — and the FFI shims it emits are wasm32-only and inert in a host
//! rlib, so the integration tests link the same artifact.

extern crate alloc;

pub mod client;
pub mod sim;

pub use client::{PlayerClient, PlayerClientConfig};
pub use sim::{
    CellPosition, EntityState, GridBounds, MoveDirection, MoveIntent, Poll, PollResult, SimConfig, Spawn, StateSummary,
    TickBundle, TrajectoryEvent, TrajectoryKind, TurnSim,
};

// A cdylib carries one `export!` (the shared init/receive FFI entry); the
// macro emits the wasm32 FFI shims and the `aether.kinds` custom section for
// every listed actor, all behind the macro's own `cfg(not(feature =
// "library"))` gate. This crate has no bare-load target — each actor is loaded
// by its `module@actor` selector (ADR-0138 defaultless policy), so the
// `export!` names no default. Nothing embeds this pair in another cdylib, so
// the crate declares no `library` feature of its own.
aether_actor::export!(sim::TurnSim, client::PlayerClient);
