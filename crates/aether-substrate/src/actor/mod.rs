//! The actor primitive — components and capabilities collapse into
//! one shape (ADR-0074) sharing a single SDK over two transport impls.
//!
//! - [`native`] — chassis-cap actors compiled in (Rust state + Rust
//!   dispatcher).
//! - [`wasm`] — guest components loaded as wasm trampolines.
//! - [`registry`] — cross-flavour actor lifecycle table (live /
//!   tombstoned ids + monitor entries).
//! - [`monitor`] — RAII handle returned by `NativeCtx::monitor` (see
//!   ADR-0079); cross-flavour because both wasm trampolines and native
//!   caps participate in monitor fan-out.

pub mod monitor;
pub mod native;
pub mod registry;
pub mod wasm;

pub use monitor::MonitorHandle;
pub use registry::{ActorEntry, ActorRegistry, MonitorEntry, MonitorError};
