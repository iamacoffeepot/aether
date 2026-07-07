//! The widget-compositing wire vocabulary (ADR-0117).
//!
//! A widget tree is a cluster of inline-child actors (ADR-0114). Each
//! widget draws in its own local coordinates and never touches
//! `aether.render`; instead the cluster's root composites the whole
//! subtree and emits it as **one** ordered batch, so a dense panel costs
//! one render sender rather than one per widget and draw order is the
//! structural depth-first traversal of the tree.
//!
//! Two kinds carry the protocol:
//!
//! - [`Collect`] flows data-down. A compositing node sends it to each of
//!   its children, in its own layout order, once per frame.
//! - [`WidgetDrawList`] flows events-up. A widget's [`Collect`] handler
//!   **always** replies one — empty when it draws nothing — to its
//!   parent. That always-reply contract is what lets the parent close on
//!   a filled-slot counter rather than a timeout: the intra-cluster send
//!   queue drains breadth-first inside the one host dispatch that
//!   delivered the frame, so counting filled slots against the fanned
//!   count is a structural completion signal, not a temporal guess.
//!
//! Coordinates on a [`WidgetDrawItem`] are local to the widget's own
//! origin; the parent offsets each child's items by the rect it assigned
//! that child from its own layout table (layout is data-down, so no
//! origin handshake beyond attribution). [`WidgetConfig`] is the
//! non-recursive tree description a compositing widget is loaded or
//! spawned with — each child rides as an opaque pre-encoded
//! [`WidgetConfig`] in [`WidgetChildSpec::config`], which breaks the
//! type-level recursion a nested tree would otherwise form.

use alloc::string::String;
use alloc::vec::Vec;

use aether_math::Vec2;
use serde::{Deserialize, Serialize};

/// `aether.kit.widget.collect` — a per-frame poll a compositing node
/// sends to each of its children in layout order. The child answers with
/// its [`WidgetDrawList`]. Fieldless: the poll carries no data, because
/// layout already flowed down at spawn (a child's rect is its parent's to
/// assign) and only geometry flows back up.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.collect")]
pub struct Collect;

/// One draw in a [`WidgetDrawList`], in the widget's own local
/// coordinates (offset by the parent at composite time). A widget emits a
/// heterogeneous run of these in authored order — the single-list shape
/// preserves per-item quad/text interleave, which a two-vector
/// quads-then-texts split would foreclose. Not a kind on its own; only
/// addressable inside [`WidgetDrawList::items`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum WidgetDrawItem {
    /// A flat-colored rectangle. `(x, y)` is the top-left corner and
    /// `(width, height)` the size, in the widget's local pixels; `color`
    /// is a linear RGBA value.
    Quad {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: [f32; 4],
    },
    /// A glyph run. `(x, y)` is the baseline origin in local pixels;
    /// `font_id` names a session-scoped font loaded through `aether.text`;
    /// `color` is a linear RGBA multiplier over glyph coverage.
    Text {
        x: f32,
        y: f32,
        font_id: u32,
        text: String,
        size_pixels: f32,
        color: [f32; 4],
    },
}

impl WidgetDrawItem {
    /// This item translated by `by` — the offset the parent applies when
    /// it files a child's list into that child's assigned slot. Composing
    /// offsets down the reply chain (each interior node offsets its own
    /// children before replying up) accumulates a node's absolute
    /// position, so the root emits screen-correct geometry.
    #[must_use]
    pub fn offset(&self, by: Vec2) -> Self {
        match self {
            Self::Quad {
                x,
                y,
                width,
                height,
                color,
            } => Self::Quad {
                x: x + by.x,
                y: y + by.y,
                width: *width,
                height: *height,
                color: *color,
            },
            Self::Text {
                x,
                y,
                font_id,
                text,
                size_pixels,
                color,
            } => Self::Text {
                x: x + by.x,
                y: y + by.y,
                font_id: *font_id,
                text: text.clone(),
                size_pixels: *size_pixels,
                color: *color,
            },
        }
    }
}

/// `aether.kit.widget.draw_list` — a widget's reply to a [`Collect`], the
/// one channel that flows up the tree. `items` are the widget's draws in
/// authored order, in its local coordinates; `intrinsic` is its measured
/// content size (`[width, height]`) when the parent needs it to position a
/// content-sized slot — a cached event, never a pull — and `None` when the
/// widget's size is externally fixed.
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq,
)]
#[kind(name = "aether.kit.widget.draw_list")]
pub struct WidgetDrawList {
    pub intrinsic: Option<[f32; 2]>,
    pub items: Vec<WidgetDrawItem>,
}

/// One child's placement in a compositing node's layout table. `subname`
/// is the inline-child address segment the parent spawns and collects it
/// under; `origin` is the local-pixel offset the parent applies to the
/// child's every draw. `config` is the child's own [`WidgetConfig`],
/// pre-encoded to bytes — carrying it opaquely (rather than a nested
/// [`WidgetConfig`] by value) is what lets a tree nest without forming a
/// self-referential schema, which a recursive `const SCHEMA` cannot
/// express.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct WidgetChildSpec {
    pub subname: String,
    pub origin: [f32; 2],
    pub config: Vec<u8>,
}

/// `aether.kit.widget.config` — the tree a compositing widget is
/// instantiated with, at load (the root) or at `spawn_inline_child` (every
/// interior and leaf). `root` marks the node that drives the frame and
/// emits to `aether.render` / `aether.text`; a non-root node instead
/// replies its composite up to its parent. `chrome` is the node's own
/// draws in local coordinates; `intrinsic` is the size it reports up;
/// `children` is its ordered layout table, each carrying its own
/// pre-encoded config.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.kit.widget.config")]
pub struct WidgetConfig {
    pub root: bool,
    pub chrome: Vec<WidgetDrawItem>,
    pub intrinsic: Option<[f32; 2]>,
    pub children: Vec<WidgetChildSpec>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_offset_translates_position_and_keeps_size() {
        let item = WidgetDrawItem::Quad {
            x: 3.0,
            y: 5.0,
            width: 10.0,
            height: 4.0,
            color: [1.0, 0.0, 0.0, 1.0],
        };
        assert_eq!(
            item.offset(Vec2::new(100.0, 20.0)),
            WidgetDrawItem::Quad {
                x: 103.0,
                y: 25.0,
                width: 10.0,
                height: 4.0,
                color: [1.0, 0.0, 0.0, 1.0],
            },
            "offset moves the corner by the vector and leaves the extent untouched",
        );
    }

    #[test]
    fn text_offset_translates_baseline_and_keeps_content() {
        let item = WidgetDrawItem::Text {
            x: 1.0,
            y: 2.0,
            font_id: 7,
            text: "hp".into(),
            size_pixels: 12.0,
            color: [1.0, 1.0, 1.0, 1.0],
        };
        assert_eq!(
            item.offset(Vec2::new(10.0, 40.0)),
            WidgetDrawItem::Text {
                x: 11.0,
                y: 42.0,
                font_id: 7,
                text: "hp".into(),
                size_pixels: 12.0,
                color: [1.0, 1.0, 1.0, 1.0],
            },
            "offset moves the baseline and preserves the glyph run",
        );
    }
}
