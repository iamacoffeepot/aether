//! The engine's pointer vocabulary lowered onto winit's.
//!
//! A lookup, not a policy: [`CursorIcon`] names the gesture a hovered element
//! affords and winit names the platform shape, so the only thing this file
//! decides is which of winit's many shapes each of the twelve engine names
//! lowers to. The diagonal pair is the arm worth reading twice — *rising*
//! runs bottom-left to top-right, which is CSS's (and winit's) `nesw`.

use winit::window::CursorIcon as WinitCursorIcon;

use crate::CursorIcon;

pub(super) fn map_cursor_icon(icon: CursorIcon) -> WinitCursorIcon {
    match icon {
        CursorIcon::Default => WinitCursorIcon::Default,
        CursorIcon::Pointer => WinitCursorIcon::Pointer,
        CursorIcon::Text => WinitCursorIcon::Text,
        CursorIcon::Move => WinitCursorIcon::Move,
        CursorIcon::ResizeHorizontal => WinitCursorIcon::EwResize,
        CursorIcon::ResizeVertical => WinitCursorIcon::NsResize,
        CursorIcon::ResizeDiagonalRising => WinitCursorIcon::NeswResize,
        CursorIcon::ResizeDiagonalFalling => WinitCursorIcon::NwseResize,
        CursorIcon::Grab => WinitCursorIcon::Grab,
        CursorIcon::Grabbing => WinitCursorIcon::Grabbing,
        CursorIcon::NotAllowed => WinitCursorIcon::NotAllowed,
        CursorIcon::Wait => WinitCursorIcon::Wait,
    }
}
