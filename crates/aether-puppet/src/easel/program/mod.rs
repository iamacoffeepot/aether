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
//! Three modules rasterize rather than paint fullscreen (ADR-0171).
//! [`bake`] (iamacoffeepot/aether#4411) authors the painter's input
//! maps, which [`super::regions::rasterize`] walks the subject's faces
//! for, as draw passes over the subject's own geometry.
//!
//! [`sight`] (iamacoffeepot/aether#4418, ADR-0172) rasterizes the
//! subject into a depth image and turns [`crate::visibility::runs`] —
//! the ray-per-point occlusion walk that was the frame budget's binding
//! constraint — into a field over each stroke's own parameterization.
//! [`stroke`] rasterizes the drawing through that field, and reduces the
//! raster it draws into the ink coverage plane the wash's flow is solved
//! off (iamacoffeepot/aether#4451) — so where paint yields boundary duty
//! and where ink actually stands are one answer rather than two.

/// The skinning prelude, appended after a program's own WGSL wherever a
/// vertex stage stands on a posed surface (iamacoffeepot/aether#4462).
///
/// Concatenated rather than duplicated because three programs pose the
/// same subject from the same bone table and a second transcription of a
/// blend is a second thing to get wrong. It reads `params.bones`, so a
/// program that appends it declares that member on its own uniform
/// block; nothing here binds anything.
pub const SKIN_WGSL: &str = include_str!("skin.wgsl");

/// The lighting prelude, appended the same way wherever a stage has to
/// answer `extract::Settings::tone` against a normal the pose turned.
///
/// It reads `params.light`, `params.ambient` and `params.face_lift`.
pub const TONE_WGSL: &str = include_str!("tone.wgsl");

pub mod bake;
pub mod care;
pub mod face;
pub mod flow;
pub mod pigment;
pub mod puddle;
pub mod sheet;
pub mod sight;
pub mod stroke;
pub mod wash;
