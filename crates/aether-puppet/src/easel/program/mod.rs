//! The wash as authored render programs (ADR-0170).
//!
//! The GPU develop is the CPU one in [`super::field`] re-spoken as WGSL:
//! the substrate executes a registered pass graph, and everything the
//! sheet needs — the [`super::field::WashAccidents`] serialized as a
//! uniform blob, the [`super::field::NoisePlanes`] uploaded as textures —
//! is data authored here. Nothing rolls dice on the GPU, and the CPU
//! develop stays the oracle its parity scenarios paint against.
//!
//! The op vocabulary lands in three file-disjoint slices, one module
//! each, so their pull requests never contend on these declarations:
//! [`puddle`] (iamacoffeepot/aether#4366), [`pigment`]
//! (iamacoffeepot/aether#4367) and [`sheet`] (iamacoffeepot/aether#4368).
//! Each slice owns its own `.wgsl` and scenario files under this
//! directory.

pub mod pigment;
pub mod puddle;
pub mod sheet;
