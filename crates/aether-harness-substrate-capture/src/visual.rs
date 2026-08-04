//! Visual assertions over decoded frame pixels — re-exported from
//! [`aether_substrate::render::visual`], where ADR-0161 §Decision 4 rehomed
//! the scorer so the pumped render runtime in `aether-render` (which depends
//! on `aether-substrate`) can call `run_checks` / `score_similarity` without
//! a dependency cycle. This module keeps the `crate::visual::…` path its
//! assertion consumers, `artifacts`, and the round-trip tests already use;
//! the scoring, decode, and reduction logic all live below now.

pub use aether_substrate::render::visual::*;

/// The encoder the chassis writes a capture with, so a scenario that
/// builds a frame of its own — a difference image against a baseline,
/// say — writes it in the same format `decode_png` above reads.
pub use aether_substrate::render::encode_png;
