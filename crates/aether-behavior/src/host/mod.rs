//! The in-cluster behavior script host (ADR-0137, issue 2687) — the crate's
//! third face, behind the non-default `host` feature.
//!
//! Where the default face is the script SDK and the trunk is the host↔script
//! envelope, this module is the *host* side: [`BehaviorHost`], a non-generic
//! wasm `#[actor]` that embeds a `wasmi` interpreter and interposes at a tree
//! slot. It spawns its wrapped child by type tag (#2692), offers the mail
//! flowing through the slot to a fuel-metered filter call, fails open on a
//! trap (passthrough + disable-after-threshold), drains effects
//! verdict-then-effects with echo suppression, handles the
//! `aether.behavior.{load_script,set_script}` control kinds (fs-fetch through
//! `aether.fs`), and persists one bundle that re-instantiates only its own
//! script on reload (the wrapped child reconstructs through the composite
//! walk, #2694).
//!
//! The `host` feature turns on the optional `aether-actor` / `wasmi` deps —
//! neither named by the default face — so a behavior script never links them
//! and never misclassifies as a component.

mod actor;
pub mod config;
mod drain;
mod persist;
mod slot;

#[cfg(test)]
mod test_support;

pub use actor::BehaviorHost;
pub use config::{ChildSpec, HostConfig, LoadScript, LoadScriptResult, ScriptSource, SetScript};
