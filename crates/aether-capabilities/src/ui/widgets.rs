//! `aether.ui` cap (ADR-0107). Translates immediate-mode widget mail into
//! `draw_solid_quads` and `draw_text` sends the same tick — a CPU-only
//! translator with no retained widget state across frames. Components lay
//! out and resend every frame; the cap forwards.
//!
//! Three handlers, each fire-and-forget:
//!
//! - **`on_panel`** → one `DrawSolidQuads` (screen-space) to `aether.render`.
//! - **`on_bar`** → two `SolidQuad`s in one `DrawSolidQuads` (track + frac-
//!   sized fill, screen-space) to `aether.render`.
//! - **`on_label`** → one `DrawText` (screen-space) to `aether.text`. The
//!   string flows from the screen-pixel `(x, y)` along the baseline, where
//!   `(0, 0)` is the window's top-left corner.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the decoded
// bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root because `#[actor]`
// emits `impl HandlesKind<K> for X {}` markers always-on, outside the
// `ui-native` runtime gate, so they reference these kinds from here.
use aether_kinds::{MouseButton, MouseMove, Tick};

use super::kinds::{UiBar, UiButton, UiClicked, UiLabel, UiPanel};

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` type —
// the handler/init ctx, the runtime state, and the helpers — lives in the
// `runtime` module below, gated once by `feature = "ui-native"` and written
// cfg-free within; the `#[actor] impl` reaches all of it through the single
// `use runtime::*` glob. The kind types (`UiPanel` / `UiBar` / …) stay
// always-on via the imports above — the always-on `HandlesKind<K>` markers
// name them.
use aether_actor::actor;

// The `runtime` module is this cap's private runtime-half namespace; the impl
// reaches all of it (state, ctx types, helpers) through this single seam,
// so the glob is intentional rather than a dozen one-line imports.
#[cfg(feature = "ui-native")]
#[allow(clippy::wildcard_imports)]
use super::runtime::*;

/// `aether.ui` cap **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`),
/// the per-handler `HandlesKind` markers, and the name-inventory entry,
/// all emitted always-on by `#[actor]`. The state-bearing runtime
/// (`UiCapabilityState`, which holds the cursor position and button-rect
/// double-buffer) lives behind the one `feature = "ui-native"` gate, so
/// a transport-only build never names `UiCapabilityState` nor pulls
/// `aether_substrate` through this cap.
pub struct UiCapability;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` type —
// the handler/init ctx, the runtime state — lives in the `runtime` module
// below, gated once by `feature = "ui-native"` and written cfg-free within;
// the `#[actor] impl` reaches all of it through the single `use runtime::*`
// glob. The kind types stay always-on via imports at module root — the
// always-on `HandlesKind<K>` markers name them.
#[actor(singleton, runtime_feature = "ui-native")]
impl NativeActor for UiCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// state-bearing struct holding the cursor position and button-rect
    /// double-buffer.
    type State = UiCapabilityState;

    type Config = ();

    /// ADR-0107 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.ui";

    /// No substrate resources to claim — the cap holds only its own
    /// per-frame hit-test state.
    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<UiCapabilityState, BootError> {
        Ok(UiCapabilityState::default())
    }

    /// Subscribe the cursor + click streams and the frame edge.
    ///
    /// Mirrors the kit's input-subscription pattern: `MouseMove` /
    /// `MouseButton` through `aether.input`, `Tick` through
    /// `aether.lifecycle`. The subscriptions survive `replace` (the
    /// mailbox id is stable) and clear on drop.
    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        ctx.actor::<InputCapability>().subscribe::<MouseMove>();
        ctx.actor::<InputCapability>().subscribe::<MouseButton>();
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
    }

    /// Draw a flat-colored panel.
    ///
    /// # Agent
    /// Fire-and-forget. Forwards one `DrawSolidQuads` (screen-space) to
    /// `aether.render` the same tick. Resend every frame.
    #[handler]
    fn on_panel(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: UiPanel) {
        let [x, y, width, height] = mail.rect;
        let draw = DrawSolidQuads {
            space: QuadSpace::Screen,
            quads: vec![SolidQuad {
                x,
                y,
                width,
                height,
                color: mail.color,
            }],
        };
        ctx.actor::<RenderCapability>().send(&draw);
    }

    /// Draw a two-layer progress bar.
    ///
    /// # Agent
    /// Fire-and-forget. Forwards a two-quad `DrawSolidQuads` (screen-
    /// space, track then frac-sized fill) to `aether.render` the same
    /// tick. `frac` is clamped to [0, 1]. Resend every frame.
    #[handler]
    fn on_bar(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: UiBar) {
        let [x, y, width, height] = mail.rect;
        let frac = mail.frac.clamp(0.0, 1.0);
        let draw = DrawSolidQuads {
            space: QuadSpace::Screen,
            quads: vec![
                // Track: full rect.
                SolidQuad {
                    x,
                    y,
                    width,
                    height,
                    color: mail.track_color,
                },
                // Fill: frac-fraction of the width.
                SolidQuad {
                    x,
                    y,
                    width: width * frac,
                    height,
                    color: mail.fill_color,
                },
            ],
        };
        ctx.actor::<RenderCapability>().send(&draw);
    }

    /// Draw a text label.
    ///
    /// # Agent
    /// Fire-and-forget. Forwards one `DrawText` (screen-space) to
    /// `aether.text` the same tick. The string flows from the screen-pixel
    /// `(x, y)` along the baseline, where `(0, 0)` is the window's
    /// top-left corner. An unknown `font_id` warn-drops in the text cap.
    /// Resend every frame.
    #[handler]
    fn on_label(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: UiLabel) {
        let draw = DrawText {
            font_id: mail.font_id,
            text: mail.text,
            size_pixels: mail.size_pixels,
            color: mail.color,
            origin: [mail.x, mail.y],
            space: QuadSpace::Screen,
        };
        ctx.actor::<TextCapability>().send(&draw);
    }

    /// Draw a clickable button and record it for hit-testing.
    ///
    /// # Agent
    /// Fire-and-forget. Forwards the fill (`color`) as a screen-space
    /// `DrawSolidQuads` to `aether.render` and the `text` label as a
    /// `DrawText` to `aether.text`, the same tick. Records `(rect, id,
    /// owner)` for the in-progress frame; `owner` is the sending
    /// component, read from the inbound's host-stamped source. A left-
    /// click inside `rect` replies `UiClicked { id }` to `owner`
    /// within one frame (see `on_mouse_button`). Resend every frame.
    #[handler]
    fn on_button(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: UiButton) {
        let [x, y, width, height] = mail.rect;
        // Record for the next click's hit-test. A button mailed with
        // no component source (broadcast / session) has nowhere to
        // reply, so it draws but never activates.
        if let Some(owner) = ctx.source_mailbox() {
            state.current.push(ButtonRect {
                rect: mail.rect,
                id: mail.id,
                owner,
            });
        }
        let fill = DrawSolidQuads {
            space: QuadSpace::Screen,
            quads: vec![SolidQuad {
                x,
                y,
                width,
                height,
                color: mail.color,
            }],
        };
        ctx.actor::<RenderCapability>().send(&fill);
        let label = DrawText {
            font_id: mail.font_id,
            text: mail.text,
            size_pixels: mail.size_pixels,
            color: mail.text_color,
            origin: [x, y],
            space: QuadSpace::Screen,
        };
        ctx.actor::<TextCapability>().send(&label);
    }

    /// Cache the cursor position.
    ///
    /// # Agent
    /// Fire-and-forget. Updates the latest cursor used by the next
    /// `on_mouse_button` hit-test. No forwarded mail.
    #[handler]
    fn on_mouse_move(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: MouseMove) {
        state.cursor = [mail.x, mail.y];
    }

    /// Hit-test a left-click against the last frame's buttons.
    ///
    /// # Agent
    /// Fire-and-forget. Tests the cached cursor against the last
    /// completed frame's button rects, topmost-wins (later-drawn
    /// buttons paint on top, so it scans in reverse draw order), and
    /// on a hit sends `UiClicked { id }` to that button's recorded
    /// owner by id. A click outside every rect does nothing. v1: the
    /// mouse-button stream has no button discriminant or release, so
    /// this fires on left-press only.
    #[handler]
    fn on_mouse_button(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: MouseButton) {
        let [cursor_x, cursor_y] = state.cursor;
        let target = state
            .last
            .iter()
            .rev()
            .find(|button| {
                let [x, y, width, height] = button.rect;
                cursor_x >= x && cursor_x < x + width && cursor_y >= y && cursor_y < y + height
            })
            .map(|button| (button.id, button.owner));
        if let Some((id, owner)) = target {
            ctx.fanout(once(owner), &UiClicked { id });
        }
    }

    /// Frame edge: rotate the button-rect buffers.
    ///
    /// # Agent
    /// Fire-and-forget. The buttons drawn this frame become the hit-
    /// test set (`last`), and the new frame starts with an empty
    /// accumulator (`current`). No forwarded mail.
    #[handler]
    fn on_tick(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _tick: Tick) {
        swap(&mut state.current, &mut state.last);
        state.current.clear();
    }
}

#[cfg(all(test, feature = "ui-native"))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    use aether_data::{Kind, MailId, MailboxId, SessionToken, Source, SourceAddr, Uuid};
    use aether_kinds::QuadSpace;
    use aether_substrate::actor::native::NativeCtx;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::outbound::EgressEvent;

    use super::super::runtime::UiCapabilityState;
    use super::UiCapability;
    use super::{MouseButton, MouseMove, Tick, UiBar, UiButton, UiClicked, UiLabel, UiPanel};
    use crate::render::DrawSolidQuads;
    use crate::test_chassis::test_mailer_and_rx;
    use crate::text::DrawText;

    fn session_sender() -> Source {
        Source::to(SourceAddr::Session(SessionToken(Uuid::nil())))
    }

    /// A sender stamped as the component mailbox `id` — the shape the
    /// host stamps on a fire-and-forget `UiButton`, so `on_button`
    /// records `id` as the button's owner.
    fn component_sender(id: u64) -> Source {
        Source::to(SourceAddr::Component(MailboxId(id)))
    }

    fn ctx_binding() -> (Arc<NativeBinding>, Receiver<EgressEvent>) {
        let (mailer, rx) = test_mailer_and_rx();
        let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
        (binding, rx)
    }

    // A free fn (not a `&self` method) so the borrow is of `binding`
    // only, leaving `&mut state` a disjoint field borrow at the call
    // site — ADR-0122 split: handlers are associated fns on the identity
    // taking `state: &mut UiCapabilityState`, called as
    // `UiCapability::on_x(&mut state, &mut ctx, mail)`.
    fn make_ctx(binding: &Arc<NativeBinding>, sender: Source) -> NativeCtx<'_> {
        NativeCtx::new(binding, sender, MailId::NONE, MailId::NONE)
    }

    /// Flush and drain egress until a `UiClicked` arrives; return its
    /// recipient mailbox + decoded payload. Skips the button's
    /// forwarded fill / label sends.
    fn recv_clicked(binding: &NativeBinding, rx: &Receiver<EgressEvent>) -> (MailboxId, UiClicked) {
        binding.flush_outbound();
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("test: UiClicked egress arrives within deadline");
            if let EgressEvent::UnresolvedMail {
                recipient_mailbox_id,
                kind_id,
                payload,
                ..
            } = event
                && kind_id == UiClicked::ID
            {
                let decoded =
                    UiClicked::decode_from_bytes(&payload).expect("test: decodes UiClicked");
                return (recipient_mailbox_id, decoded);
            }
        }
    }

    /// Flush and drain the whole channel, asserting no `UiClicked` was
    /// delivered (the button's fill / label sends are allowed).
    fn assert_no_clicked(binding: &NativeBinding, rx: &Receiver<EgressEvent>) {
        binding.flush_outbound();
        while let Ok(event) = rx.try_recv() {
            if let EgressEvent::UnresolvedMail { kind_id, .. } = event {
                assert_ne!(
                    kind_id,
                    UiClicked::ID,
                    "a click outside every rect must not deliver UiClicked"
                );
            }
        }
    }

    /// Flush buffered sends and drain egress until a `UnresolvedMail` of
    /// kind `K` arrives. Skips non-mail events.
    fn assert_next_send_kind<K: Kind>(binding: &NativeBinding, rx: &Receiver<EgressEvent>) {
        binding.flush_outbound();
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("test: egress event arrives within deadline");
            if let EgressEvent::UnresolvedMail { kind_id, .. } = event {
                assert_eq!(kind_id, K::ID, "unexpected bubbled kind");
                return;
            }
        }
    }

    #[test]
    fn panel_produces_draw_solid_quads() {
        let mut state = UiCapabilityState::default();
        let (binding, rx) = ctx_binding();
        let mut ctx = make_ctx(&binding, session_sender());
        UiCapability::on_panel(
            &mut state,
            &mut ctx,
            UiPanel {
                rect: [10.0, 20.0, 100.0, 50.0],
                color: [0.2, 0.4, 0.6, 1.0],
            },
        );
        assert_next_send_kind::<DrawSolidQuads>(&binding, &rx);
    }

    #[test]
    fn bar_produces_draw_solid_quads_with_two_quads() {
        let mut state = UiCapabilityState::default();
        let (binding, rx) = ctx_binding();
        let mut ctx = make_ctx(&binding, session_sender());
        UiCapability::on_bar(
            &mut state,
            &mut ctx,
            UiBar {
                rect: [0.0, 0.0, 200.0, 20.0],
                frac: 0.5,
                track_color: [0.3, 0.3, 0.3, 1.0],
                fill_color: [0.0, 0.8, 0.0, 1.0],
            },
        );
        binding.flush_outbound();
        let event = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test: egress event");
        let EgressEvent::UnresolvedMail {
            kind_id, payload, ..
        } = event
        else {
            panic!("expected UnresolvedMail for DrawSolidQuads");
        };
        assert_eq!(kind_id, DrawSolidQuads::ID, "expected DrawSolidQuads");
        let decoded =
            DrawSolidQuads::decode_from_bytes(&payload).expect("test: decodes DrawSolidQuads");
        assert_eq!(decoded.quads.len(), 2, "bar emits track + fill quad");
        assert_eq!(decoded.quads[1].width, 100.0, "fill is frac * full width");
    }

    #[test]
    fn bar_clamps_frac_above_one() {
        let mut state = UiCapabilityState::default();
        let (binding, rx) = ctx_binding();
        let mut ctx = make_ctx(&binding, session_sender());
        UiCapability::on_bar(
            &mut state,
            &mut ctx,
            UiBar {
                rect: [0.0, 0.0, 200.0, 20.0],
                frac: 2.0,
                track_color: [0.3, 0.3, 0.3, 1.0],
                fill_color: [0.8, 0.0, 0.0, 1.0],
            },
        );
        binding.flush_outbound();
        let event = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test: egress event");
        let EgressEvent::UnresolvedMail {
            kind_id, payload, ..
        } = event
        else {
            panic!("expected UnresolvedMail");
        };
        assert_eq!(kind_id, DrawSolidQuads::ID);
        let decoded =
            DrawSolidQuads::decode_from_bytes(&payload).expect("test: decodes DrawSolidQuads");
        assert_eq!(
            decoded.quads[1].width, 200.0,
            "frac > 1 clamps to full width"
        );
    }

    #[test]
    fn bar_clamps_frac_below_zero() {
        let mut state = UiCapabilityState::default();
        let (binding, rx) = ctx_binding();
        let mut ctx = make_ctx(&binding, session_sender());
        UiCapability::on_bar(
            &mut state,
            &mut ctx,
            UiBar {
                rect: [0.0, 0.0, 200.0, 20.0],
                frac: -0.5,
                track_color: [0.3, 0.3, 0.3, 1.0],
                fill_color: [0.8, 0.0, 0.0, 1.0],
            },
        );
        binding.flush_outbound();
        let event = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test: egress event");
        let EgressEvent::UnresolvedMail {
            kind_id, payload, ..
        } = event
        else {
            panic!("expected UnresolvedMail");
        };
        assert_eq!(kind_id, DrawSolidQuads::ID);
        let decoded =
            DrawSolidQuads::decode_from_bytes(&payload).expect("test: decodes DrawSolidQuads");
        assert_eq!(decoded.quads[1].width, 0.0, "frac < 0 clamps to zero width");
    }

    #[test]
    fn label_produces_draw_text_screen_space() {
        let mut state = UiCapabilityState::default();
        let (binding, rx) = ctx_binding();
        let mut ctx = make_ctx(&binding, session_sender());
        UiCapability::on_label(
            &mut state,
            &mut ctx,
            UiLabel {
                x: 50.0,
                y: 100.0,
                font_id: 3,
                text: "Score: 42".to_owned(),
                size_pixels: 24.0,
                color: [1.0, 1.0, 1.0, 1.0],
            },
        );
        binding.flush_outbound();
        let event = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test: egress event arrives within deadline");
        let EgressEvent::UnresolvedMail {
            kind_id, payload, ..
        } = event
        else {
            panic!("expected UnresolvedMail for DrawText");
        };
        assert_eq!(kind_id, DrawText::ID, "expected DrawText");
        let decoded = DrawText::decode_from_bytes(&payload).expect("test: decodes DrawText");
        assert_eq!(decoded.space, QuadSpace::Screen, "label uses Screen space");
        assert_eq!(decoded.origin, [50.0, 100.0], "label origin is (x, y)");
    }

    fn button(id: u32, rect: [f32; 4]) -> UiButton {
        UiButton {
            id,
            rect,
            color: [0.1, 0.1, 0.1, 1.0],
            font_id: 0,
            text: "Go".to_owned(),
            size_pixels: 16.0,
            text_color: [1.0, 1.0, 1.0, 1.0],
        }
    }

    #[test]
    fn button_records_rect_with_owner_and_tick_swaps() {
        let mut state = UiCapabilityState::default();
        let (binding, _rx) = ctx_binding();
        let mut ctx = make_ctx(&binding, component_sender(42));
        UiCapability::on_button(&mut state, &mut ctx, button(7, [10.0, 10.0, 100.0, 40.0]));
        assert_eq!(state.current.len(), 1, "one button recorded this frame");
        assert_eq!(state.current[0].id, 7);
        assert_eq!(state.current[0].rect, [10.0, 10.0, 100.0, 40.0]);
        assert_eq!(state.current[0].owner, MailboxId(42), "owner is the sender");
        // Frame edge: this frame's buttons become the hit-test set,
        // and the next frame starts empty.
        UiCapability::on_tick(&mut state, &mut ctx, Tick);
        assert_eq!(state.last.len(), 1, "tick swaps current into last");
        assert!(state.current.is_empty(), "tick clears current");
    }

    #[test]
    fn click_inside_button_delivers_clicked_to_owner() {
        let mut state = UiCapabilityState::default();
        let (binding, rx) = ctx_binding();
        let mut ctx = make_ctx(&binding, component_sender(42));
        UiCapability::on_button(&mut state, &mut ctx, button(7, [10.0, 10.0, 100.0, 40.0]));
        UiCapability::on_tick(&mut state, &mut ctx, Tick);
        UiCapability::on_mouse_move(&mut state, &mut ctx, MouseMove { x: 50.0, y: 25.0 });
        UiCapability::on_mouse_button(&mut state, &mut ctx, MouseButton);
        let (recipient, clicked) = recv_clicked(&binding, &rx);
        assert_eq!(recipient, MailboxId(42), "clicked routes to the owner");
        assert_eq!(clicked.id, 7, "clicked carries the button id");
    }

    #[test]
    fn click_outside_button_delivers_nothing() {
        let mut state = UiCapabilityState::default();
        let (binding, rx) = ctx_binding();
        let mut ctx = make_ctx(&binding, component_sender(42));
        UiCapability::on_button(&mut state, &mut ctx, button(7, [10.0, 10.0, 100.0, 40.0]));
        UiCapability::on_tick(&mut state, &mut ctx, Tick);
        // Cursor outside the rect (right of x + width).
        UiCapability::on_mouse_move(&mut state, &mut ctx, MouseMove { x: 200.0, y: 25.0 });
        UiCapability::on_mouse_button(&mut state, &mut ctx, MouseButton);
        assert_no_clicked(&binding, &rx);
    }

    #[test]
    fn overlapping_buttons_activate_topmost() {
        let mut state = UiCapabilityState::default();
        let (binding, rx) = ctx_binding();
        // Button A (id 1, owner 42) drawn first; button B (id 2,
        // owner 43) drawn second, so B paints on top.
        let mut ctx_a = make_ctx(&binding, component_sender(42));
        UiCapability::on_button(&mut state, &mut ctx_a, button(1, [0.0, 0.0, 100.0, 100.0]));
        let mut ctx_b = make_ctx(&binding, component_sender(43));
        UiCapability::on_button(
            &mut state,
            &mut ctx_b,
            button(2, [50.0, 50.0, 100.0, 100.0]),
        );
        UiCapability::on_tick(&mut state, &mut ctx_b, Tick);
        // Cursor inside both rects.
        UiCapability::on_mouse_move(&mut state, &mut ctx_b, MouseMove { x: 60.0, y: 60.0 });
        UiCapability::on_mouse_button(&mut state, &mut ctx_b, MouseButton);
        let (recipient, clicked) = recv_clicked(&binding, &rx);
        assert_eq!(
            recipient,
            MailboxId(43),
            "topmost (last-drawn) button's owner"
        );
        assert_eq!(clicked.id, 2, "topmost button id wins on overlap");
    }
}
