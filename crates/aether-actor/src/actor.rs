//! Marker traits for the actor model. Re-exported from `aether-data`
//! (`pub use aether_data::Actor;` etc.).
//!
//! These were originally declared here, but ADR-0075's facade pattern
//! puts chassis cap structs in `aether-kinds`, which forced a move to
//! break the dependency cycle (`aether-actor` → `aether-kinds` for
//! `aether.control.subscribe_input`, `aether-kinds` → `aether-actor`
//! for the marker traits). The traits themselves are pure compile-time
//! markers with no transport machinery, so `aether-data` (the
//! universal data layer) is a fitting home — and both `aether-actor`
//! and `aether-kinds` depend on `aether-data`, no cycle.
//!
//! See `aether_data::actor` for the trait definitions.

pub use aether_data::{Actor, HandlesKind, Singleton};
