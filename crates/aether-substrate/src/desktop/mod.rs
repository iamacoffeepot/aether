//! Desktop chassis: winit event loop, wgpu renderer, capture queue,
//! and the chassis-side control-plane handler that owns the desktop-
//! only kinds (capture_frame, set_window_mode, platform_info).

pub mod chassis;
pub mod driver;
pub mod render;

pub use chassis::{DesktopChassis, DesktopEnv, UserEvent, chassis_control_handler};
pub use driver::{DesktopDriverCapability, DesktopDriverRunning};
