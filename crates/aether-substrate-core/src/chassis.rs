//! The ADR-0035 universal `Chassis` trait, refined by ADR-0071.
//!
//! Each chassis binary implements it over whatever peripheral layer
//! it has — winit + wgpu for desktop, a std timer + stdio for
//! headless, a TCP listener + MCP surface for hub, etc. The trait
//! stays deliberately narrow: identity (`PROFILE`) plus lifecycle
//! (`run`). Chassis-specific control-plane kinds (e.g. desktop's
//! `capture_frame` / `set_window_mode` / `platform_info`) ride
//! through `ControlPlane::chassis_handler`, not through trait
//! methods — that keeps any single chassis from having to implement
//! `Unsupported` stubs for operations it doesn't support.
//!
//! Consumed at boot: `main()` constructs a concrete chassis + hands
//! it the core state it needs, then calls `chassis.run()` which
//! takes ownership and drives the event loop to termination.
//!
//! ADR-0071 phase 2A renames the previous `KIND` const to `PROFILE`
//! (avoiding the data-layer `Kind` / `KindId` / `KindShape` /
//! `KindLabels` clobber) and removes the `CAPABILITIES` const + the
//! associated `ChassisCapabilities` struct. The flag-shaped const
//! was right for ADR-0035's hardcoded chassis but gets wrong post-
//! ADR-0071 since a chassis can in principle compose any combination
//! of capabilities, not just the original `(gpu, window,
//! tcp_listener)` set. Self-describing introspection — a runtime
//! method on the chassis returning the actual booted capability
//! list — is the planned replacement; tracked as follow-on work.
//!
//! Phases 3-7 of ADR-0071 replace each chassis's `run()` body with a
//! `DriverCapability`-driven path; once that lands the trait
//! collapses further to identity-only.

/// The lifecycle contract a chassis implements. Every concrete
/// chassis is `Sized` (binary crates pick exactly one at compile
/// time), so `PROFILE` is a compile-time const and `run(self)` takes
/// ownership.
pub trait Chassis: Sized {
    /// Stable identifier for this chassis. Used in boot logs and
    /// wherever the chassis needs to identify itself to an observer.
    /// `"desktop"`, `"headless"`, `"hub"`, etc.
    ///
    /// Renamed from the ADR-0035 `KIND` const by ADR-0071 to avoid
    /// clobbering the data layer's `Kind` vocabulary.
    const PROFILE: &'static str;

    /// Take ownership of the chassis and drive its event loop to
    /// termination. Returns when the event source exits cleanly
    /// (desktop: winit close; headless: SIGTERM or shutdown mail;
    /// hub: all connections drained). Propagates any unrecoverable
    /// startup or runtime error as `wasmtime::Result<()>` — matches
    /// `main()`'s current return shape so binaries can `chassis.run()?`
    /// without glue.
    fn run(self) -> wasmtime::Result<()>;
}
