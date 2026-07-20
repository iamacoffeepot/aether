//! Render/GPU capture support for `aether-substrate-bench` (issue
//! #3765): the offscreen wgpu pipeline ([`Gpu`]), the seam
//! implementations that plug it into the core bench
//! ([`GpuRenderExt`] / [`GpuFrameHook`]), the image-compare and
//! `FrameCheck` scoring in [`visual`], and the failure-only
//! [`ArtifactGuard`]. Split from the core so only visual consumers
//! carry the aether-render + wgpu edge in their `cargo xtask affected`
//! closure.
//!
//! A visual test composes render with the builder extension and reads
//! overlay state with the bench extension:
//!
//! ```ignore
//! use aether_substrate_bench::SubstrateBench;
//! use aether_substrate_bench_capture::{RenderBenchBuilderExt, RenderBenchExt};
//!
//! let mut bench = SubstrateBench::builder().size(64, 48).with_render().build()?;
//! let png = bench.execute(vec![("snap", BenchOp::capture())])?;
//! let overlays = bench.committed_overlay_snapshot();
//! ```

pub mod artifacts;
mod ext;
mod gpu;
pub mod test_helpers;
pub mod visual;

pub use artifacts::ArtifactGuard;
pub use ext::{GpuFrameHook, GpuRenderExt, RenderBenchBuilderExt, RenderBenchExt};
pub use gpu::Gpu;
