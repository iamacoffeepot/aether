//! Stable identifiers for mouse buttons, carried in `MouseButton.button`
//! and `MouseButtonRelease.button`. These are the engine's own named u32
//! space — decoupled from winit's `MouseButton` discriminants, the same
//! way `keycode` decouples from winit's `KeyCode`.
//!
//! The substrate maps `winit::event::MouseButton → u32` via the constants
//! below; components match on these constants. Unmapped buttons (winit's
//! `Other(n)`) produce no mail, mirroring the unmapped-key contract in
//! `keycode`.

/// Primary button — the left button on a right-handed mouse.
pub const LEFT: u32 = 0;
/// Secondary button — the right button on a right-handed mouse.
pub const RIGHT: u32 = 1;
/// Middle button — usually the scroll-wheel click.
pub const MIDDLE: u32 = 2;
/// Back button — the thumb button that navigates backward.
pub const BACK: u32 = 3;
/// Forward button — the thumb button that navigates forward.
pub const FORWARD: u32 = 4;
