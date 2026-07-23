//! Editor-wide input routing between independently-rooted peer regions.
//!
//! [`Routing`] owns only deterministic state. The [`EditorShell`](super::EditorShell)
//! actor owns subscriptions and turns these named effects into raw mail sends.

use alloc::vec::Vec;

use aether_data::MailboxId;
use aether_kinds::keycode::KEY_TAB;
use aether_kinds::{Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel};
use aether_math::{Aabb, Vec3};

use super::{EditorKeyChord, EditorRegionRect, RegionInputLanes, RegionSpec};

/// One raw input lane considered by the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionInputLane {
    PointerPress,
    PointerRelease,
    PointerMotion,
    Wheel,
    KeyPress,
    KeyRelease,
    TextInput,
    ImePreedit,
    Modifiers,
}

impl RegionInputLanes {
    #[must_use]
    pub const fn accepts(self, lane: RegionInputLane) -> bool {
        match lane {
            RegionInputLane::PointerPress => self.pointer_press,
            RegionInputLane::PointerRelease => self.pointer_release,
            RegionInputLane::PointerMotion => self.pointer_motion,
            RegionInputLane::Wheel => self.wheel,
            RegionInputLane::KeyPress => self.key_press,
            RegionInputLane::KeyRelease => self.key_release,
            RegionInputLane::TextInput => self.text_input,
            RegionInputLane::ImePreedit => self.ime_preedit,
            RegionInputLane::Modifiers => self.modifiers,
        }
    }
}

impl EditorKeyChord {
    #[must_use]
    pub const fn matches(self, key: Key, modifiers: Modifiers) -> bool {
        let key_code = key.code;
        self.key_code == key_code
            && self.shift == modifiers.shift
            && self.ctrl == modifiers.ctrl
            && self.alt == modifiers.alt
            && self.meta == modifiers.meta
    }
}

/// Region-level pointer capture, tied to the button that established it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionPressOwner {
    pub target: MailboxId,
    pub button: u32,
}

/// Direction of the reserved editor-region focus cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionFocusDirection {
    Forward,
    Backward,
}

/// One editor-region focus edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionFocusTransition {
    pub previous: Option<MailboxId>,
    pub next: Option<MailboxId>,
}

/// A pointer press route plus any focus edge it caused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionPointerRoute {
    pub target: Option<MailboxId>,
    pub focus: Option<RegionFocusTransition>,
}

/// A key route. Reserved editor chords are consumed with no target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionKeyRoute {
    pub target: Option<MailboxId>,
    pub focus: Option<RegionFocusTransition>,
    pub consumed: bool,
}

#[derive(Debug, Clone)]
struct RegionEntry {
    target: MailboxId,
    rect: Aabb,
    keyboard_focus_eligible: bool,
    input_lanes: RegionInputLanes,
    activation_chord: Option<EditorKeyChord>,
}

impl RegionEntry {
    fn from_spec(spec: &RegionSpec) -> Option<Self> {
        valid_rect(spec.rect).then(|| Self {
            target: spec.target,
            rect: Aabb::from_min_max(
                Vec3::new(spec.rect.x_pixels, spec.rect.y_pixels, 0.0),
                Vec3::new(
                    spec.rect.x_pixels + spec.rect.width_pixels,
                    spec.rect.y_pixels + spec.rect.height_pixels,
                    0.0,
                ),
            ),
            keyboard_focus_eligible: spec.keyboard_focus_eligible,
            input_lanes: spec.input_lanes,
            activation_chord: spec.activation_chord,
        })
    }
}

fn valid_rect(rect: EditorRegionRect) -> bool {
    rect.x_pixels.is_finite()
        && rect.y_pixels.is_finite()
        && rect.width_pixels.is_finite()
        && rect.height_pixels.is_finite()
        && rect.width_pixels > 0.0
        && rect.height_pixels > 0.0
        && (rect.x_pixels + rect.width_pixels).is_finite()
        && (rect.y_pixels + rect.height_pixels).is_finite()
}

/// Deterministic editor-wide routing state. Holds no mailbox or capability
/// handle; callers turn its named targets/transitions into sends.
#[derive(Default)]
pub struct Routing {
    entries: Vec<RegionEntry>,
    focused: Option<MailboxId>,
    modifiers: Modifiers,
    cycle_armed: bool,
    press_owner: Option<RegionPressOwner>,
}

impl Routing {
    #[must_use]
    pub fn new(regions: &[RegionSpec]) -> Self {
        Self { entries: regions.iter().filter_map(RegionEntry::from_spec).collect(), ..Self::default() }
    }

    #[must_use]
    pub fn focused(&self) -> Option<MailboxId> {
        self.focused
    }

    #[must_use]
    pub const fn cached_modifiers(&self) -> Modifiers {
        self.modifiers
    }

    #[must_use]
    pub fn press_owner(&self) -> Option<RegionPressOwner> {
        self.press_owner
    }

    #[must_use]
    pub fn target_accepts(&self, target: MailboxId, lane: RegionInputLane) -> bool {
        self.accepting_target(target, lane).is_some()
    }

    #[must_use]
    pub fn hit_test(&self, x_pixels: f32, y_pixels: f32) -> Option<MailboxId> {
        if !x_pixels.is_finite() || !y_pixels.is_finite() {
            return None;
        }
        let point = Vec3::new(x_pixels, y_pixels, 0.0);
        self.entries.iter().rev().find(|entry| entry.rect.contains_point(point)).map(|entry| entry.target)
    }

    pub fn pointer_press(&mut self, press: MouseButton) -> RegionPointerRoute {
        if let Some(owner) = self.press_owner {
            return RegionPointerRoute {
                target: self.accepting_target(owner.target, RegionInputLane::PointerPress),
                focus: None,
            };
        }
        let target = self
            .hit_test(press.x, press.y)
            .and_then(|target| self.accepting_target(target, RegionInputLane::PointerPress));
        let focus = target.and_then(|target| {
            self.press_owner = Some(RegionPressOwner { target, button: press.button });
            self.focus_target(target)
        });
        RegionPointerRoute { target, focus }
    }

    pub fn pointer_release(&mut self, release: MouseButtonRelease) -> Option<MailboxId> {
        if let Some(owner) = self.press_owner {
            if release.button == owner.button {
                self.press_owner = None;
            }
            return self.accepting_target(owner.target, RegionInputLane::PointerRelease);
        }
        self.hit_test(release.x, release.y)
            .and_then(|target| self.accepting_target(target, RegionInputLane::PointerRelease))
    }

    #[must_use]
    pub fn pointer_motion(&self, moved: MouseMove) -> Option<MailboxId> {
        if let Some(owner) = self.press_owner {
            return self.accepting_target(owner.target, RegionInputLane::PointerMotion);
        }
        self.hit_test(moved.x, moved.y).and_then(|target| self.accepting_target(target, RegionInputLane::PointerMotion))
    }

    #[must_use]
    pub fn wheel(&self, wheel: MouseWheel) -> Option<MailboxId> {
        self.hit_test(wheel.x, wheel.y).and_then(|target| self.accepting_target(target, RegionInputLane::Wheel))
    }

    pub fn key_press(&mut self, key: Key) -> RegionKeyRoute {
        if key.code == KEY_TAB && self.modifiers.ctrl {
            self.cycle_armed = true;
            let direction = if self.modifiers.shift {
                RegionFocusDirection::Backward
            } else {
                RegionFocusDirection::Forward
            };
            return RegionKeyRoute { target: None, focus: self.move_focus(direction), consumed: true };
        }

        if let Some(target) = self.activation_target(key) {
            let focus = self.focus_target(target);
            return RegionKeyRoute {
                target: self.accepting_target(target, RegionInputLane::KeyPress),
                focus,
                consumed: false,
            };
        }

        RegionKeyRoute { target: self.focused_target(RegionInputLane::KeyPress), focus: None, consumed: false }
    }

    pub fn key_release(&mut self, release: KeyRelease) -> RegionKeyRoute {
        if release.code == KEY_TAB && self.cycle_armed {
            self.cycle_armed = false;
            return RegionKeyRoute { target: None, focus: None, consumed: true };
        }
        RegionKeyRoute { target: self.focused_target(RegionInputLane::KeyRelease), focus: None, consumed: false }
    }

    pub fn modifiers(&mut self, modifiers: Modifiers) -> Option<MailboxId> {
        self.modifiers = modifiers;
        self.focused_target(RegionInputLane::Modifiers)
    }

    #[must_use]
    pub fn text_input_target(&self) -> Option<MailboxId> {
        self.focused_target(RegionInputLane::TextInput)
    }

    #[must_use]
    pub fn ime_preedit_target(&self) -> Option<MailboxId> {
        self.focused_target(RegionInputLane::ImePreedit)
    }

    fn activation_target(&self, key: Key) -> Option<MailboxId> {
        self.entries
            .iter()
            .find(|entry| {
                entry.keyboard_focus_eligible
                    && entry.activation_chord.is_some_and(|chord| chord.matches(key, self.modifiers))
            })
            .map(|entry| entry.target)
    }

    fn accepting_target(&self, target: MailboxId, lane: RegionInputLane) -> Option<MailboxId> {
        self.entries
            .iter()
            .find(|entry| entry.target == target)
            .filter(|entry| entry.input_lanes.accepts(lane))
            .map(|entry| entry.target)
    }

    fn focused_target(&self, lane: RegionInputLane) -> Option<MailboxId> {
        self.focused.and_then(|target| self.accepting_target(target, lane))
    }

    fn focus_target(&mut self, target: MailboxId) -> Option<RegionFocusTransition> {
        let eligible = self.entries.iter().any(|entry| entry.target == target && entry.keyboard_focus_eligible);
        if !eligible || self.focused == Some(target) {
            return None;
        }
        let previous = self.focused;
        self.focused = Some(target);
        Some(RegionFocusTransition { previous, next: Some(target) })
    }

    fn move_focus(&mut self, direction: RegionFocusDirection) -> Option<RegionFocusTransition> {
        let count = self.entries.len();
        if count == 0 {
            return None;
        }
        let current = self.focused.and_then(|target| self.entries.iter().position(|entry| entry.target == target));
        for offset in 0..count {
            let index = match (direction, current) {
                (RegionFocusDirection::Forward, Some(index)) => (index + 1 + offset) % count,
                (RegionFocusDirection::Forward, None) => offset,
                (RegionFocusDirection::Backward, Some(index)) => (index + count - 1 - offset) % count,
                (RegionFocusDirection::Backward, None) => count - 1 - offset,
            };
            if self.entries[index].keyboard_focus_eligible {
                return self.focus_target(self.entries[index].target);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use aether_kinds::WindowId;

    use super::*;

    const TEST_WINDOW_ID: WindowId = WindowId(1);

    fn rect(x_pixels: f32, y_pixels: f32, width_pixels: f32, height_pixels: f32) -> EditorRegionRect {
        EditorRegionRect { x_pixels, y_pixels, width_pixels, height_pixels }
    }

    fn region(target: u64, rect: EditorRegionRect, keyboard: bool, input_lanes: RegionInputLanes) -> RegionSpec {
        RegionSpec {
            name: format!("region-{target}"),
            rect,
            target: MailboxId(target),
            keyboard_focus_eligible: keyboard,
            input_lanes,
            activation_chord: None,
        }
    }

    fn press(button: u32, x: f32, y: f32) -> MouseButton {
        MouseButton { window: TEST_WINDOW_ID, button, x, y }
    }

    fn release(button: u32, x: f32, y: f32) -> MouseButtonRelease {
        MouseButtonRelease { window: TEST_WINDOW_ID, button, x, y }
    }

    #[test]
    fn overlap_chooses_topmost_and_lane_rejection_does_not_fall_through() {
        let lower = region(1, rect(0.0, 0.0, 20.0, 20.0), true, RegionInputLanes::ALL);
        let upper = region(2, rect(10.0, 10.0, 20.0, 20.0), true, RegionInputLanes::default());
        let mut routing = Routing::new(&[lower, upper]);

        assert_eq!(routing.hit_test(15.0, 15.0), Some(MailboxId(2)));
        assert_eq!(routing.pointer_press(press(0, 15.0, 15.0)).target, None);
        assert_eq!(routing.press_owner(), None);
        assert_eq!(routing.pointer_press(press(0, 5.0, 5.0)).target, Some(MailboxId(1)));
    }

    #[test]
    fn first_press_owns_cross_region_motion_until_its_matching_release() {
        let mut routing = Routing::new(&[
            region(1, rect(0.0, 0.0, 10.0, 10.0), true, RegionInputLanes::ALL),
            region(2, rect(20.0, 0.0, 10.0, 10.0), true, RegionInputLanes::ALL),
        ]);

        let route = routing.pointer_press(press(0, 5.0, 5.0));
        assert_eq!(route.target, Some(MailboxId(1)));
        assert_eq!(route.focus, Some(RegionFocusTransition { previous: None, next: Some(MailboxId(1)) }));
        assert_eq!(routing.pointer_press(press(1, 25.0, 5.0)).target, Some(MailboxId(1)));
        assert_eq!(routing.pointer_motion(MouseMove { window: TEST_WINDOW_ID, x: 25.0, y: 5.0 }), Some(MailboxId(1)));

        assert_eq!(routing.pointer_release(release(1, 25.0, 5.0)), Some(MailboxId(1)));
        assert_eq!(routing.press_owner(), Some(RegionPressOwner { target: MailboxId(1), button: 0 }));
        assert_eq!(routing.pointer_release(release(0, 25.0, 5.0)), Some(MailboxId(1)));
        assert_eq!(routing.press_owner(), None);
        assert_eq!(routing.pointer_motion(MouseMove { window: TEST_WINDOW_ID, x: 25.0, y: 5.0 }), Some(MailboxId(2)));
    }

    #[test]
    fn release_without_owner_and_wheel_route_by_current_hit_and_lane() {
        let mut no_wheel = RegionInputLanes::ALL;
        no_wheel.wheel = false;
        let mut routing = Routing::new(&[
            region(1, rect(0.0, 0.0, 10.0, 10.0), true, RegionInputLanes::ALL),
            region(2, rect(20.0, 0.0, 10.0, 10.0), true, no_wheel),
        ]);

        assert_eq!(routing.pointer_release(release(0, 25.0, 5.0)), Some(MailboxId(2)));
        assert_eq!(
            routing.wheel(MouseWheel { window: TEST_WINDOW_ID, delta_x: 0.0, delta_y: 2.0, x: 25.0, y: 5.0 }),
            None
        );
        assert_eq!(
            routing.wheel(MouseWheel { window: TEST_WINDOW_ID, delta_x: 0.0, delta_y: 2.0, x: 5.0, y: 5.0 }),
            Some(MailboxId(1))
        );
    }

    #[test]
    fn ctrl_tab_cycles_regions_reverse_with_shift_and_plain_tab_routes() {
        let mut routing = Routing::new(&[
            region(1, rect(0.0, 0.0, 10.0, 10.0), true, RegionInputLanes::ALL),
            region(2, rect(20.0, 0.0, 10.0, 10.0), false, RegionInputLanes::ALL),
            region(3, rect(40.0, 0.0, 10.0, 10.0), true, RegionInputLanes::ALL),
        ]);
        routing.pointer_press(press(0, 5.0, 5.0));
        routing.pointer_release(release(0, 5.0, 5.0));

        routing.modifiers(Modifiers { window: TEST_WINDOW_ID, ctrl: true, ..Modifiers::default() });
        let forward = routing.key_press(Key { window: TEST_WINDOW_ID, code: KEY_TAB });
        assert!(forward.consumed);
        assert_eq!(forward.target, None);
        assert_eq!(
            forward.focus,
            Some(RegionFocusTransition { previous: Some(MailboxId(1)), next: Some(MailboxId(3)) })
        );
        assert!(routing.key_release(KeyRelease { window: TEST_WINDOW_ID, code: KEY_TAB }).consumed);

        routing.modifiers(Modifiers { window: TEST_WINDOW_ID, ctrl: true, shift: true, ..Modifiers::default() });
        assert_eq!(
            routing.key_press(Key { window: TEST_WINDOW_ID, code: KEY_TAB }).focus,
            Some(RegionFocusTransition { previous: Some(MailboxId(3)), next: Some(MailboxId(1)) })
        );
        routing.key_release(KeyRelease { window: TEST_WINDOW_ID, code: KEY_TAB });

        routing.modifiers(Modifiers { window: TEST_WINDOW_ID, ..Modifiers::default() });
        let plain = routing.key_press(Key { window: TEST_WINDOW_ID, code: KEY_TAB });
        assert!(!plain.consumed);
        assert_eq!(plain.target, Some(MailboxId(1)));
        assert_eq!(plain.focus, None);
    }

    #[test]
    fn activation_chord_focuses_and_lane_filters_keyboard_text_and_ime() {
        let first = region(1, rect(0.0, 0.0, 10.0, 10.0), true, RegionInputLanes::ALL);
        let mut second_lanes = RegionInputLanes::ALL;
        second_lanes.text_input = false;
        second_lanes.ime_preedit = false;
        let mut second = region(2, rect(20.0, 0.0, 10.0, 10.0), true, second_lanes);
        second.activation_chord = Some(EditorKeyChord { key_code: 96, ..EditorKeyChord::default() });
        let mut routing = Routing::new(&[first, second]);

        routing.pointer_press(press(0, 5.0, 5.0));
        routing.pointer_release(release(0, 5.0, 5.0));
        let activation = routing.key_press(Key { window: TEST_WINDOW_ID, code: 96 });
        assert_eq!(activation.target, Some(MailboxId(2)));
        assert_eq!(
            activation.focus,
            Some(RegionFocusTransition { previous: Some(MailboxId(1)), next: Some(MailboxId(2)) })
        );
        assert_eq!(routing.text_input_target(), None);
        assert_eq!(routing.ime_preedit_target(), None);
        assert_eq!(routing.key_release(KeyRelease { window: TEST_WINDOW_ID, code: 96 }).target, Some(MailboxId(2)));
    }

    #[test]
    fn empty_invalid_and_all_unfocusable_tables_stay_unrouted() {
        let mut empty = Routing::new(&[]);
        assert_eq!(empty.pointer_press(press(0, 1.0, 1.0)).target, None);
        empty.modifiers(Modifiers { window: TEST_WINDOW_ID, ctrl: true, ..Modifiers::default() });
        assert_eq!(empty.key_press(Key { window: TEST_WINDOW_ID, code: KEY_TAB }).focus, None);

        let invalid = region(1, rect(0.0, 0.0, f32::NAN, 10.0), true, RegionInputLanes::ALL);
        let overflowing = region(3, rect(f32::MAX, 0.0, f32::MAX, 10.0), true, RegionInputLanes::ALL);
        let static_region = region(2, rect(20.0, 0.0, 10.0, 10.0), false, RegionInputLanes::ALL);
        let mut routing = Routing::new(&[invalid, overflowing, static_region]);
        assert_eq!(routing.hit_test(1.0, 1.0), None);
        assert_eq!(routing.pointer_press(press(0, 25.0, 5.0)).focus, None);
        routing.modifiers(Modifiers { window: TEST_WINDOW_ID, ctrl: true, ..Modifiers::default() });
        assert_eq!(routing.key_press(Key { window: TEST_WINDOW_ID, code: KEY_TAB }).focus, None);
    }
}
