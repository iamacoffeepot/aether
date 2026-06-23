//! Shared config defaults for the capabilities' `#[derive(Config)]`
//! structs (ADR-0090).
//!
//! The per-cap `parse_env` helpers that once lived here are gone: a
//! numeric / `Duration` / `bool` field rides confique's native env
//! deserialization (which trims, treats an empty value as unset →
//! default, and hard-errors on a non-empty garbage value, ADR-0090 §4),
//! a `csv_set` field auto-wires `aether_substrate::config::parse_csv_set`
//! through the derive, and a zero-is-degenerate knob carries the
//! `nonzero` hint. Only genuinely-bespoke parsers (the `aether.fs`
//! `parse_dir`) still name a `parse =` function.

/// Default per-cap concurrency bound shared by the content-gen
/// providers (`aether.gemini`, `aether.anthropic`) when their
/// `AETHER_*_MAX_IN_FLIGHT` env var is unset, non-positive, or
/// unparseable. ADR-0050.
pub const DEFAULT_PROVIDER_MAX_IN_FLIGHT: usize = 2;
