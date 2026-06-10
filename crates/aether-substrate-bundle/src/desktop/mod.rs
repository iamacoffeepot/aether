//! Desktop chassis: winit event loop, wgpu renderer, capture queue.
//! Issue 603 retired the chassis-side control-plane handler that
//! pre-Phases-2-4 owned `capture_frame` / window kinds /
//! `platform_info` — each kind now has its own cap (or, for
//! `platform_info`, was deleted entirely).

pub mod chassis;
pub mod driver;
pub mod render;

pub use chassis::{DesktopChassis, DesktopEnv, UserEvent};
pub use driver::{DesktopDriverCapability, DesktopDriverRunning};

pub use crate::autoload::AutoloadComponent;
