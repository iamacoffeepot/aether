//! Input and window event kind vocabulary.
//!
//! Every window-originated event starts with the canonical actor path of the
//! window that produced it (`aether.window/aether.window.instance:main`), the
//! same text `aether.window.list` reports. A path is heap text, so the family
//! rides the structured wire path and none of its kinds is `Copy` or has a
//! default.

use aether_data::ErasedActorPath;

/// A single keyboard keypress, identified by the stable codes in
/// `keycode`. Dispatched on press only (no repeat). Released keys
/// arrive as `KeyRelease`. Unmapped winit keys (any `KeyCode` variant
/// the substrate doesn't translate) produce no mail.
#[aether_data::kind(name = "aether.key", eq)]
pub struct Key {
    pub window: ErasedActorPath,
    pub code: u32,
}

/// Release counterpart of `Key`. Dispatched once per key release, with
/// the same `code` value the press carried. Components tracking
/// hold-to-act semantics (e.g. WASD movement) pair subscription to
/// both kinds so they can clear state on release.
#[aether_data::kind(name = "aether.key_release", eq)]
pub struct KeyRelease {
    pub window: ErasedActorPath,
    pub code: u32,
}

/// A mouse-button press. `button` identifies which button via the
/// `mouse_button` constant space (`LEFT` / `RIGHT` / `MIDDLE` / …);
/// `x` / `y` carry the cursor position at press time in window
/// coordinates, matching `MouseMove` — so a click event is
/// self-contained and needs no external cursor correlation. Omits `Eq`
/// because the `f32` fields make it non-`Eq`, same as `MouseMove`. Those
/// coordinates are physical pixels, the same space `WindowSize` and
/// `QuadSpace::Screen` speak, so hit-testing a click against screen-space
/// geometry needs no scale conversion.
#[aether_data::kind(name = "aether.mouse_button", partial_eq)]
pub struct MouseButton {
    pub window: ErasedActorPath,
    pub button: u32,
    pub x: f32,
    pub y: f32,
}

/// Release counterpart of `MouseButton`. Dispatched once per button
/// release, carrying the same `button` code the press carried and the
/// cursor position at release time. Components tracking press-move-release
/// drag pair subscription to both kinds so they can commit on release.
/// `x` / `y` are physical pixels, the same space `WindowSize` and
/// `QuadSpace::Screen` speak.
#[aether_data::kind(name = "aether.mouse_button_release", partial_eq)]
pub struct MouseButtonRelease {
    pub window: ErasedActorPath,
    pub button: u32,
    pub x: f32,
    pub y: f32,
}

/// A mouse-wheel scroll. `delta_x` / `delta_y` carry the scroll amount
/// (line deltas normalized to pixels by the driver); `x` / `y` carry the
/// cursor position at scroll time in window coordinates, so wheel-zoom-at-
/// cursor needs no external cursor correlation. `x` / `y` — and a
/// touchpad's pixel-precise deltas — are physical pixels, the same space
/// `WindowSize` and `QuadSpace::Screen` speak.
#[aether_data::kind(name = "aether.mouse_wheel", partial_eq)]
pub struct MouseWheel {
    pub window: ErasedActorPath,
    pub delta_x: f32,
    pub delta_y: f32,
    pub x: f32,
    pub y: f32,
}

/// Cursor position in window coordinates, as physical pixels cast to f32
/// — the desktop chassis forwards winit's `CursorMoved` position, which is
/// a `PhysicalPosition`, without converting it. That is the same space
/// `WindowSize.width` / `height` and `QuadSpace::Screen` use, so pointing
/// the cursor at screen-space geometry is a direct comparison and needs no
/// `scale_factor` anywhere in it. A consumer that genuinely wants logical
/// pixels divides by `WindowSize.scale_factor`; the conversion runs away
/// from this kind, never toward it.
#[aether_data::kind(name = "aether.mouse_move", partial_eq)]
pub struct MouseMove {
    pub window: ErasedActorPath,
    pub x: f32,
    pub y: f32,
}

/// Current window size in physical pixels, plus the display's scale
/// factor: `physical = logical × scale_factor`.
///
/// Every pixel quantity the engine puts on the wire is physical, and they
/// agree: `width` / `height` here, `QuadSpace::Screen` (the space solid
/// quads, textured quads, and `aether.render.draw_text` address), and the cursor
/// position on `MouseMove`, `MouseButton`, `MouseButtonRelease`, and
/// `MouseWheel`. So pointer-against-screen-space math — hover, hit-testing,
/// dragging a handle — compares the two directly and must **not** apply
/// `scale_factor`; doing so misses by exactly the scale factor on a
/// non-1x display.
///
/// `scale_factor` is for the conversions that cross into logical space,
/// which is where a *measure* rather than a coordinate lives: multiply to
/// size a logical UI quantity into physical pixels — a 16-logical-pixel
/// label, a 44-logical-pixel touch target — so it keeps its apparent size
/// on any display; divide to hand a consumer that genuinely wants logical
/// coordinates. At `scale_factor == 1.0` the two spaces coincide, which is
/// why a mistake in either direction is invisible on a standard-density
/// display and off by 2x on a 2x one.
///
/// Published by the desktop chassis on startup (once the window exists),
/// on every `WindowEvent::Resized` that isn't a zero-dimension minimize,
/// and on `WindowEvent::ScaleFactorChanged` (dragging a window between
/// displays of different density) so a cached value never goes stale.
/// Headless and hub chassis never publish — they have no window. Neither
/// does the synthetic harness window manager: it derives no size of its
/// own, so a harness test injects whatever value it wants observed, and
/// every in-tree one injects `scale_factor: 1.0`. A client that needs to
/// map pixel-space input (e.g. `MouseMove`) to clip-space geometry
/// subscribes to this kind and caches the latest value. A subscription
/// delivers changes from then on, so a component that wires after the window
/// opened reads the current `width`, `height`, and `scale_factor` from the
/// `aether.window.list` reply, then keeps them current from this kind.
#[aether_data::kind(name = "aether.window_size", partial_eq)]
pub struct WindowSize {
    pub window: ErasedActorPath,
    pub width: u32,
    pub height: u32,
    /// Physical pixels per logical pixel, as the display reports it
    /// (winit's `Window::scale_factor`). `1.0` on a standard-density
    /// display, and in every in-tree harness injection.
    pub scale_factor: f32,
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
/// Carries a `String`, so it rides the structured wire path shared by the
/// window-tagged input family (`Kind::encode_into_bytes` → `encode_wire`).
#[aether_data::kind(name = "aether.text_input", eq)]
pub struct TextInput {
    pub window: ErasedActorPath,
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
#[aether_data::kind(name = "aether.ime_preedit", eq)]
pub struct ImePreedit {
    pub window: ErasedActorPath,
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
/// Windows key elsewhere. A late subscriber has no value until the first
/// `ModifiersChanged` arrives — the same warm-up every stream has — and
/// reads that absence as no modifier held.
#[aether_data::kind(name = "aether.modifiers", eq)]
// Four named bool fields are the wire contract: a machine consumer reads
// `{ "shift": true }` off the JSON schema directly rather than decoding a
// packed bit mask. A two-variant-enum refactor would defeat that.
#[allow(clippy::struct_excessive_bools)]
pub struct Modifiers {
    pub window: ErasedActorPath,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}
