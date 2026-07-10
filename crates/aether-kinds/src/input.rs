//! Input and window event kind vocabulary.

use alloc::string::String;

use bytemuck::{Pod, Zeroable};

/// A single keyboard keypress, identified by the stable codes in
/// `keycode`. Dispatched on press only (no repeat). Released keys
/// arrive as `KeyRelease`. Unmapped winit keys (any `KeyCode` variant
/// the substrate doesn't translate) produce no mail.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.key")]
pub struct Key {
    pub code: u32,
}

/// Release counterpart of `Key`. Dispatched once per key release, with
/// the same `code` value the press carried. Components tracking
/// hold-to-act semantics (e.g. WASD movement) pair subscription to
/// both kinds so they can clear state on release.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.key_release")]
pub struct KeyRelease {
    pub code: u32,
}

/// A mouse-button press. `button` identifies which button via the
/// `mouse_button` constant space (`LEFT` / `RIGHT` / `MIDDLE` / …);
/// `x` / `y` carry the cursor position at press time in window
/// coordinates, matching `MouseMove` — so a click event is
/// self-contained and needs no external cursor correlation. Omits `Eq`
/// because the `f32` fields make it non-`Eq`, same as `MouseMove`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.mouse_button")]
pub struct MouseButton {
    pub button: u32,
    pub x: f32,
    pub y: f32,
}

/// Release counterpart of `MouseButton`. Dispatched once per button
/// release, carrying the same `button` code the press carried and the
/// cursor position at release time. Components tracking press-move-release
/// drag pair subscription to both kinds so they can commit on release.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.mouse_button_release")]
pub struct MouseButtonRelease {
    pub button: u32,
    pub x: f32,
    pub y: f32,
}

/// A mouse-wheel scroll. `delta_x` / `delta_y` carry the scroll amount
/// (line deltas normalized to pixels by the driver); `x` / `y` carry the
/// cursor position at scroll time in window coordinates, so wheel-zoom-at-
/// cursor needs no external cursor correlation.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.mouse_wheel")]
pub struct MouseWheel {
    pub delta_x: f32,
    pub delta_y: f32,
    pub x: f32,
    pub y: f32,
}

/// Cursor position in window coordinates, as logical pixels cast to f32.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.mouse_move")]
pub struct MouseMove {
    pub x: f32,
    pub y: f32,
}

/// Current window size in physical pixels. Published by the desktop
/// chassis on startup (once the window exists) and on every
/// `WindowEvent::Resized` that isn't a zero-dimension minimize.
/// Headless and hub chassis never publish — they have no window. A
/// client that needs to map pixel-space input (e.g. `MouseMove`) to
/// clip-space geometry subscribes to this kind and caches the latest
/// value; the initial value arrives right after the component's
/// auto-subscribe fires, without any request/reply dance.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.window_size")]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

/// Committed, layout-resolved text input (`aether.text_input`) — one or
/// more characters the user typed, already translated through the active
/// keyboard layout and IME. Published by the desktop chassis from two
/// winit sources deduped by a composition gate: `KeyEvent.text` when no
/// IME composition is active, and `Ime::Commit` when one is, so a
/// character is never delivered twice. Unlike `Key` (a physical-scancode
/// edge event), this stream forwards key repeats — holding a key types a
/// run of characters. A text-field widget subscribes this and inserts
/// `text` at its caret with no guest-side scancode keymap. Headless and
/// hub chassis never publish — they have no window (same as `Key`).
/// `text` never carries a control character: named keys with a
/// control-char text representation (Backspace, Enter, Tab, Escape,
/// Delete) arrive only as `Key` scancode edges, and the chassis strips
/// any control characters winit's `KeyEvent.text` reports before
/// publishing.
///
/// Carries a `String`, so it rides the structured wire path
/// (`Kind::encode_into_bytes` → `encode_wire`), not the `#[repr(C)]`
/// cast path `Key` / `KeyRelease` use.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize,
)]
#[kind(name = "aether.text_input")]
pub struct TextInput {
    pub text: String,
}

/// In-flight IME composition (`aether.ime_preedit`) — the underlined,
/// not-yet-committed text a component renders inline while the user
/// composes. Mirrors winit's `Ime::Preedit(String, Option<(usize,
/// usize)>)`: `cursor_begin` / `cursor_end` are byte offsets into `text`
/// marking the cursor/selection span the IME reports (both `None` when
/// the IME gives no span). Empty `text` means the composition was
/// cleared — the widget drops any preedit it was showing. Published by
/// the desktop chassis only. Rides the structured wire path.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize,
)]
#[kind(name = "aether.ime_preedit")]
pub struct ImePreedit {
    pub text: String,
    pub cursor_begin: Option<u32>,
    pub cursor_end: Option<u32>,
}

/// Latest-wins keyboard modifier state (`aether.modifiers`) — the chord
/// keys currently held. Published by the desktop chassis on every
/// `WindowEvent::ModifiersChanged`, following the same caching contract
/// `WindowSize` documents: a component subscribes, caches the latest
/// value, and consults it when it receives a `Key` (e.g. to tell Ctrl+C
/// from a bare C). Named bool fields rather than a packed bit mask so a
/// machine consumer reading the JSON schema sees `{ "shift": true }`
/// directly. `meta` is the platform "super" key — Command on macOS, the
/// Windows key elsewhere. A late subscriber holds the all-false default
/// until the first `ModifiersChanged` arrives — the same warm-up every
/// stream has. Carries `bool`s, so it rides the structured wire path.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    aether_data::Kind,
    aether_data::Schema,
    serde::Serialize,
    serde::Deserialize,
)]
#[kind(name = "aether.modifiers")]
// Four named bool fields are the wire contract: a machine consumer reads
// `{ "shift": true }` off the JSON schema directly rather than decoding a
// packed bit mask. A two-variant-enum refactor would defeat that.
#[allow(clippy::struct_excessive_bools)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}
