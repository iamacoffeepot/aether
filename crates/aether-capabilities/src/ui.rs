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

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings of
// the mod (always-on, outside the cfg gate).
use aether_kinds::{UiBar, UiLabel, UiPanel};

#[aether_actor::bridge(singleton, feature = "ui-native")]
mod native {
    use aether_actor::actor;
    use aether_kinds::{DrawSolidQuads, DrawText, QuadSpace, SolidQuad};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    use crate::render::RenderCapability;
    use crate::text::TextCapability;

    use super::{UiBar, UiLabel, UiPanel};

    /// `aether.ui` mailbox cap. CPU-only — no GPU handles, just a
    /// stateless translator from widget mail to render + text mail.
    pub struct UiCapability;

    #[actor]
    impl NativeActor for UiCapability {
        type Config = ();

        /// ADR-0107 chassis-owned mailbox.
        const NAMESPACE: &'static str = "aether.ui";

        /// No substrate resources to claim — the cap holds no state.
        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        /// Draw a flat-colored panel.
        ///
        /// # Agent
        /// Fire-and-forget. Forwards one `DrawSolidQuads` (screen-space) to
        /// `aether.render` the same tick. Resend every frame.
        // Cap holds no state; `&mut self` is the required handler ABI.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_panel(&mut self, ctx: &mut NativeCtx<'_>, mail: UiPanel) {
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
            let _ = ctx.actor::<RenderCapability>().send_traced(ctx, &draw);
        }

        /// Draw a two-layer progress bar.
        ///
        /// # Agent
        /// Fire-and-forget. Forwards a two-quad `DrawSolidQuads` (screen-
        /// space, track then frac-sized fill) to `aether.render` the same
        /// tick. `frac` is clamped to [0, 1]. Resend every frame.
        // Cap holds no state; `&mut self` is the required handler ABI.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_bar(&mut self, ctx: &mut NativeCtx<'_>, mail: UiBar) {
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
            let _ = ctx.actor::<RenderCapability>().send_traced(ctx, &draw);
        }

        /// Draw a text label.
        ///
        /// # Agent
        /// Fire-and-forget. Forwards one `DrawText` (screen-space) to
        /// `aether.text` the same tick. The string flows from the screen-pixel
        /// `(x, y)` along the baseline, where `(0, 0)` is the window's
        /// top-left corner. An unknown `font_id` warn-drops in the text cap.
        /// Resend every frame.
        // Cap holds no state; `&mut self` is the required handler ABI.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_label(&mut self, ctx: &mut NativeCtx<'_>, mail: UiLabel) {
            let draw = DrawText {
                font_id: mail.font_id,
                text: mail.text,
                size_pixels: mail.size_pixels,
                color: mail.color,
                origin: [mail.x, mail.y],
                space: QuadSpace::Screen,
            };
            let _ = ctx.actor::<TextCapability>().send_traced(ctx, &draw);
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use std::sync::Arc;
        use std::sync::mpsc::Receiver;
        use std::time::Duration;

        use aether_data::{Kind, MailId, MailboxId, SessionToken, Source, SourceAddr, Uuid};
        use aether_kinds::{DrawSolidQuads, DrawText};
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::mail::outbound::EgressEvent;

        use super::*;
        use crate::test_chassis::test_mailer_and_rx;

        fn session_sender() -> Source {
            Source::to(SourceAddr::Session(SessionToken(Uuid::nil())))
        }

        fn ctx_binding() -> (Arc<NativeBinding>, Receiver<EgressEvent>) {
            let (mailer, rx) = test_mailer_and_rx();
            let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
            (binding, rx)
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
            let mut cap = UiCapability;
            let (binding, rx) = ctx_binding();
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_panel(
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
            let mut cap = UiCapability;
            let (binding, rx) = ctx_binding();
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_bar(
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
            let decoded: DrawSolidQuads =
                postcard::from_bytes(&payload).expect("test: decodes DrawSolidQuads");
            assert_eq!(decoded.quads.len(), 2, "bar emits track + fill quad");
            assert_eq!(decoded.quads[1].width, 100.0, "fill is frac * full width");
        }

        #[test]
        fn bar_clamps_frac_above_one() {
            let mut cap = UiCapability;
            let (binding, rx) = ctx_binding();
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_bar(
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
            let decoded: DrawSolidQuads =
                postcard::from_bytes(&payload).expect("test: decodes DrawSolidQuads");
            assert_eq!(
                decoded.quads[1].width, 200.0,
                "frac > 1 clamps to full width"
            );
        }

        #[test]
        fn bar_clamps_frac_below_zero() {
            let mut cap = UiCapability;
            let (binding, rx) = ctx_binding();
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_bar(
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
            let decoded: DrawSolidQuads =
                postcard::from_bytes(&payload).expect("test: decodes DrawSolidQuads");
            assert_eq!(decoded.quads[1].width, 0.0, "frac < 0 clamps to zero width");
        }

        #[test]
        fn label_produces_draw_text_screen_space() {
            let mut cap = UiCapability;
            let (binding, rx) = ctx_binding();
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_label(
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
            let decoded: DrawText = postcard::from_bytes(&payload).expect("test: decodes DrawText");
            assert_eq!(decoded.space, QuadSpace::Screen, "label uses Screen space");
            assert_eq!(decoded.origin, [50.0, 100.0], "label origin is (x, y)");
        }
    }
}
