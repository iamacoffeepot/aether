//! Stable identifiers for keyboard keys, carried in `Key.code` and
//! `KeyRelease.code`. These are the engine's own named u32 space —
//! decoupled from winit's `KeyCode` discriminants, which have no
//! stability guarantee across winit versions (the enum's repr is
//! `#[repr(u32)]` but variant ordering is not a public contract).
//!
//! The substrate maps `winit::keyboard::KeyCode → u32` via the
//! per-variant constants below; components match on these constants.
//! Unmapped keys (any winit variant not listed here) produce no mail.
//!
//! Value convention:
//!
//! - Printable ASCII letters use their uppercase ASCII codepoint
//!   (`KEY_A = 0x41`, `KEY_W = 0x57`, …). This keeps ASCII-range
//!   values self-documenting in logs and tool output.
//! - Digits use their ASCII codepoint (`KEY_0 = 0x30`, …).
//! - Control keys (arrows, space, escape, modifiers) use values above
//!   `0xFF` so ASCII values stay free for future printable keys.

/// Letters — uppercase ASCII.
pub const KEY_A: u32 = b'A' as u32;
pub const KEY_B: u32 = b'B' as u32;
pub const KEY_C: u32 = b'C' as u32;
pub const KEY_D: u32 = b'D' as u32;
pub const KEY_E: u32 = b'E' as u32;
pub const KEY_F: u32 = b'F' as u32;
pub const KEY_G: u32 = b'G' as u32;
pub const KEY_H: u32 = b'H' as u32;
pub const KEY_I: u32 = b'I' as u32;
pub const KEY_J: u32 = b'J' as u32;
pub const KEY_K: u32 = b'K' as u32;
pub const KEY_L: u32 = b'L' as u32;
pub const KEY_M: u32 = b'M' as u32;
pub const KEY_N: u32 = b'N' as u32;
pub const KEY_O: u32 = b'O' as u32;
pub const KEY_P: u32 = b'P' as u32;
pub const KEY_Q: u32 = b'Q' as u32;
pub const KEY_R: u32 = b'R' as u32;
pub const KEY_S: u32 = b'S' as u32;
pub const KEY_T: u32 = b'T' as u32;
pub const KEY_U: u32 = b'U' as u32;
pub const KEY_V: u32 = b'V' as u32;
pub const KEY_W: u32 = b'W' as u32;
pub const KEY_X: u32 = b'X' as u32;
pub const KEY_Y: u32 = b'Y' as u32;
pub const KEY_Z: u32 = b'Z' as u32;

/// Digits — ASCII codepoint.
pub const KEY_0: u32 = b'0' as u32;
pub const KEY_1: u32 = b'1' as u32;
pub const KEY_2: u32 = b'2' as u32;
pub const KEY_3: u32 = b'3' as u32;
pub const KEY_4: u32 = b'4' as u32;
pub const KEY_5: u32 = b'5' as u32;
pub const KEY_6: u32 = b'6' as u32;
pub const KEY_7: u32 = b'7' as u32;
pub const KEY_8: u32 = b'8' as u32;
pub const KEY_9: u32 = b'9' as u32;

/// Control keys — above the ASCII range so printable codepoints stay
/// free. Values are arbitrary-but-stable; additions go at the end.
pub const KEY_SPACE: u32 = 0x0100;
pub const KEY_ESCAPE: u32 = 0x0101;
pub const KEY_ENTER: u32 = 0x0102;
pub const KEY_TAB: u32 = 0x0103;
pub const KEY_BACKSPACE: u32 = 0x0104;

/// Arrow keys.
pub const KEY_LEFT: u32 = 0x0110;
pub const KEY_RIGHT: u32 = 0x0111;
pub const KEY_UP: u32 = 0x0112;
pub const KEY_DOWN: u32 = 0x0113;

/// Modifiers. One code per physical key (left and right shift are
/// distinct) — callers that don't care which side can match on both.
pub const KEY_SHIFT_LEFT: u32 = 0x0120;
pub const KEY_SHIFT_RIGHT: u32 = 0x0121;
pub const KEY_CTRL_LEFT: u32 = 0x0122;
pub const KEY_CTRL_RIGHT: u32 = 0x0123;
pub const KEY_ALT_LEFT: u32 = 0x0124;
pub const KEY_ALT_RIGHT: u32 = 0x0125;
