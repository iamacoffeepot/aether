//! Winit lowering for the window boot knobs. The env/argv grammar
//! (`WindowConfig`, `parse_window_mode_env`) lives in `aether-chassis`
//! (the fleet-wide config registry names its derived layer); this
//! module keeps only the monitor/video-mode matching that needs winit.

use aether_kinds::WindowMode;
use winit::monitor::{MonitorHandle, VideoModeHandle};
use winit::window::Fullscreen;

/// Find a `VideoModeHandle` on `monitor` matching the given size +
/// refresh exactly. Returns `None` if no match — the caller surfaces
/// this as `SetWindowModeResult::Err` rather than falling back
/// silently to something close.
fn find_exclusive_mode(monitor: &MonitorHandle, width: u32, height: u32, refresh_mhz: u32) -> Option<VideoModeHandle> {
    monitor
        .video_modes()
        .find(|m| m.size().width == width && m.size().height == height && m.refresh_rate_millihertz() == refresh_mhz)
}

/// Build winit's `Option<Fullscreen>` for the requested mode.
/// `monitor_for_exclusive` is the monitor to match video modes
/// against — the window's current monitor at runtime, the primary at
/// boot.
pub(super) fn resolve_fullscreen(
    mode: &WindowMode,
    monitor_for_exclusive: Option<&MonitorHandle>,
) -> Result<Option<Fullscreen>, String> {
    match mode {
        WindowMode::Windowed => Ok(None),
        WindowMode::FullscreenBorderless => Ok(Some(Fullscreen::Borderless(None))),
        WindowMode::FullscreenExclusive { width, height, refresh_mhz } => {
            let monitor = monitor_for_exclusive
                .ok_or_else(|| "fullscreen-exclusive requested but no monitor available".to_owned())?;
            let handle = find_exclusive_mode(monitor, *width, *height, *refresh_mhz).ok_or_else(|| {
                format!("no video mode matches {width}x{height}@{refresh_mhz}mhz on monitor {:?}", monitor.name())
            })?;
            Ok(Some(Fullscreen::Exclusive(handle)))
        }
    }
}
