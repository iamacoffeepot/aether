//! Native capabilities (ADR-0070). Phase 1 lands the empty module;
//! phases 2–5 populate it one submodule per extracted sink:
//!
//! - `handle.rs` (Phase 2 — least state, validates the trait shape)
//! - `log.rs`
//! - `io.rs`
//! - `net.rs`
//! - `audio.rs` (gated by `audio` feature)
//! - `render.rs` (gated by `render` feature; owns `aether.sink.render`
//!   and `aether.sink.camera`)
//!
//! `HubClientCapability` and `HubServerCapability` live in the
//! `aether-hub` crate (Phase 4–5), not here, so the kernel ends with
//! zero hub knowledge.
