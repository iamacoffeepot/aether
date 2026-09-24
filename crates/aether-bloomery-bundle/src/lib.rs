//! Author facade for the bloomery `bundle` export generator.
//!
//! Exact author form:
//!
//! ```ignore
//! aether_actor::export!(
//!     public = [Summarize, SourcePublisher],
//!     generators = [aether_bloomery_bundle::bundle],
//! );
//! ```
//!
//! The generator replaces every `#[program]` and `#[reactor]` in `exports`
//! with one hidden bundle root at [`BUNDLE_NAMESPACE`]. A module provides
//! programs, reactors, or both; a module with neither role fails to compile.
//! Authors depend on this facade plus the SDK crate they author with:
//! `aether-bloomery-program` for `#[program]`, `aether-bloomery-reactor` for
//! `#[reactor]`.

#![no_std]
#![forbid(unsafe_code)]

mod export;

#[doc(hidden)]
pub use aether_bloomery_bundle_derive::__bundle_export_generate;
pub use aether_bloomery_kinds::{BUNDLE_NAMESPACE, PROGRAMS_SECTION};

#[doc(hidden)]
pub mod __macro_internals {
    pub use aether_bloomery_program;
    pub use aether_bloomery_reactor;
}
