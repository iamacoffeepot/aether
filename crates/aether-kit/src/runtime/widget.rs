// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload and hands
// it off, so a by-value parameter is the contract, not a copy the body
// could borrow.
#![allow(clippy::needless_pass_by_value)]

//! The widget-compositing actor (ADR-0117).
//!
//! One `#[actor(instanced)]` type realizes every role in a widget tree —
//! root, interior, and leaf — selected by its [`WidgetConfig`]. It is
//! loaded (the root) or spawned as an inline child (everything below), and
//! the same protocol handlers drive it whichever it is:
//!
//! - The **root** (`config.root`) subscribes the frame stage once and, on
//!   each `Tick`, resets its [`Composite`], appends its own chrome, and
//!   sends [`Collect`] to each child in layout order. When its slots close
//!   it emits the whole subtree as one `DrawSolidQuads` (plus one
//!   `DrawText` per glyph run) to `aether.render` / `aether.text`.
//! - An **interior** node does the same on each inbound [`Collect`], but
//!   instead of emitting it replies its flattened composite up to its
//!   parent — withholding that reply until its own slots close, which is
//!   what carries a nested subtree's internal order up intact.
//! - A **leaf** (no children) answers a [`Collect`] with its own chrome
//!   immediately, because a slotless [`Composite`] is complete on sight.
//!
//! The whole collect cascade settles inside the single host dispatch that
//! delivered the frame: an intra-cluster send is queued and drained
//! breadth-first before control returns to the host (ADR-0114), so the
//! filled-slot counter — not a re-poke or a deadline — is the sound flush
//! trigger. A self-addressed flush would run *before* the children's
//! replies on that same FIFO, so counting is the only correct signal.
//!
//! Children are spawned lazily on the node's first activation rather than
//! in `wire`: an inline child receives only `init` (no `wire`), and `init`
//! cannot spawn, so an interior node spawns its own children from its first
//! `Collect` handler — where it holds a send-capable ctx. The root spawns
//! on its first `Tick` for the one code path.

use aether_actor::{
    ActorInitError, Manual, OutboundReply, Subname, WasmActor, WasmCtx, WasmInitCtx, actor,
};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::render::{DrawSolidQuads, SolidQuad};
use aether_capabilities::text::DrawText;
use aether_capabilities::{LifecycleCapability, RenderCapability, TextCapability};
use aether_data::Kind;
use aether_kinds::{QuadSpace, Tick};
use aether_math::Vec2;

use crate::runtime::composite::Composite;
use crate::widgets::{Collect, WidgetConfig, WidgetDrawItem, WidgetDrawList};

/// A compositing widget node. `config` fixes its role and layout;
/// `composite` accumulates its subtree each frame; `spawned` guards the
/// one-time lazy spawn of its children.
pub struct Widget {
    config: WidgetConfig,
    composite: Composite,
    spawned: bool,
}

impl Widget {
    /// Spawn this node's children once and register a slot per child. An
    /// inline child gets only `init` (which cannot spawn), so this runs
    /// from the node's first activation handler, where the ctx can spawn.
    /// A child whose subname fails validation or whose config fails to
    /// decode is skipped with a warn — its slot is never registered, so
    /// the completion counter stays honest.
    fn ensure_spawned(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        if self.spawned {
            return;
        }
        self.spawned = true;
        for spec in &self.config.children {
            let Some(child_config) = WidgetConfig::decode_from_bytes(&spec.config) else {
                tracing::warn!(
                    target: "aether_kit",
                    subname = %spec.subname,
                    "widget child config failed to decode; slot skipped",
                );
                continue;
            };
            match ctx.spawn_inline_child::<Self>(Subname::Named(&spec.subname), &child_config) {
                Ok(alias) => self
                    .composite
                    .register_slot(alias, Vec2::new(spec.origin[0], spec.origin[1])),
                Err(error) => tracing::warn!(
                    target: "aether_kit",
                    subname = %spec.subname,
                    ?error,
                    "widget child spawn failed; slot skipped",
                ),
            }
        }
    }

    /// Open a frame and fan `Collect` to every child. Resets the
    /// composite, lays down own chrome, then polls each child in layout
    /// order. A leaf (no children) is already complete, so it finishes on
    /// the spot; a node with children finishes later, from `on_draw_list`.
    fn drive_frame(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        self.ensure_spawned(ctx);
        self.composite.begin_frame();
        self.composite
            .extend_chrome(self.config.chrome.iter().cloned());
        for spec in &self.config.children {
            if let Some(child) = ctx.child(&spec.subname) {
                child.send(&Collect);
            }
        }
        if self.composite.is_complete() {
            self.finish(ctx);
        }
    }

    /// Discharge the closed composite: the root emits it to the render /
    /// text caps; an interior or leaf node replies it up to its parent.
    fn finish(&mut self, ctx: &mut WasmCtx<'_, Manual>) {
        let list = self.composite.flatten(self.config.intrinsic);
        if self.config.root {
            emit(ctx, &list);
        } else if let Some(parent) = ctx.parent() {
            parent.send(&list);
        }
    }
}

/// Emit a flattened subtree as the cluster's single render + text output:
/// one `DrawSolidQuads` for all the flat fills (in depth-first order, so
/// chrome draws under children), then one `DrawText` per glyph run. Text's
/// extra hop through the text cap lands its glyphs after the direct quad
/// batch the same frame — the fills-under-labels layering the shipped
/// `aether.ui` cap already relies on.
pub(crate) fn emit(ctx: &mut WasmCtx<'_, Manual>, list: &WidgetDrawList) {
    let quads: Vec<SolidQuad> = list
        .items
        .iter()
        .filter_map(|item| match item {
            WidgetDrawItem::Quad {
                x,
                y,
                width,
                height,
                color,
            } => Some(SolidQuad {
                x: *x,
                y: *y,
                width: *width,
                height: *height,
                color: *color,
            }),
            WidgetDrawItem::Text { .. } => None,
        })
        .collect();
    if !quads.is_empty() {
        ctx.actor::<RenderCapability>().send(&DrawSolidQuads {
            space: QuadSpace::Screen,
            quads,
        });
    }
    for item in &list.items {
        if let WidgetDrawItem::Text {
            x,
            y,
            font_id,
            text,
            size_pixels,
            color,
        } = item
        {
            ctx.actor::<TextCapability>().send(&DrawText {
                font_id: *font_id,
                text: text.clone(),
                size_pixels: *size_pixels,
                color: *color,
                origin: [*x, *y],
                space: QuadSpace::Screen,
            });
        }
    }
}

/// Compositing widget. Loaded as the root of a widget cluster or spawned
/// as an inline child within one; its [`WidgetConfig`] selects the role.
///
/// # Agent
/// Load the root with a `WidgetConfig { root: true, chrome, children }`;
/// each `children` entry carries a pre-encoded child `WidgetConfig` in its
/// `config` bytes, so the whole tree ships in one load. The cluster is one
/// render sender: the root emits a single `DrawSolidQuads` per frame with
/// every widget's quads in structural depth-first order, so a background
/// drawn as root chrome sits under the children by construction.
#[actor(instanced)]
impl WasmActor for Widget {
    type Config = WidgetConfig;
    const NAMESPACE: &'static str = "aether.kit.widget";

    fn init(config: WidgetConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Widget {
            config,
            composite: Composite::new(),
            spawned: false,
        })
    }

    /// The root subscribes the frame stage once (the root-subscribes-once
    /// pattern). `Tick` is a frame-lifecycle stage, so it rides
    /// `aether.lifecycle` (ADR-0082), not the input cap. A non-root node
    /// is driven by its parent's `Collect`, so it subscribes nothing.
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        if self.config.root {
            ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
        }
    }

    /// Root frame driver: open a frame and fan `Collect`. Non-root nodes
    /// are not tick-subscribed, so this is the root's path only.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler::manual]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_, Manual>, _tick: Tick) {
        self.drive_frame(ctx);
    }

    /// A collect poll from this node's parent. A leaf answers with its own
    /// chrome at once; an interior node fans `Collect` to its own children
    /// and withholds its reply until its slots close.
    ///
    /// # Agent
    /// Sent by a compositing parent each frame; not useful to send
    /// manually.
    #[handler::manual]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_, Manual>, _collect: Collect) {
        self.drive_frame(ctx);
    }

    /// A child's draw list. Attribute it to the child's slot by the
    /// inbound source, and when every slot has replied this frame,
    /// discharge the composite (emit at the root, reply up at an interior
    /// node).
    ///
    /// # Agent
    /// A child's reply; not useful to send manually.
    #[handler::manual]
    fn on_draw_list(&mut self, ctx: &mut WasmCtx<'_, Manual>, list: WidgetDrawList) {
        if let Some(source) = ctx.source_mailbox() {
            self.composite.fill(source, list);
        }
        if self.composite.is_complete() {
            self.finish(ctx);
        }
    }
}
