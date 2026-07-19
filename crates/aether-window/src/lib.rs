//! `aether.window` cap surface (issue 603 Phase 3).
//!
//! On desktop the chassis driver claims `aether.window` directly and
//! drains the inbox between frames — window mutations require the
//! chassis main thread (winit / macOS), and the driver is already
//! there. The driver-as-actor path lives in the desktop chassis
//! driver; this crate hosts the chassis-without-window
//! companion that headless and test-bench compose to fail-fast with
//! `Err`-replies on `set_mode` / `set_title`.

// Handler-signature kinds must be importable at module root because
// `#[actor]` emits `impl HandlesKind<K> for X {}` markers always-on,
// outside the `feature = "runtime"` gate.
use aether_kinds::{FocusWindow, SetWindowMode, SetWindowTitle};

use aether_actor::actor;

/// `aether.window` headless-companion cap **identity** (ADR-0122
/// identity/runtime split). A ZST carrying only the addressing — the
/// `Addressable` / `HandlesKind` markers and the name-inventory entry,
/// all emitted always-on by `#[actor]`. The state-bearing runtime
/// (`HeadlessWindowCapabilityState`) lives behind the one
/// `feature = "runtime"` gate, so a transport-only build never names it
/// nor pulls `aether_substrate` through this cap.
///
/// Chassis-without-window companion to the desktop driver's
/// driver-as-actor `aether.window` claim. Mirrors
/// `HeadlessRenderCapability`: same mailbox the desktop
/// owner claims, `Err`-replying handlers so MCP `set_window_mode`
/// / `set_window_title` fail fast on chassis without a window
/// (headless and test-bench).
///
/// Each chassis composes one of {desktop driver, this cap}, never
/// both — the chassis builder rejects double-claiming a mailbox.
#[actor(singleton)]
pub struct HeadlessWindowCapability;

// The reply kinds ride the native gate (not `runtime`): the `#[actor]`
// macro's ADR-0109 `HandlerEntry` inventory submission — emitted on every
// native build, runtime or not — names each handler's reply kind `::ID`,
// so a transport-only build must still see them. The `aether_substrate`-
// typed ctx imports and the empty state struct sit behind the one
// `feature = "runtime"` gate; the macro gates everything it emits for the
// runtime half, so this cap's identity compiles transport-only.
#[cfg(not(target_family = "wasm"))]
use aether_kinds::{FocusWindowResult, SetWindowModeResult, SetWindowTitleResult};

// The runtime half — the `aether_substrate`-typed ctx imports, the empty
// state struct, and the `#[runtime] impl` — lives in `runtime.rs`, gated
// once here. Nothing in this file names a runtime type directly, so there
// is no `use runtime::*` glob (matching `fs/mod.rs`).
#[cfg(feature = "runtime")]
mod runtime;
