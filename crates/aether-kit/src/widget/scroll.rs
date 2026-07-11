// `#[handler]` methods take decoded mail by value per the actor dispatch ABI.
#![allow(clippy::needless_pass_by_value)]

//! Stateful clipped scroll containers and recursive wheel ownership.
//!
//! Each `ScrollWidget` owns one fixed viewport, one fixed content extent, and
//! one retained offset. Its content draws in local space at
//! `content_origin - offset`; the slot clip is always viewport-local. Input
//! uses absolute `WidgetFrame` coordinates instead: a nested scroll child gets
//! an absolute frame and a wheel-only hit rectangle intersected with this
//! viewport. Keeping those conversions side by side prevents painting and hit
//! testing from drifting under non-zero panel origins or ancestor offsets.

use aether_actor::{ActorInitError, Manual, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_data::MailboxId;
use aether_kinds::MouseWheel;
use aether_math::Vec2;

use crate::widget::composite::Composite;
use crate::widget::focus::{Focus, FocusEligibility, FocusRect};
use crate::widget::panel::{ChildLayout, spawn_widget_child};
use crate::widget::theme::SetTheme;
use crate::widget::{
    Collect, ScrollConfig, ScrollDelta, ScrollExtent, ScrollOffset, ScrollOutcome, ScrollResidual, WidgetChildSpec,
    WidgetClipRect, WidgetControlState, WidgetDrawList, WidgetFrame,
};
use crate::widget::{FrameDischarge, accept_open_child_list, flush_membership};

struct ScrollContent {
    id: MailboxId,
    is_scroll: bool,
}

/// A dedicated stateful scroll actor. It owns one content root and is itself a
/// normal compositing child, so nested containers retain independent offsets.
pub struct ScrollWidget {
    viewport_extent: ScrollExtent,
    content_extent: ScrollExtent,
    content_spec: WidgetChildSpec,
    content_origin: Vec2,
    offset: ScrollOffset,
    frame: WidgetFrame,
    composite: Composite,
    frame_discharge: FrameDischarge,
    scroll_focus: Focus,
    content: Option<ScrollContent>,
    spawned: bool,
}

#[must_use]
fn extent_is_valid(extent: ScrollExtent) -> bool {
    extent.width_pixels.is_finite()
        && extent.height_pixels.is_finite()
        && extent.width_pixels >= 0.0
        && extent.height_pixels >= 0.0
}

#[must_use]
fn content_origin_is_valid(origin: Vec2) -> bool {
    origin.x.is_finite() && origin.y.is_finite()
}

#[must_use]
fn max_offset(viewport_pixels: f32, content_pixels: f32) -> f32 {
    (content_pixels - viewport_pixels).max(0.0)
}

#[must_use]
fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

#[must_use]
fn clamp_offset(viewport_extent: ScrollExtent, content_extent: ScrollExtent, offset: ScrollOffset) -> ScrollOffset {
    ScrollOffset {
        x_pixels: finite_or_zero(offset.x_pixels)
            .clamp(0.0, max_offset(viewport_extent.width_pixels, content_extent.width_pixels)),
        y_pixels: finite_or_zero(offset.y_pixels)
            .clamp(0.0, max_offset(viewport_extent.height_pixels, content_extent.height_pixels)),
    }
}

#[allow(clippy::struct_field_names)] // pixel units are load-bearing here
struct AxisOutcome {
    offset_pixels: f32,
    consumed_pixels: f32,
    residual_pixels: f32,
}

#[must_use]
fn apply_axis(old_pixels: f32, requested_pixels: f32, max_pixels: f32) -> AxisOutcome {
    let old_pixels = finite_or_zero(old_pixels).clamp(0.0, max_pixels);
    let requested_pixels = finite_or_zero(requested_pixels);
    let offset_pixels = (old_pixels + requested_pixels).clamp(0.0, max_pixels);
    let consumed_pixels = offset_pixels - old_pixels;
    AxisOutcome { offset_pixels, consumed_pixels, residual_pixels: requested_pixels - consumed_pixels }
}

#[must_use]
fn apply_scroll(
    container: MailboxId,
    viewport_extent: ScrollExtent,
    content_extent: ScrollExtent,
    old_offset: ScrollOffset,
    requested: ScrollDelta,
) -> ScrollOutcome {
    let x = apply_axis(
        old_offset.x_pixels,
        requested.x_pixels,
        max_offset(viewport_extent.width_pixels, content_extent.width_pixels),
    );
    let y = apply_axis(
        old_offset.y_pixels,
        requested.y_pixels,
        max_offset(viewport_extent.height_pixels, content_extent.height_pixels),
    );
    ScrollOutcome {
        container,
        offset: ScrollOffset { x_pixels: x.offset_pixels, y_pixels: y.offset_pixels },
        consumed: ScrollDelta { x_pixels: x.consumed_pixels, y_pixels: y.consumed_pixels },
        residual: ScrollResidual { x_pixels: x.residual_pixels, y_pixels: y.residual_pixels },
    }
}

#[must_use]
fn wheel_delta(wheel: MouseWheel) -> ScrollDelta {
    ScrollDelta { x_pixels: finite_or_zero(-wheel.delta_x), y_pixels: finite_or_zero(-wheel.delta_y) }
}

#[must_use]
fn has_residual(residual: ScrollResidual) -> bool {
    residual.x_pixels != 0.0 || residual.y_pixels != 0.0
}

#[must_use]
fn viewport_clip(extent: ScrollExtent) -> WidgetClipRect {
    WidgetClipRect { x: 0.0, y: 0.0, width: extent.width_pixels, height: extent.height_pixels }
}

#[must_use]
fn clipped_focus_rect(viewport: &WidgetFrame, child: &WidgetFrame) -> Option<FocusRect> {
    let viewport_right = viewport.x + viewport.width;
    let viewport_bottom = viewport.y + viewport.height;
    let child_right = child.x + child.width;
    let child_bottom = child.y + child.height;
    if ![
        viewport.x,
        viewport.y,
        viewport.width,
        viewport.height,
        viewport_right,
        viewport_bottom,
        child.x,
        child.y,
        child.width,
        child.height,
        child_right,
        child_bottom,
    ]
    .into_iter()
    .all(f32::is_finite)
    {
        return None;
    }
    let x = viewport.x.max(child.x);
    let y = viewport.y.max(child.y);
    let right = viewport_right.min(child_right);
    let bottom = viewport_bottom.min(child_bottom);
    (right > x && bottom > y).then_some(FocusRect { x, y, width: right - x, height: bottom - y })
}

impl ScrollWidget {
    fn ensure_spawned(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if self.spawned {
            return;
        }
        self.spawned = true;
        let Some(spawned) =
            spawn_widget_child(ctx, &self.content_spec, ChildLayout::Content { assigned_extent: self.content_extent })
        else {
            return;
        };
        self.composite.register_slot(
            spawned.id,
            self.local_content_origin(),
            Some(viewport_clip(self.viewport_extent)),
            &self.content_spec.subname,
            spawned.type_namespace,
        );
        self.content = Some(ScrollContent { id: spawned.id, is_scroll: spawned.scroll_viewport.is_some() });
        self.sync_layout(ctx);
    }

    fn local_content_origin(&self) -> Vec2 {
        Vec2::new(self.content_origin.x - self.offset.x_pixels, self.content_origin.y - self.offset.y_pixels)
    }

    fn content_frame(&self) -> WidgetFrame {
        let local_origin = self.local_content_origin();
        WidgetFrame {
            x: self.frame.x + local_origin.x,
            y: self.frame.y + local_origin.y,
            width: self.content_extent.width_pixels,
            height: self.content_extent.height_pixels,
        }
    }

    fn sync_layout(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        let Some(content) = &self.content else {
            return;
        };
        let content_id = content.id;
        let is_scroll = content.is_scroll;
        let content_frame = self.content_frame();
        self.composite.update_slot_layout(
            content_id,
            self.local_content_origin(),
            Some(viewport_clip(self.viewport_extent)),
        );
        ctx.send_to(content_id, &content_frame);

        self.scroll_focus.clear();
        if is_scroll && let Some(rect) = clipped_focus_rect(&self.frame, &content_frame) {
            self.scroll_focus.register(
                content_id,
                rect,
                FocusEligibility { pointer: true, keyboard: false },
                &WidgetControlState::default(),
            );
        }
    }

    fn drive_frame(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        self.ensure_spawned(ctx);
        flush_membership(&mut self.composite, ctx);
        self.composite.begin_frame();
        self.frame_discharge.begin_frame();
        if let Some(content) = &self.content {
            ctx.send_to(content.id, &Collect);
        }
        if self.composite.is_complete() {
            self.finish(ctx);
        }
    }

    fn finish(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if self.frame_discharge.is_closed() {
            return;
        }
        let list =
            self.composite.flatten(Some([self.viewport_extent.width_pixels, self.viewport_extent.height_pixels]));
        if let Some(parent) = ctx.parent() {
            parent.send(&list);
        } else {
            tracing::warn!(target: "aether_kit", "scroll widget finished without a parent; draw list dropped");
        }
        let closed = self.frame_discharge.close_frame();
        debug_assert!(closed, "an open scroll frame closes exactly once");
    }

    fn apply_delta(&mut self, ctx: &mut WasmCtx<'_, Manual>, delta: ScrollDelta) {
        let outcome = apply_scroll(ctx.mailbox_id(), self.viewport_extent, self.content_extent, self.offset, delta);
        self.offset = outcome.offset;
        self.sync_layout(ctx);
        if let Some(parent) = ctx.parent() {
            parent.send(&outcome);
            if has_residual(outcome.residual) {
                parent.send(&outcome.residual);
            }
        }
    }

    fn nested_source(&self, source: Option<MailboxId>) -> bool {
        self.content.as_ref().is_some_and(|content| content.is_scroll && source == Some(content.id))
    }
}

/// Stateful scroll viewport. Spawned through `WidgetKind::Scroll`; its parent
/// assigns a `WidgetFrame`, sends `Collect`, and routes wheel input by cursor
/// hit testing. The actor emits `ScrollOutcome` and any exact residual upward.
#[actor(instanced)]
impl WasmActor for ScrollWidget {
    type Config = ScrollConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.scroll";

    fn init(config: ScrollConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        if !extent_is_valid(config.viewport_extent) {
            return Err(ActorInitError::from("scroll viewport extent must be finite and non-negative"));
        }
        if !extent_is_valid(config.content_extent) {
            return Err(ActorInitError::from("scroll content extent must be finite and non-negative"));
        }
        let content_origin = Vec2::new(config.content.origin[0], config.content.origin[1]);
        if !content_origin_is_valid(content_origin) {
            return Err(ActorInitError::from("scroll content origin must be finite"));
        }
        let offset = clamp_offset(config.viewport_extent, config.content_extent, config.initial_offset);
        Ok(Self {
            viewport_extent: config.viewport_extent,
            content_extent: config.content_extent,
            content_spec: config.content,
            content_origin,
            offset,
            frame: WidgetFrame {
                x: 0.0,
                y: 0.0,
                width: config.viewport_extent.width_pixels,
                height: config.viewport_extent.height_pixels,
            },
            composite: Composite::new(),
            frame_discharge: FrameDischarge::default(),
            scroll_focus: Focus::new(),
            content: None,
            spawned: false,
        })
    }

    #[handler::manual]
    fn on_frame(&mut self, ctx: &mut WasmCtx<'_, Manual>, frame: WidgetFrame) {
        self.frame = frame;
        self.sync_layout(ctx);
    }

    #[handler::manual]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_, Manual>, _collect: Collect) {
        self.drive_frame(ctx);
    }

    #[handler::manual]
    fn on_draw_list(&mut self, ctx: &mut WasmCtx<'_, Manual>, list: WidgetDrawList) {
        if accept_open_child_list(&self.frame_discharge, &mut self.composite, ctx, list) {
            self.finish(ctx);
        }
    }

    /// Relay a live theme or font update to the retained content root. Nested
    /// scroll actors apply the same rule, so style follows the actor tree.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        if let Some(content) = &self.content {
            ctx.send_to(content.id, &set);
        }
    }

    #[handler::manual]
    fn on_mouse_wheel(&mut self, ctx: &mut WasmCtx<'_, Manual>, wheel: MouseWheel) {
        if let Some(child) = self.scroll_focus.hit_test(wheel.x, wheel.y) {
            ctx.send_to(child, &wheel);
        } else {
            self.apply_delta(ctx, wheel_delta(wheel));
        }
    }

    #[handler::manual]
    fn on_scroll_outcome(&mut self, ctx: &mut WasmCtx<'_, Manual>, outcome: ScrollOutcome) {
        if !self.nested_source(ctx.source_mailbox()) {
            tracing::warn!(target: "aether_kit", "ignored scroll outcome from non-child source");
            return;
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&outcome);
        }
    }

    #[handler::manual]
    fn on_scroll_residual(&mut self, ctx: &mut WasmCtx<'_, Manual>, residual: ScrollResidual) {
        if !self.nested_source(ctx.source_mailbox()) {
            tracing::warn!(target: "aether_kit", "ignored scroll residual from non-child source");
            return;
        }
        self.apply_delta(ctx, ScrollDelta { x_pixels: residual.x_pixels, y_pixels: residual.y_pixels });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::WidgetKind;

    const VIEWPORT: ScrollExtent = ScrollExtent { width_pixels: 40.0, height_pixels: 30.0 };
    const CONTENT: ScrollExtent = ScrollExtent { width_pixels: 70.0, height_pixels: 80.0 };

    fn assert_axis_invariant(requested: f32, consumed: f32, residual: f32) {
        assert!(
            (requested - (consumed + residual)).abs() <= f32::EPSILON * requested.abs().max(1.0),
            "requested {requested} != consumed {consumed} + residual {residual}",
        );
    }

    #[test]
    fn scroll_math_covers_middle_bounds_and_partial_overshoot_per_axis() {
        let middle = apply_scroll(
            MailboxId(7),
            VIEWPORT,
            CONTENT,
            ScrollOffset { x_pixels: 10.0, y_pixels: 20.0 },
            ScrollDelta { x_pixels: 8.0, y_pixels: 12.0 },
        );
        assert_eq!(middle.offset, ScrollOffset { x_pixels: 18.0, y_pixels: 32.0 });
        assert_eq!(middle.consumed.x_pixels, 8.0);
        assert_eq!(middle.consumed.y_pixels, 12.0);
        assert_eq!(middle.residual, ScrollResidual::default());

        let positive = apply_scroll(
            MailboxId(7),
            VIEWPORT,
            CONTENT,
            ScrollOffset { x_pixels: 25.0, y_pixels: 45.0 },
            ScrollDelta { x_pixels: 12.0, y_pixels: 20.0 },
        );
        assert_eq!(positive.offset.x_pixels, 30.0);
        assert_eq!(positive.offset.y_pixels, 50.0);
        assert_eq!(positive.consumed.x_pixels, 5.0);
        assert_eq!(positive.consumed.y_pixels, 5.0);
        assert_eq!(positive.residual.x_pixels, 7.0);
        assert_eq!(positive.residual.y_pixels, 15.0);
        assert_axis_invariant(12.0, positive.consumed.x_pixels, positive.residual.x_pixels);
        assert_axis_invariant(20.0, positive.consumed.y_pixels, positive.residual.y_pixels);

        let negative = apply_scroll(
            MailboxId(7),
            VIEWPORT,
            CONTENT,
            ScrollOffset { x_pixels: 4.0, y_pixels: 9.0 },
            ScrollDelta { x_pixels: -10.0, y_pixels: -12.0 },
        );
        assert_eq!(negative.offset, ScrollOffset::default());
        assert_eq!(negative.consumed.x_pixels, -4.0);
        assert_eq!(negative.consumed.y_pixels, -9.0);
        assert_eq!(negative.residual.x_pixels, -6.0);
        assert_eq!(negative.residual.y_pixels, -3.0);
    }

    #[test]
    fn no_overflow_extent_returns_the_whole_request_and_reversal_leaves_bound() {
        assert_eq!(
            clamp_offset(VIEWPORT, CONTENT, ScrollOffset { x_pixels: 99.0, y_pixels: -2.0 },),
            ScrollOffset { x_pixels: 30.0, y_pixels: 0.0 },
            "initial offsets clamp independently into the configured extent",
        );
        let no_overflow = apply_scroll(
            MailboxId(1),
            ScrollExtent { width_pixels: 50.0, height_pixels: 50.0 },
            ScrollExtent { width_pixels: 20.0, height_pixels: 0.0 },
            ScrollOffset::default(),
            ScrollDelta { x_pixels: 9.0, y_pixels: -3.0 },
        );
        assert_eq!(no_overflow.offset, ScrollOffset::default());
        assert_eq!(no_overflow.consumed, ScrollDelta::default());
        assert_eq!(no_overflow.residual, ScrollResidual { x_pixels: 9.0, y_pixels: -3.0 });

        let reversed = apply_scroll(
            MailboxId(1),
            VIEWPORT,
            CONTENT,
            ScrollOffset { x_pixels: 30.0, y_pixels: 50.0 },
            ScrollDelta { x_pixels: -6.0, y_pixels: -7.0 },
        );
        assert_eq!(reversed.offset.x_pixels, 24.0);
        assert_eq!(reversed.offset.y_pixels, 43.0);
        assert_eq!(reversed.residual, ScrollResidual::default());
    }

    #[test]
    fn non_finite_offsets_and_requests_never_enter_retained_state() {
        let clamped = clamp_offset(VIEWPORT, CONTENT, ScrollOffset { x_pixels: f32::NAN, y_pixels: f32::INFINITY });
        assert_eq!(clamped, ScrollOffset::default());
        let outcome = apply_scroll(
            MailboxId(1),
            VIEWPORT,
            CONTENT,
            clamped,
            ScrollDelta { x_pixels: f32::NEG_INFINITY, y_pixels: f32::NAN },
        );
        assert_eq!(outcome.offset, ScrollOffset::default());
        assert_eq!(outcome.consumed, ScrollDelta::default());
        assert_eq!(outcome.residual, ScrollResidual::default());
        assert!(outcome.offset.x_pixels.is_finite());
        assert!(outcome.offset.y_pixels.is_finite());
    }

    #[test]
    fn wheel_is_converted_once_and_axes_remain_independent() {
        assert_eq!(
            wheel_delta(MouseWheel { delta_x: 5.0, delta_y: -7.0, x: 100.0, y: 200.0 }),
            ScrollDelta { x_pixels: -5.0, y_pixels: 7.0 }
        );
        let outcome = apply_scroll(
            MailboxId(1),
            VIEWPORT,
            CONTENT,
            ScrollOffset { x_pixels: 0.0, y_pixels: 49.0 },
            ScrollDelta { x_pixels: 12.0, y_pixels: 8.0 },
        );
        assert_eq!(outcome.consumed.x_pixels, 12.0);
        assert_eq!(outcome.residual.x_pixels, 0.0);
        assert_eq!(outcome.consumed.y_pixels, 1.0);
        assert_eq!(outcome.residual.y_pixels, 7.0);
    }

    #[test]
    fn local_draw_and_absolute_input_transforms_share_the_same_offsets() {
        let widget = ScrollWidget {
            viewport_extent: VIEWPORT,
            content_extent: CONTENT,
            content_spec: WidgetChildSpec {
                subname: "content".into(),
                kind: WidgetKind::Composite,
                origin: [0.0, 0.0],
                clip: None,
                config: Vec::new(),
            },
            content_origin: Vec2::new(7.0, 9.0),
            offset: ScrollOffset { x_pixels: 3.0, y_pixels: 5.0 },
            frame: WidgetFrame { x: 100.0, y: 200.0, width: 40.0, height: 30.0 },
            composite: Composite::new(),
            frame_discharge: FrameDischarge::default(),
            scroll_focus: Focus::new(),
            content: None,
            spawned: false,
        };
        assert_eq!(widget.local_content_origin(), Vec2::new(4.0, 4.0));
        let child = widget.content_frame();
        assert_eq!(child.x, 104.0);
        assert_eq!(child.y, 204.0);
        assert_eq!(child.width, 70.0);
        assert_eq!(child.height, 80.0);
        assert_eq!(
            clipped_focus_rect(&widget.frame, &child),
            Some(FocusRect { x: 104.0, y: 204.0, width: 36.0, height: 26.0 })
        );
    }

    #[test]
    fn wheel_hit_testing_ignores_unrelated_capture_and_prefers_topmost_overlap() {
        let mut ordinary = Focus::new();
        let mut wheel = Focus::new();
        let live = WidgetControlState::default();
        ordinary.register(
            MailboxId(1),
            FocusRect { x: 50.0, y: 50.0, width: 10.0, height: 10.0 },
            FocusEligibility { pointer: true, keyboard: true },
            &live,
        );
        ordinary.begin_capture(MailboxId(1));
        for (child, x) in [(MailboxId(2), 0.0), (MailboxId(3), 5.0)] {
            wheel.register(
                child,
                FocusRect { x, y: 0.0, width: 10.0, height: 10.0 },
                FocusEligibility { pointer: true, keyboard: false },
                &live,
            );
        }
        assert_eq!(ordinary.captured(), Some(MailboxId(1)));
        assert_eq!(wheel.hit_test(7.0, 7.0), Some(MailboxId(3)));
        assert_eq!(wheel.hit_test(30.0, 30.0), None);
    }

    #[test]
    fn invalid_extents_and_disjoint_frames_are_rejected() {
        assert!(!extent_is_valid(ScrollExtent { width_pixels: f32::NAN, height_pixels: 1.0 }));
        assert!(!extent_is_valid(ScrollExtent { width_pixels: 1.0, height_pixels: -1.0 }));
        assert_eq!(
            clipped_focus_rect(
                &WidgetFrame { x: 0.0, y: 0.0, width: 10.0, height: 10.0 },
                &WidgetFrame { x: 20.0, y: 20.0, width: 5.0, height: 5.0 },
            ),
            None,
        );
    }
}
