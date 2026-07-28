// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The static image widget (issue 2917).
//!
//! The image borrows a consumer-created render texture id, fits that texture
//! into its parent-owned slot, and reports the configured natural pixel size
//! through the existing widget intrinsic channel. It never acquires or
//! releases texture lifecycle authority.

use alloc::vec;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_math::Rgba;

use crate::set::apply_static_control_state;
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::{SetTheme, Theme};
use crate::{Collect, ImageConfig, ImageFit, SetWidgetState, WidgetDrawItem, WidgetDrawList, WidgetFrame};

/// Pure fit output. Destination fields are widget-local pixels; UV fields are
/// normalized texture coordinates. Keeping each semantic named avoids the
/// width/height and UV-endpoint transpositions positional aggregates invite.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ImagePlacement {
    destination_x_pixels: f32,
    destination_y_pixels: f32,
    destination_width_pixels: f32,
    destination_height_pixels: f32,
    uv_left: f32,
    uv_top: f32,
    uv_right: f32,
    uv_bottom: f32,
}

struct WideImageExtent {
    width_pixels: f64,
    height_pixels: f64,
}

struct WideUvCrop {
    left: f64,
    top: f64,
}

impl ImagePlacement {
    fn is_valid(self) -> bool {
        self.destination_x_pixels.is_finite()
            && self.destination_y_pixels.is_finite()
            && self.destination_width_pixels.is_finite()
            && self.destination_height_pixels.is_finite()
            && self.destination_width_pixels > 0.0
            && self.destination_height_pixels > 0.0
            && self.uv_left.is_finite()
            && self.uv_top.is_finite()
            && self.uv_right.is_finite()
            && self.uv_bottom.is_finite()
            && (0.0..self.uv_right).contains(&self.uv_left)
            && self.uv_right <= 1.0
            && (0.0..self.uv_bottom).contains(&self.uv_top)
            && self.uv_bottom <= 1.0
    }
}

/// A non-interactive image leaf. The parent owns its frame and the consumer
/// owns its texture; this actor owns only presentation state.
pub struct ImageWidget {
    texture_id: u32,
    natural_width_pixels: f32,
    natural_height_pixels: f32,
    fit: ImageFit,
    tint: Rgba,
    theme: Theme,
    frame: WidgetFrame,
    /// Read-only and validation are inapplicable to a static image;
    /// visibility and enabled still control absence and muted presentation.
    state: InteractionState,
}

impl ImageWidget {
    fn has_valid_natural_size(&self) -> bool {
        self.natural_width_pixels.is_finite()
            && self.natural_height_pixels.is_finite()
            && self.natural_width_pixels > 0.0
            && self.natural_height_pixels > 0.0
    }

    fn placement(&self) -> Option<ImagePlacement> {
        if !self.has_valid_natural_size()
            || !self.frame.x.is_finite()
            || !self.frame.y.is_finite()
            || !self.frame.width.is_finite()
            || !self.frame.height.is_finite()
            || self.frame.width <= 0.0
            || self.frame.height <= 0.0
        {
            return None;
        }

        let frame_width = self.frame.width;
        let frame_height = self.frame.height;
        let natural_width = self.natural_width_pixels;
        let natural_height = self.natural_height_pixels;
        let full = || ImagePlacement {
            destination_x_pixels: 0.0,
            destination_y_pixels: 0.0,
            destination_width_pixels: frame_width,
            destination_height_pixels: frame_height,
            uv_left: 0.0,
            uv_top: 0.0,
            uv_right: 1.0,
            uv_bottom: 1.0,
        };

        let placement = match self.fit {
            ImageFit::Fill => full(),
            ImageFit::Contain => {
                // Compare cross-products and derive the unconstrained axis in
                // f64. Every input is a finite f32, so both products and the
                // resulting dimension fit in f64 even when the equivalent
                // f32 fit ratios would overflow (for example 1e30 / 1e-30).
                let frame_width_wide = f64::from(frame_width);
                let frame_height_wide = f64::from(frame_height);
                let natural_width_wide = f64::from(natural_width);
                let natural_height_wide = f64::from(natural_height);
                let source_is_wider = natural_width_wide * frame_height_wide > natural_height_wide * frame_width_wide;
                let extent = if source_is_wider {
                    WideImageExtent {
                        width_pixels: frame_width_wide,
                        height_pixels: frame_width_wide * natural_height_wide / natural_width_wide,
                    }
                } else {
                    WideImageExtent {
                        width_pixels: frame_height_wide * natural_width_wide / natural_height_wide,
                        height_pixels: frame_height_wide,
                    }
                };
                let width = positive_f32(extent.width_pixels)?;
                let height = positive_f32(extent.height_pixels)?;
                ImagePlacement {
                    destination_x_pixels: (frame_width - width) * 0.5,
                    destination_y_pixels: (frame_height - height) * 0.5,
                    destination_width_pixels: width,
                    destination_height_pixels: height,
                    ..full()
                }
            }
            ImageFit::Cover => {
                // Work directly in normalized crop fractions. Cross-products
                // in f64 avoid overflowing either f32 scale ratio, while a
                // final narrowing that cannot represent distinct UV endpoints
                // is rejected by `ImagePlacement::is_valid` below.
                let frame_width_wide = f64::from(frame_width);
                let frame_height_wide = f64::from(frame_height);
                let natural_width_wide = f64::from(natural_width);
                let natural_height_wide = f64::from(natural_height);
                let source_cross = natural_width_wide * frame_height_wide;
                let frame_cross = natural_height_wide * frame_width_wide;
                let crop = if source_cross > frame_cross {
                    WideUvCrop { left: 0.5 * (1.0 - frame_cross / source_cross), top: 0.0 }
                } else {
                    WideUvCrop { left: 0.0, top: 0.5 * (1.0 - source_cross / frame_cross) }
                };
                let uv_left = finite_f32(crop.left)?;
                let uv_top = finite_f32(crop.top)?;
                ImagePlacement { uv_left, uv_top, uv_right: 1.0 - uv_left, uv_bottom: 1.0 - uv_top, ..full() }
            }
            ImageFit::Natural => ImagePlacement {
                destination_x_pixels: (frame_width - natural_width) * 0.5,
                destination_y_pixels: (frame_height - natural_height) * 0.5,
                destination_width_pixels: natural_width,
                destination_height_pixels: natural_height,
                ..full()
            },
        };
        placement.is_valid().then_some(placement)
    }

    fn draw_list(&self) -> WidgetDrawList {
        let intrinsic =
            self.has_valid_natural_size().then_some([self.natural_width_pixels, self.natural_height_pixels]);
        if !self.state.is_visible() {
            return WidgetDrawList { intrinsic, items: Vec::new() };
        }
        let Some(placement) = self.placement() else {
            return WidgetDrawList { intrinsic, items: Vec::new() };
        };
        WidgetDrawList {
            intrinsic,
            items: vec![WidgetDrawItem::TexturedQuad {
                texture_id: self.texture_id,
                x: placement.destination_x_pixels,
                y: placement.destination_y_pixels,
                width: placement.destination_width_pixels,
                height: placement.destination_height_pixels,
                u0: placement.uv_left,
                v0: placement.uv_top,
                u1: placement.uv_right,
                v1: placement.uv_bottom,
                tint: self.theme.fill(self.tint, self.state.theme_state(false)),
                clip: None,
            }],
        }
    }

    fn apply_config(&mut self, config: ImageConfig) -> bool {
        self.texture_id = config.texture_id;
        self.natural_width_pixels = config.natural_width_pixels;
        self.natural_height_pixels = config.natural_height_pixels;
        self.fit = config.fit;
        self.tint = config.tint;
        self.theme = config.theme;
        self.state.replace(config.state)
    }
}

fn positive_f32(value: f64) -> Option<f32> {
    let value = finite_f32(value)?;
    (value > 0.0).then_some(value)
}

fn finite_f32(value: f64) -> Option<f32> {
    #[allow(clippy::cast_possible_truncation)]
    let narrowed = value as f32;
    narrowed.is_finite().then_some(narrowed)
}

/// A static image widget. Spawned inline by a panel root with an
/// [`ImageConfig`]. The texture id is borrowed: its creator remains
/// responsible for update and destruction.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `ImageConfig` again to replace texture or presentation in place.
#[actor(instanced, composable)]
impl WasmActor for ImageWidget {
    type Config = ImageConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.image";

    fn init(config: ImageConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self {
            texture_id: config.texture_id,
            natural_width_pixels: config.natural_width_pixels,
            natural_height_pixels: config.natural_height_pixels,
            fit: config.fit,
            tint: config.tint,
            theme: config.theme,
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            state: InteractionState::new(config.state),
        })
    }

    /// Replace the borrowed texture and presentation without changing the
    /// parent-owned frame.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: ImageConfig) {
        if self.apply_config(config) {
            emit_state_changed(ctx, &self.state);
        }
    }

    /// Update external availability without changing image presentation.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        apply_static_control_state(ctx, &mut self.state, set.state);
    }

    /// Restyle disabled presentation.
    #[handler::single]
    fn on_set_theme(&mut self, _ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.theme = set.theme;
    }

    /// Cache the layout rect the root assigned without taking layout
    /// authority from it.
    #[handler::single]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        self.frame = frame;
    }

    /// Reply with valid intrinsic size and at most one fitted textured item.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if let Some(parent) = ctx.parent() {
            parent.send(&self.draw_list());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WidgetControlState;
    use crate::theme::ThemeState;

    fn image(fit: ImageFit) -> ImageWidget {
        ImageWidget {
            texture_id: 7,
            natural_width_pixels: 60.0,
            natural_height_pixels: 30.0,
            fit,
            tint: Rgba::WHITE,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 90.0, height: 60.0 },
            state: InteractionState::new(WidgetControlState::default()),
        }
    }

    fn placement(widget: &ImageWidget) -> ImagePlacement {
        widget.placement().expect("valid placement")
    }

    #[test]
    fn fit_modes_name_exact_destination_and_uv_semantics() {
        assert_eq!(
            placement(&image(ImageFit::Fill)),
            ImagePlacement {
                destination_x_pixels: 0.0,
                destination_y_pixels: 0.0,
                destination_width_pixels: 90.0,
                destination_height_pixels: 60.0,
                uv_left: 0.0,
                uv_top: 0.0,
                uv_right: 1.0,
                uv_bottom: 1.0,
            }
        );
        assert_eq!(
            placement(&image(ImageFit::Contain)),
            ImagePlacement {
                destination_x_pixels: 0.0,
                destination_y_pixels: 7.5,
                destination_width_pixels: 90.0,
                destination_height_pixels: 45.0,
                uv_left: 0.0,
                uv_top: 0.0,
                uv_right: 1.0,
                uv_bottom: 1.0,
            }
        );
        assert_eq!(
            placement(&image(ImageFit::Cover)),
            ImagePlacement {
                destination_x_pixels: 0.0,
                destination_y_pixels: 0.0,
                destination_width_pixels: 90.0,
                destination_height_pixels: 60.0,
                uv_left: 0.125,
                uv_top: 0.0,
                uv_right: 0.875,
                uv_bottom: 1.0,
            }
        );
        assert_eq!(
            placement(&image(ImageFit::Natural)),
            ImagePlacement {
                destination_x_pixels: 15.0,
                destination_y_pixels: 15.0,
                destination_width_pixels: 60.0,
                destination_height_pixels: 30.0,
                uv_left: 0.0,
                uv_top: 0.0,
                uv_right: 1.0,
                uv_bottom: 1.0,
            }
        );
    }

    #[test]
    fn contain_keeps_extreme_positive_finite_sizes_visible() {
        let mut widget = image(ImageFit::Contain);
        widget.natural_width_pixels = 1.0e-30;
        widget.natural_height_pixels = 1.0e-30;
        widget.frame.width = 1.0e30;
        widget.frame.height = 5.0e29;

        assert_eq!(
            placement(&widget),
            ImagePlacement {
                destination_x_pixels: 2.5e29,
                destination_y_pixels: 0.0,
                destination_width_pixels: 5.0e29,
                destination_height_pixels: 5.0e29,
                uv_left: 0.0,
                uv_top: 0.0,
                uv_right: 1.0,
                uv_bottom: 1.0,
            },
            "valid extreme ratios must not overflow to an absent image",
        );
    }

    #[test]
    fn cover_keeps_extreme_positive_finite_sizes_visible() {
        let mut widget = image(ImageFit::Cover);
        widget.natural_width_pixels = 1.0e-30;
        widget.natural_height_pixels = 1.0e-30;
        widget.frame.width = 1.0e30;
        widget.frame.height = 5.0e29;

        assert_eq!(
            placement(&widget),
            ImagePlacement {
                destination_x_pixels: 0.0,
                destination_y_pixels: 0.0,
                destination_width_pixels: 1.0e30,
                destination_height_pixels: 5.0e29,
                uv_left: 0.0,
                uv_top: 0.25,
                uv_right: 1.0,
                uv_bottom: 0.75,
            },
            "valid extreme ratios must retain a representable crop",
        );
    }

    #[test]
    fn invalid_natural_size_and_frame_never_emit_texture() {
        for invalid in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let mut width = image(ImageFit::Contain);
            width.natural_width_pixels = invalid;
            let invalid_natural = width.draw_list();
            assert_eq!(invalid_natural.intrinsic, None);
            assert!(invalid_natural.items.is_empty());

            let mut height = image(ImageFit::Contain);
            height.natural_height_pixels = invalid;
            let invalid_natural = height.draw_list();
            assert_eq!(invalid_natural.intrinsic, None);
            assert!(invalid_natural.items.is_empty());

            let mut frame_width = image(ImageFit::Contain);
            frame_width.frame.width = invalid;
            let invalid_frame = frame_width.draw_list();
            assert_eq!(invalid_frame.intrinsic, Some([60.0, 30.0]));
            assert!(invalid_frame.items.is_empty());

            let mut frame_height = image(ImageFit::Contain);
            frame_height.frame.height = invalid;
            let invalid_frame = frame_height.draw_list();
            assert_eq!(invalid_frame.intrinsic, Some([60.0, 30.0]));
            assert!(invalid_frame.items.is_empty());
        }

        for invalid in [f32::NAN, f32::INFINITY] {
            let mut frame_x = image(ImageFit::Contain);
            frame_x.frame.x = invalid;
            assert!(frame_x.draw_list().items.is_empty());

            let mut frame_y = image(ImageFit::Contain);
            frame_y.frame.y = invalid;
            assert!(frame_y.draw_list().items.is_empty());
        }

        let mut first_consumer_texture = image(ImageFit::Contain);
        first_consumer_texture.texture_id = 0;
        assert!(matches!(
            first_consumer_texture.draw_list().items[0],
            WidgetDrawItem::TexturedQuad { texture_id: 0, .. }
        ));
    }

    #[test]
    fn hidden_retains_intrinsic_and_disabled_applies_theme_tint() {
        let mut widget = image(ImageFit::Natural);
        let mut hidden = widget.state.control().clone();
        hidden.visible = false;
        assert!(widget.state.replace(hidden));
        let hidden = widget.draw_list();
        assert_eq!(hidden.intrinsic, Some([60.0, 30.0]));
        assert!(hidden.items.is_empty());

        let mut disabled = widget.state.control().clone();
        disabled.visible = true;
        disabled.enabled = false;
        assert!(widget.state.replace(disabled));
        let disabled = widget.draw_list();
        assert_eq!(disabled.items.len(), 1);
        let WidgetDrawItem::TexturedQuad { tint, .. } = disabled.items[0] else {
            panic!("image emits one textured quad")
        };
        assert_eq!(tint, Theme::DEFAULT.fill(Rgba::WHITE, ThemeState::Disabled));
    }

    #[test]
    fn replacement_updates_presentation_without_relayout() {
        let mut widget = image(ImageFit::Fill);
        let frame = WidgetFrame {
            x: widget.frame.x,
            y: widget.frame.y,
            width: widget.frame.width,
            height: widget.frame.height,
        };
        let replacement_tint = Rgba::new(0.2, 0.4, 0.6, 0.8);
        let state = widget.state.control().clone();
        assert!(!widget.apply_config(ImageConfig {
            texture_id: 19,
            natural_width_pixels: 30.0,
            natural_height_pixels: 45.0,
            fit: ImageFit::Natural,
            tint: replacement_tint,
            theme: Theme { disabled_alpha: 0.25, ..Theme::DEFAULT },
            state,
        }));

        assert_eq!(widget.frame.width, frame.width);
        assert_eq!(widget.frame.height, frame.height);
        let list = widget.draw_list();
        assert_eq!(list.intrinsic, Some([30.0, 45.0]));
        assert_eq!(list.items.len(), 1);
        assert!(matches!(
            list.items[0],
            WidgetDrawItem::TexturedQuad {
                texture_id: 19,
                x: 30.0,
                y: 7.5,
                width: 30.0,
                height: 45.0,
                tint,
                ..
            } if tint == replacement_tint
        ));
    }
}
