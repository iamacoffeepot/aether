//! Render/GPU capture support for `aether-harness-substrate` (issue #3765,
//! ADR-0161): the [`GpuFrameHook`] that owns the pumped `aether.render`
//! slot and the builder / harness extensions that boot and read it, the
//! image-compare and `FrameCheck` scoring in [`visual`], and the
//! failure-only [`ArtifactGuard`]. [`RenderHarnessExt::program_gpu_timings`]
//! reads GPU timestamp state without capture readback or PNG encode. Split
//! from the core so only visual consumers carry the aether-render + wgpu edge
//! in their `cargo xtask affected` closure.
//!
//! A visual test composes render with the builder extension and reads
//! overlay state with the harness extension:
//!
//! ```ignore
//! use aether_harness_substrate::SubstrateHarness;
//! use aether_harness_substrate_capture::{RenderHarnessBuilderExt, RenderHarnessExt};
//!
//! let mut harness = SubstrateHarness::builder().size(64, 48).with_render().build()?;
//! let png = harness.execute(vec![("snap", HarnessOp::capture())])?;
//! let overlays = harness.committed_overlay_snapshot();
//! ```

pub mod artifacts;
mod ext;
pub mod test_helpers;
pub mod visual;

pub use aether_render::{PassTimingRow, ProgramTimingsResult};
pub use artifacts::ArtifactGuard;
pub use ext::{GpuFrameHook, RenderHarnessBuilderExt, RenderHarnessExt};
