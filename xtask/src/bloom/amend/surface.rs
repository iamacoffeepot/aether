//! The granularity rule, re-exported from the domain crate.
//!
//! The rewrite itself lives in `aether-bloomery` beside the
//! `unnamed_file_entries` the seal door refuses on, because the coordinator's
//! own auto-tier grant (ADR-0207) has to admit a requested path exactly the way
//! this command does — two copies of that rule would be two answers to "what
//! did the estate actually grant".

pub use aether_bloomery::widen;
