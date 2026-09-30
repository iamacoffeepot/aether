//! The bloomery `bundle` export generator.
//!
//! Exact author form:
//!
//! ```ignore
//! aether_actor::export!(
//!     public = [Summarize, SourcePublisher],
//!     generators = [aether_bloomery_program::bundle],
//! );
//! ```
//!
//! The generator replaces every `#[program]` and `#[reactor]` in `exports`
//! with one hidden bundle root at [`crate::BUNDLE_NAMESPACE`]. A module
//! provides programs, reactors, or both; a module with neither role fails to
//! compile.

#![forbid(unsafe_code)]

mod export;
