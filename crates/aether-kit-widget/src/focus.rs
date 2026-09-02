//! Root-owned focus, hover, and pointer-capture routing for one widget panel.
//!
//! Static widget eligibility and dynamic external availability are distinct:
//! labels never take focus or pointer input, while any stock control may become
//! hidden or disabled at runtime without losing its layout slot. The helper
//! owns no mail; it returns named transitions that the panel turns into mail.

use alloc::vec::Vec;

use aether_data::MailboxId;
use aether_math::{Aabb, Vec3};

use crate::WidgetControlState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusTransition {
    pub previous: Option<MailboxId>,
    pub next: Option<MailboxId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoverTransition {
    pub previous: Option<MailboxId>,
    pub next: Option<MailboxId>,
}

/// Cleanup caused by a live availability update. A focused child moves focus
/// forward through the remaining ring; hover and capture clear immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AvailabilityEffects {
    pub focus: Option<FocusTransition>,
    pub hover: Option<HoverTransition>,
    pub cleared_capture: Option<MailboxId>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FocusRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusEligibility {
    pub pointer: bool,
    pub keyboard: bool,
}

#[derive(Debug, Clone, Copy)]
struct Availability {
    visible: bool,
    enabled: bool,
}

struct Entry {
    child: MailboxId,
    rect: Aabb,
    eligibility: FocusEligibility,
    availability: Availability,
}

impl Entry {
    fn pointer_live(&self) -> bool {
        self.eligibility.pointer && self.availability.visible && self.availability.enabled
    }

    fn focus_live(&self) -> bool {
        self.eligibility.keyboard && self.availability.visible && self.availability.enabled
    }
}

#[derive(Default)]
pub struct Focus {
    entries: Vec<Entry>,
    focused: Option<MailboxId>,
    hovered: Option<MailboxId>,
    capture: Option<MailboxId>,
    /// The modal pointer grab an open dropdown or popover holds: every
    /// pointer event routes here until it ends, across releases, so a press
    /// outside the widget's own slot still reaches it (to select a row drawn
    /// in its overlay, or to dismiss). Outranks drag capture.
    grab: Option<MailboxId>,
}

impl Focus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.focused = None;
        self.hovered = None;
        self.capture = None;
        self.grab = None;
    }

    /// Route every pointer event to `child` until [`Self::end_grab`] — the
    /// modal grab a widget asks for while its overlay is open (a dropdown's
    /// list). Ignored for a child the table does not hold live.
    pub fn begin_grab(&mut self, child: MailboxId) {
        if self.entries.iter().any(|entry| entry.child == child && entry.pointer_live()) {
            self.grab = Some(child);
        }
    }

    /// End the modal grab, if any. Hover is not recomputed here: the next
    /// motion event re-derives it.
    pub fn end_grab(&mut self) {
        self.grab = None;
    }

    #[must_use]
    pub fn grabbed(&self) -> Option<MailboxId> {
        self.grab
    }

    /// Register one fixed layout entry. Dynamic visibility/enabled state may be
    /// updated later without rebuilding the table.
    pub fn register(
        &mut self,
        child: MailboxId,
        frame: FocusRect,
        eligibility: FocusEligibility,
        state: &WidgetControlState,
    ) {
        let rect = Aabb::from_min_max(
            Vec3::new(frame.x, frame.y, 0.0),
            Vec3::new(frame.x + frame.width, frame.y + frame.height, 0.0),
        );
        self.entries.push(Entry {
            child,
            rect,
            eligibility,
            availability: Availability { visible: state.visible, enabled: state.enabled },
        });
    }

    #[must_use]
    pub fn hit_test(&self, x: f32, y: f32) -> Option<MailboxId> {
        let point = Vec3::new(x, y, 0.0);
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.pointer_live() && entry.rect.contains_point(point))
            .map(|entry| entry.child)
    }

    #[must_use]
    pub fn pointer_target(&self, x: f32, y: f32) -> Option<MailboxId> {
        self.grab.or(self.capture).or_else(|| self.hit_test(x, y))
    }

    #[must_use]
    pub fn keyboard_target(&self) -> Option<MailboxId> {
        self.focused
    }

    pub fn begin_capture(&mut self, child: MailboxId) {
        if self.entries.iter().any(|entry| entry.child == child && entry.pointer_live()) {
            self.capture = Some(child);
        }
    }

    /// Clear capture and recompute hover at the release position.
    pub fn release_capture(&mut self, x: f32, y: f32) -> Option<HoverTransition> {
        self.capture = None;
        self.update_hover(x, y)
    }

    #[must_use]
    pub fn captured(&self) -> Option<MailboxId> {
        self.capture
    }

    pub fn set_focus(&mut self, next: Option<MailboxId>) -> Option<FocusTransition> {
        if let Some(child) = next
            && !self.entries.iter().any(|entry| entry.child == child && entry.focus_live())
        {
            return None;
        }
        if self.focused == next {
            return None;
        }
        let previous = self.focused;
        self.focused = next;
        Some(FocusTransition { previous, next })
    }

    /// The topmost keyboard-focusable child under the point, whether or not
    /// focusing it would change anything. A root needs this to tell "the press
    /// landed on a control" from "the press landed on nothing", because the
    /// second is what clears focus — and [`Self::focus_hit`] answers `None` to
    /// both.
    #[must_use]
    pub fn focus_hit_test(&self, x: f32, y: f32) -> Option<MailboxId> {
        let point = Vec3::new(x, y, 0.0);
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.focus_live() && entry.rect.contains_point(point))
            .map(|entry| entry.child)
    }

    pub fn focus_hit(&mut self, x: f32, y: f32) -> Option<FocusTransition> {
        self.set_focus(Some(self.focus_hit_test(x, y)?))
    }

    /// Move through the live focus ring in the requested direction, wrapping.
    pub fn move_focus(&mut self, direction: FocusDirection) -> Option<FocusTransition> {
        let count = self.entries.len();
        if count == 0 {
            return None;
        }
        let current = self.focused.and_then(|child| self.entries.iter().position(|entry| entry.child == child));
        for offset in 0..count {
            let index = match (direction, current) {
                (FocusDirection::Forward, Some(index)) => (index + 1 + offset) % count,
                (FocusDirection::Forward, None) => offset,
                (FocusDirection::Backward, Some(index)) => (index + count - 1 - offset) % count,
                (FocusDirection::Backward, None) => count - 1 - offset,
            };
            let entry = &self.entries[index];
            if entry.focus_live() {
                return self.set_focus(Some(entry.child));
            }
        }
        None
    }

    /// Recompute the child under the pointer independently from capture. The
    /// panel can therefore send hover edges while still routing raw motion to a
    /// drag captor.
    pub fn update_hover(&mut self, x: f32, y: f32) -> Option<HoverTransition> {
        let next = self.hit_test(x, y);
        if self.hovered == next {
            return None;
        }
        let previous = self.hovered;
        self.hovered = next;
        Some(HoverTransition { previous, next })
    }

    /// Apply a source-attributed state change. If the child becomes unavailable,
    /// move focus forward and clear live hover/capture paths immediately.
    pub fn update_availability(&mut self, child: MailboxId, state: &WidgetControlState) -> AvailabilityEffects {
        let Some(index) = self.entries.iter().position(|entry| entry.child == child) else {
            return AvailabilityEffects::default();
        };
        self.entries[index].availability = Availability { visible: state.visible, enabled: state.enabled };
        if state.visible && state.enabled {
            return AvailabilityEffects::default();
        }

        let mut effects = AvailabilityEffects::default();
        if self.focused == Some(child) {
            self.focused = None;
            let next = self.next_live_from(index, FocusDirection::Forward);
            self.focused = next;
            effects.focus = Some(FocusTransition { previous: Some(child), next });
        }
        if self.hovered == Some(child) {
            self.hovered = None;
            effects.hover = Some(HoverTransition { previous: Some(child), next: None });
        }
        if self.capture == Some(child) {
            self.capture = None;
            effects.cleared_capture = Some(child);
        }
        effects
    }

    fn next_live_from(&self, index: usize, direction: FocusDirection) -> Option<MailboxId> {
        let count = self.entries.len();
        for offset in 0..count {
            let candidate = match direction {
                FocusDirection::Forward => (index + 1 + offset) % count,
                FocusDirection::Backward => (index + count - 1 - offset) % count,
            };
            let entry = &self.entries[candidate];
            if entry.focus_live() {
                return Some(entry.child);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available() -> WidgetControlState {
        WidgetControlState::default()
    }

    fn register(focus: &mut Focus, child: u64, x: f32, y: f32, pointer: bool, keyboard: bool) {
        focus.register(
            MailboxId(child),
            FocusRect { x, y, width: 10.0, height: 10.0 },
            FocusEligibility { pointer, keyboard },
            &available(),
        );
    }

    fn focus_with_three() -> Focus {
        let mut focus = Focus::new();
        register(&mut focus, 1, 0.0, 0.0, true, true);
        register(&mut focus, 2, 0.0, 20.0, false, false);
        register(&mut focus, 3, 0.0, 40.0, true, true);
        focus
    }

    #[test]
    fn overlap_chooses_the_topmost_live_pointer_entry() {
        let mut focus = Focus::new();
        focus.register(
            MailboxId(1),
            FocusRect { x: 0.0, y: 0.0, width: 20.0, height: 20.0 },
            FocusEligibility { pointer: true, keyboard: true },
            &available(),
        );
        focus.register(
            MailboxId(2),
            FocusRect { x: 10.0, y: 10.0, width: 20.0, height: 20.0 },
            FocusEligibility { pointer: true, keyboard: true },
            &available(),
        );
        assert_eq!(focus.hit_test(15.0, 15.0), Some(MailboxId(2)));

        let mut hidden = available();
        hidden.visible = false;
        focus.update_availability(MailboxId(2), &hidden);
        assert_eq!(focus.hit_test(15.0, 15.0), Some(MailboxId(1)));
    }

    #[test]
    fn forward_and_backward_wrap_skip_static_and_unavailable_entries() {
        let mut focus = focus_with_three();
        assert_eq!(
            focus.move_focus(FocusDirection::Forward),
            Some(FocusTransition { previous: None, next: Some(MailboxId(1)) })
        );
        assert_eq!(
            focus.move_focus(FocusDirection::Backward),
            Some(FocusTransition { previous: Some(MailboxId(1)), next: Some(MailboxId(3)) })
        );

        let mut disabled = available();
        disabled.enabled = false;
        focus.update_availability(MailboxId(3), &disabled);
        assert_eq!(focus.keyboard_target(), Some(MailboxId(1)));
        assert_eq!(focus.move_focus(FocusDirection::Forward), None);

        focus.update_availability(MailboxId(3), &available());
        assert_eq!(
            focus.move_focus(FocusDirection::Forward),
            Some(FocusTransition { previous: Some(MailboxId(1)), next: Some(MailboxId(3)) })
        );
    }

    #[test]
    fn hover_reports_sibling_then_empty_edges() {
        let mut focus = focus_with_three();
        assert_eq!(focus.update_hover(5.0, 5.0), Some(HoverTransition { previous: None, next: Some(MailboxId(1)) }));
        assert_eq!(
            focus.update_hover(5.0, 45.0),
            Some(HoverTransition { previous: Some(MailboxId(1)), next: Some(MailboxId(3)) })
        );
        assert_eq!(
            focus.update_hover(100.0, 100.0),
            Some(HoverTransition { previous: Some(MailboxId(3)), next: None })
        );
    }

    #[test]
    fn unavailable_focused_hovered_captor_returns_all_cleanup_effects() {
        let mut focus = focus_with_three();
        focus.set_focus(Some(MailboxId(1)));
        focus.update_hover(5.0, 5.0);
        focus.begin_capture(MailboxId(1));

        let mut hidden = available();
        hidden.visible = false;
        assert_eq!(
            focus.update_availability(MailboxId(1), &hidden),
            AvailabilityEffects {
                focus: Some(FocusTransition { previous: Some(MailboxId(1)), next: Some(MailboxId(3)) }),
                hover: Some(HoverTransition { previous: Some(MailboxId(1)), next: None }),
                cleared_capture: Some(MailboxId(1)),
            }
        );
        assert_eq!(focus.keyboard_target(), Some(MailboxId(3)));
        assert_eq!(focus.captured(), None);
    }

    #[test]
    fn release_clears_capture_and_recomputes_hover() {
        let mut focus = focus_with_three();
        focus.begin_capture(MailboxId(1));
        focus.update_hover(5.0, 5.0);
        assert_eq!(focus.pointer_target(5.0, 45.0), Some(MailboxId(1)));
        assert_eq!(
            focus.release_capture(5.0, 45.0),
            Some(HoverTransition { previous: Some(MailboxId(1)), next: Some(MailboxId(3)) })
        );
        assert_eq!(focus.pointer_target(5.0, 45.0), Some(MailboxId(3)));
    }

    #[test]
    fn a_grab_outranks_capture_and_survives_until_it_is_ended() {
        let mut focus = focus_with_three();
        focus.begin_capture(MailboxId(1));
        focus.begin_grab(MailboxId(3));
        assert_eq!(focus.grabbed(), Some(MailboxId(3)));
        assert_eq!(focus.pointer_target(5.0, 5.0), Some(MailboxId(3)), "the grab takes a press over a captor's hit");

        // A release clears capture; the grab is modal and outlives it.
        focus.release_capture(5.0, 5.0);
        assert_eq!(focus.pointer_target(5.0, 5.0), Some(MailboxId(3)));

        focus.end_grab();
        assert_eq!(focus.grabbed(), None);
        assert_eq!(focus.pointer_target(5.0, 5.0), Some(MailboxId(1)));
    }

    #[test]
    fn a_grab_is_refused_for_a_child_that_is_not_live_for_the_pointer() {
        let mut focus = focus_with_three();
        focus.begin_grab(MailboxId(2));
        assert_eq!(focus.grabbed(), None, "a pointer-ineligible child cannot hold the modal grab");

        let mut hidden = available();
        hidden.visible = false;
        focus.update_availability(MailboxId(3), &hidden);
        focus.begin_grab(MailboxId(3));
        assert_eq!(focus.grabbed(), None, "an unavailable child cannot hold it either");
    }

    #[test]
    fn empty_and_all_unavailable_tables_do_not_route() {
        let mut empty = Focus::new();
        assert_eq!(empty.move_focus(FocusDirection::Forward), None);
        assert_eq!(empty.update_hover(1.0, 1.0), None);

        let mut focus = focus_with_three();
        let mut disabled = available();
        disabled.enabled = false;
        focus.update_availability(MailboxId(1), &disabled);
        focus.update_availability(MailboxId(3), &disabled);
        assert_eq!(focus.move_focus(FocusDirection::Backward), None);
        assert_eq!(focus.hit_test(5.0, 5.0), None);
    }
}
