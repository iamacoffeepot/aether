//! Input and window event kind vocabulary.
//!
//! Every window-originated event starts with the engine-owned [`WindowId`]
//! that produced it. The family uses the structured wire path uniformly:
//! placing a `u64` identity before several of the legacy `u32` payloads would
//! otherwise introduce architecture-dependent `#[repr(C)]` padding.

use alloc::string::String;

use aether_data::schema::{LabelNode, SchemaType};
use aether_data::{MailboxId, Schema};
use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

/// Stable engine-owned identity for one window.
///
/// This is deliberately distinct from platform window identifiers such as
/// `winit::window::WindowId`: it is portable across native and guest code and
/// remains meaningful in traces and replay data.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable, Serialize, Deserialize)]
pub struct WindowId(pub u64);

/// A window id *is* a [`MailboxId`] — ADR-0164 addresses window-originated
/// input through the window's own actor identity, and the desktop window
/// manager mints this from that actor's ADR-0099 lineage fold. Declaring it
/// as one is what gives it the ADR-0064 tagged-string form
/// (`mbx-q3lr-bv2x-mtdr`) at the JSON boundary.
///
/// Which is not cosmetic. A lineage fold occupies the top of the `u64`
/// range — around 2^60 — and the derived `Scalar(U64)` schema this replaces
/// rendered it as a bare JSON number, where a consumer parsing numbers as
/// doubles quantises it to the nearest multiple of 256. So the id an agent
/// read back from `aether.window.list` could not be handed to
/// `capture_frame`: `1473705000037674430` returned as `...674500`, and
/// desktop capture was unaddressable (iamacoffeepot/aether#4344).
///
/// The wire encoding is unchanged — `TypeId` is a fixed 8-byte
/// little-endian field exactly as `Scalar(U64)` was, and the codec still
/// accepts a plain number on the way in.
impl Schema for WindowId {
    const SCHEMA: SchemaType = SchemaType::TypeId(MailboxId::TYPE_ID);
    const LABEL: Option<&'static str> = Some(MailboxId::TYPE_NAME);
    const LABEL_NODE: LabelNode = LabelNode::Anonymous;
}

/// A single keyboard keypress, identified by the stable codes in
/// `keycode`. Dispatched on press only (no repeat). Released keys
/// arrive as `KeyRelease`. Unmapped winit keys (any `KeyCode` variant
/// the substrate doesn't translate) produce no mail.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, aether_data::Kind, aether_data::Schema, Serialize, Deserialize,
)]
#[kind(name = "aether.key")]
pub struct Key {
    pub window: WindowId,
    pub code: u32,
}

/// Release counterpart of `Key`. Dispatched once per key release, with
/// the same `code` value the press carried. Components tracking
/// hold-to-act semantics (e.g. WASD movement) pair subscription to
/// both kinds so they can clear state on release.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, aether_data::Kind, aether_data::Schema, Serialize, Deserialize,
)]
#[kind(name = "aether.key_release")]
pub struct KeyRelease {
    pub window: WindowId,
    pub code: u32,
}

/// A mouse-button press. `button` identifies which button via the
/// `mouse_button` constant space (`LEFT` / `RIGHT` / `MIDDLE` / …);
/// `x` / `y` carry the cursor position at press time in window
/// coordinates, matching `MouseMove` — so a click event is
/// self-contained and needs no external cursor correlation. Omits `Eq`
/// because the `f32` fields make it non-`Eq`, same as `MouseMove`.
#[derive(Copy, Clone, Debug, Default, PartialEq, aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "aether.mouse_button")]
pub struct MouseButton {
    pub window: WindowId,
    pub button: u32,
    pub x: f32,
    pub y: f32,
}

/// Release counterpart of `MouseButton`. Dispatched once per button
/// release, carrying the same `button` code the press carried and the
/// cursor position at release time. Components tracking press-move-release
/// drag pair subscription to both kinds so they can commit on release.
#[derive(Copy, Clone, Debug, Default, PartialEq, aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "aether.mouse_button_release")]
pub struct MouseButtonRelease {
    pub window: WindowId,
    pub button: u32,
    pub x: f32,
    pub y: f32,
}

/// A mouse-wheel scroll. `delta_x` / `delta_y` carry the scroll amount
/// (line deltas normalized to pixels by the driver); `x` / `y` carry the
/// cursor position at scroll time in window coordinates, so wheel-zoom-at-
/// cursor needs no external cursor correlation.
#[derive(Copy, Clone, Debug, Default, PartialEq, aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "aether.mouse_wheel")]
pub struct MouseWheel {
    pub window: WindowId,
    pub delta_x: f32,
    pub delta_y: f32,
    pub x: f32,
    pub y: f32,
}

/// Cursor position in window coordinates, as logical pixels cast to f32.
#[derive(Copy, Clone, Debug, Default, PartialEq, aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "aether.mouse_move")]
pub struct MouseMove {
    pub window: WindowId,
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
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, aether_data::Kind, aether_data::Schema, Serialize, Deserialize,
)]
#[kind(name = "aether.window_size")]
pub struct WindowSize {
    pub window: WindowId,
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
/// Carries a `String`, so it rides the structured wire path shared by the
/// window-tagged input family (`Kind::encode_into_bytes` → `encode_wire`).
#[derive(Clone, Debug, Default, PartialEq, Eq, aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "aether.text_input")]
pub struct TextInput {
    pub window: WindowId,
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
#[derive(Clone, Debug, Default, PartialEq, Eq, aether_data::Kind, aether_data::Schema, Serialize, Deserialize)]
#[kind(name = "aether.ime_preedit")]
pub struct ImePreedit {
    pub window: WindowId,
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
    Copy, Clone, Debug, Default, PartialEq, Eq, aether_data::Kind, aether_data::Schema, Serialize, Deserialize,
)]
#[kind(name = "aether.modifiers")]
// Four named bool fields are the wire contract: a machine consumer reads
// `{ "shift": true }` off the JSON schema directly rather than decoding a
// packed bit mask. A two-variant-enum refactor would defeat that.
#[allow(clippy::struct_excessive_bools)]
pub struct Modifiers {
    pub window: WindowId,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

#[cfg(test)]
mod tests {
    use aether_data::{Kind, Schema, SchemaType};

    use super::{
        ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
        WindowSize,
    };

    fn assert_window_is_leading_field<K: Schema>() {
        let SchemaType::Struct { fields, repr_c } = &K::SCHEMA else {
            panic!("window input kind must have a struct schema");
        };
        assert_eq!(fields.first().map(|field| field.name.as_ref()), Some("window"));
        assert!(!repr_c, "window input kinds use the structured wire path");
    }

    #[test]
    fn window_identity_is_leading_and_changes_every_input_kind_id() {
        assert_window_is_leading_field::<Key>();
        assert_window_is_leading_field::<KeyRelease>();
        assert_window_is_leading_field::<MouseButton>();
        assert_window_is_leading_field::<MouseButtonRelease>();
        assert_window_is_leading_field::<MouseWheel>();
        assert_window_is_leading_field::<MouseMove>();
        assert_window_is_leading_field::<WindowSize>();
        assert_window_is_leading_field::<TextInput>();
        assert_window_is_leading_field::<ImePreedit>();
        assert_window_is_leading_field::<Modifiers>();

        for (name, current, legacy) in [
            (Key::NAME, Key::ID.0, 0x2cd4_71a8_6d5a_45c3),
            (KeyRelease::NAME, KeyRelease::ID.0, 0x29af_edc4_d29e_66b9),
            (MouseButton::NAME, MouseButton::ID.0, 0x2ae2_bffd_3539_0765),
            (MouseButtonRelease::NAME, MouseButtonRelease::ID.0, 0x25b3_c586_4948_a587),
            (MouseWheel::NAME, MouseWheel::ID.0, 0x200e_f6be_b9c5_c7fc),
            (MouseMove::NAME, MouseMove::ID.0, 0x23d7_5eca_383f_2613),
            (WindowSize::NAME, WindowSize::ID.0, 0x2987_ca5c_c96a_8043),
            (TextInput::NAME, TextInput::ID.0, 0x225a_4efe_afb7_7bcf),
            (ImePreedit::NAME, ImePreedit::ID.0, 0x25ad_e0e7_6687_d8db),
            (Modifiers::NAME, Modifiers::ID.0, 0x2ddb_7336_fd9e_18ce),
        ] {
            assert_ne!(current, legacy, "{name} retained its single-window schema id");
        }
    }
}
