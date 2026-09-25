//! Tests for the native ctx surface, one sibling per production module.
//!
//! [`address`], [`handles`], [`inbound`], [`registry`], [`send`] and [`store`] are named for the
//! module whose behaviour they exercise; [`mode`] covers the layout
//! invariant the `mod.rs` coercions rest on and the per-mode reachability of
//! the reply / emit surfaces. [`blob_mail`] follows a `Blob` field through
//! the typed sends in `send` and the inbound decode in `inbound`, across
//! actors. [`support`] holds the stub actors, peers, and kinds more than one
//! of them shares.

mod address;
mod blob_mail;
mod handles;
mod inbound;
mod mode;
mod registry;
mod send;
mod store;
mod support;
