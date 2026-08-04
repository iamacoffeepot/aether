//! The wash as authored render programs (ADR-0170).
//!
//! The GPU develop is the CPU one in [`super::field`] re-spoken as WGSL:
//! the substrate executes a registered pass graph, and everything the
//! sheet needs — the [`super::field::WashAccidents`] serialized as a
//! uniform blob, the [`super::field::NoisePlanes`] uploaded as textures —
//! is data authored here. Nothing rolls dice on the GPU, and the CPU
//! develop stays the oracle its parity scenarios paint against.
//!
//! The op vocabulary landed in three file-disjoint slices, one module
//! each, so their pull requests never contended on these declarations:
//! [`puddle`] (iamacoffeepot/aether#4366), [`pigment`]
//! (iamacoffeepot/aether#4367) and [`sheet`] (iamacoffeepot/aether#4368).
//! [`wash`] (iamacoffeepot/aether#4369) is the coat sequencer that
//! composes them into the one registered wash program and encodes its
//! per-develop uniform blob. Each module owns its own `.wgsl` and
//! scenario files under this directory.
//!
//! Two modules rasterize rather than paint fullscreen, and they are the
//! two halves of the same move off the CPU (ADR-0171). [`bake`]
//! (iamacoffeepot/aether#4411) authors the painter's input maps, which
//! [`super::regions::rasterize`] walks the subject's faces for, as draw
//! passes over the subject's own geometry; [`ink`]
//! (iamacoffeepot/aether#4410) authors the coverage plane, which
//! [`super::regions::ink`] walks the ribbon triangles for, as a draw
//! pass over the drawing's own geometry. Both stand beside the wash
//! rather than under it for now — it still paints from CPU-baked
//! planes, and each is held against that oracle until the switch-over.
//!
//! [`sight`] (iamacoffeepot/aether#4418, ADR-0172) is the third of that
//! shape and the one that does not answer to the wash at all: it
//! rasterizes the subject into a depth image and turns
//! [`crate::visibility::runs`] — the ray-per-point occlusion walk that
//! is the frame budget's binding constraint — into a field over each
//! stroke's own parameterization. It stands beside `visibility::runs`
//! the way the two above stand beside their oracles.

pub mod bake;
pub mod care;
pub mod face;
pub mod flow;
pub mod ink;
pub mod pigment;
pub mod puddle;
pub mod sheet;
pub mod sight;
pub mod stroke;
pub mod wash;
