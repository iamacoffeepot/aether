//! The ADR-0035 universal `Chassis` trait. Each chassis binary
//! implements it over whatever peripheral layer it has — winit +
//! wgpu for desktop, a std timer + stdio for headless, a TCP
//! listener + MCP surface for hub, etc. The trait stays deliberately
//! narrow: only lifecycle + introspection, nothing chassis-specific.
//! Chassis-specific control-plane kinds (e.g. desktop's
//! `capture_frame` / `set_window_mode` / `platform_info`) ride
//! through `ControlPlane::chassis_handler`, not through trait
//! methods — that keeps any single chassis from having to implement
//! `Unsupported` stubs for operations it doesn't support.
//!
//! Consumed at boot: `main()` constructs a concrete chassis + hands
//! it the core state it needs, then calls `chassis.run()` which
//! takes ownership and drives the event loop to termination.

/// Describes what a chassis natively supports. Informational — used
/// by boot logs today, likely by a future `describe_chassis` MCP
/// tool and hub-side `platform_info` composition. Add fields as new
/// chassis targets arrive (e.g. `has_audio` once an audio chassis
/// layer exists). The default is all-false; each chassis overrides
/// only the flags it sets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChassisCapabilities {
    pub has_gpu: bool,
    pub has_window: bool,
    pub has_tcp_listener: bool,
}

/// The lifecycle contract a chassis implements. Every concrete
/// chassis is `Sized` (binary crates pick exactly one at compile
/// time), so `KIND` + `CAPABILITIES` are compile-time consts and
/// `run(self)` takes ownership.
pub trait Chassis: Sized {
    /// Short identifier for this chassis. Used in boot logs and
    /// wherever the chassis needs to identify itself to an observer.
    /// `"desktop"`, `"headless"`, `"hub"`, etc.
    const KIND: &'static str;

    /// Feature flags describing what this chassis natively supports.
    /// Callers that want to decide behaviour based on capability
    /// (e.g. only attempt a `capture_frame` call on chassis where
    /// `has_gpu && has_window`) read this at compile time via
    /// `<T as Chassis>::CAPABILITIES`.
    const CAPABILITIES: ChassisCapabilities;

    /// Take ownership of the chassis and drive its event loop to
    /// termination. Returns when the event source exits cleanly
    /// (desktop: winit close; headless: SIGTERM or shutdown mail;
    /// hub: all connections drained). Propagates any unrecoverable
    /// startup or runtime error as `wasmtime::Result<()>` — matches
    /// `main()`'s current return shape so binaries can `chassis.run()?`
    /// without glue.
    fn run(self) -> wasmtime::Result<()>;
}
