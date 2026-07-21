//! aether-chassis-desktop: the desktop chassis (ADR-0035/ADR-0073,
//! issue #3812) — winit event loop, wgpu renderer, capture queue, cpal
//! audio. Produces the `aether-substrate` binary over the shared
//! `aether-chassis` composition layer.
//! Issue 603 retired the chassis-side control-plane handler that
//! pre-Phases-2-4 owned `capture_frame` / window kinds /
//! `platform_info` — each kind now has its own cap (or, for
//! `platform_info`, was deleted entirely).

pub mod chassis;
pub mod driver;
pub mod render;

pub use chassis::{DesktopChassis, DesktopEnv, UserEvent};
pub use driver::{DesktopDriverCapability, DesktopDriverRunning};

pub use aether_chassis::autoload::AutoloadComponent;
