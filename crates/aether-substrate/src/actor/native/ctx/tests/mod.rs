//! Tests for the native ctx surface, one sibling per production module.
//!
//! [`address`], [`handles`], [`inbound`] and [`send`] are named for the
//! module whose behaviour they exercise; [`mode`] covers the layout
//! invariant the `mod.rs` coercions rest on and the per-mode reachability of
//! the reply / emit surfaces. [`support`] holds the stub actors, peers, and
//! kinds more than one of them shares.

mod address;
mod handles;
mod inbound;
mod mode;
mod send;
mod support;
