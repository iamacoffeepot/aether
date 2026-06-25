//! The `aether.text` runtime half (ADR-0122 identity/runtime split). Compiled
//! only under `feature = "text-native"` (the `mod runtime;` declaration in the
//! parent carries the gate), so a transport-only build of the `TextCapability`
//! identity never names these types nor pulls `fontdue` / `aether_substrate`.
//! The substrate-typed imports are gated once by this module rather than
//! line-by-line; the `#[actor] impl` reaches the state, ctx types, runtime-only
//! types, and helpers through the single `use runtime::*` glob in the parent.

use std::collections::HashMap;
use std::collections::VecDeque;

pub use std::sync::Arc;

pub use aether_actor::OutboundReply;
pub use aether_data::Source;
pub use aether_kinds::QuadSpace;
pub use aether_substrate::Manual;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
pub use aether_substrate::chassis::error::BootError;

pub use crate::fs::{FsCapability, Read};
pub use crate::render::{CreateTexture, RenderCapability, TexturedQuad, UpdateTexture};

// The atlas types the state struct + helpers name. Plain `use` (not a
// `pub use` re-export): the submodule items are `pub(super)`, so a wider
// re-export is disallowed — the handlers in the parent that name atlas /
// layout symbols import them straight from `super::atlas` / `super::layout`.
use super::atlas::{ATLAS_SIZE, Atlas, AtlasEntry};

/// Which reply shape a parked font request is owed once its font is
/// resident. `load_font` and the `font_metrics` grab share the
/// `aether.fs` fetch + parse path; this rides along so the completion
/// arm replies in the caller's shape.
#[derive(Clone, Copy)]
pub enum PendingReply {
    /// Reply `LoadFontResult` — the original `load_font` caller.
    LoadFont,
    /// Reply `FontMetricsResult` — a `font_metrics` grab that missed
    /// the resident registry and triggered a load.
    FontMetrics,
}

/// A font request parked while its `aether.fs.read` is in flight,
/// keyed in [`TextCapabilityState::pending_fonts`] by the echoed
/// `(namespace, path)`. Carries the original requester so the deferred
/// reply lands on the caller, plus the shape that reply takes.
pub struct PendingFont {
    pub source: Source,
    pub reply: PendingReply,
}

/// Context carried through the font-parse task so the completion arm
/// can shape the reply the parked request is owed.
pub struct FontParseContext {
    pub namespace: String,
    pub path: String,
    pub name: String,
    pub reply: PendingReply,
}

/// A successfully parsed font plus the byte length the reply reports as
/// `resident_bytes`.
pub struct ParsedFont {
    pub font: Arc<fontdue::Font>,
    pub resident_bytes: u64,
}

/// Off-hot-path parse outcome — `Err` carries the reason the cap relays
/// as `LoadFontResult::Err`.
pub type FontParseOutput = Result<ParsedFont, String>;

/// `aether.text` runtime state (ADR-0105). CPU-only — no GPU handles,
/// just the font registry, the glyph atlas, and the parked `load_font`
/// requests. The dispatcher holds this as the cap's state and routes
/// envelopes through the macro-emitted `Dispatch` impl; the addressing
/// identity is the distinct ZST [`super::TextCapability`]. Living in this
/// private module keeps it `pub`-enough to satisfy the
/// `NativeActor::State` interface without exposing it as crate-public API.
pub struct TextCapabilityState {
    /// Session-scoped font registry. Index is the `font_id` a
    /// `LoadFontResult::Ok` handed back and `DrawText.font_id` names.
    pub(super) fonts: HashMap<u32, Arc<fontdue::Font>>,
    /// Reverse index from `(namespace, path)` to the `font_id` that
    /// file is resident under. Dedups the registry: a repeat load or
    /// a `font_metrics` grab of the same file reuses one resident
    /// font and a stable id rather than parsing a second copy.
    pub(super) font_ids: HashMap<(String, String), u32>,
    /// Next `font_id` to assign — monotonic, session-scoped.
    pub(super) next_font_id: u32,
    /// `load_font` requests awaiting their `aether.fs.read` reply,
    /// keyed by the echoed `(namespace, path)`. A `VecDeque` so
    /// concurrent loads of the same path correlate FIFO.
    pub(super) pending_fonts: HashMap<(String, String), VecDeque<PendingFont>>,
    /// The shelf-packed glyph atlas (CPU-side source of truth).
    pub(super) atlas: Atlas,
    /// The render-cap `texture_id` backing [`Self::atlas`], once
    /// `create_texture` has replied. `None` until then.
    pub(super) atlas_texture_id: Option<u32>,
    /// `true` between sending `create_texture` and its reply, so a
    /// burst of `draw`s sends exactly one creation request.
    pub(super) atlas_create_inflight: bool,
}

impl TextCapabilityState {
    pub(super) fn new() -> Self {
        Self {
            fonts: HashMap::new(),
            font_ids: HashMap::new(),
            next_font_id: 0,
            pending_fonts: HashMap::new(),
            atlas: Atlas::new(),
            atlas_texture_id: None,
            atlas_create_inflight: false,
        }
    }

    /// Pop the oldest `load_font` parked under `(namespace, path)`.
    pub(super) fn take_pending(&mut self, namespace: &str, path: &str) -> Option<PendingFont> {
        let key = (namespace.to_owned(), path.to_owned());
        let queue = self.pending_fonts.get_mut(&key)?;
        let pending = queue.pop_front();
        if queue.is_empty() {
            self.pending_fonts.remove(&key);
        }
        pending
    }

    /// Register a parsed font under a session-scoped `font_id`,
    /// deduped by `(namespace, path)`: a path already resident
    /// returns its existing id (and drops the freshly-parsed `font`),
    /// so repeat loads and metric grabs of one file share a single
    /// resident font and a stable id.
    pub(super) fn register_font(
        &mut self,
        namespace: &str,
        path: &str,
        font: Arc<fontdue::Font>,
    ) -> u32 {
        let key = (namespace.to_owned(), path.to_owned());
        if let Some(&existing) = self.font_ids.get(&key) {
            return existing;
        }
        let font_id = self.next_font_id;
        self.next_font_id = self.next_font_id.saturating_add(1);
        self.fonts.insert(font_id, font);
        self.font_ids.insert(key, font_id);
        font_id
    }

    /// Park a font request keyed `(namespace, path)` and forward its
    /// `aether.fs.read`. The `ReadResult` routes back to
    /// `on_read_result`, which parses the bytes and replies in the
    /// shape `reply` selects.
    pub(super) fn forward_font_read(
        &mut self,
        ctx: &mut NativeCtx<'_, Manual>,
        namespace: String,
        path: String,
        reply: PendingReply,
    ) {
        let source = ctx.reply_target();
        self.pending_fonts
            .entry((namespace.clone(), path.clone()))
            .or_default()
            .push_back(PendingFont { source, reply });

        // Forward the read to the single fs resolver (ADR-0041); the
        // `ReadResult` routes back to `on_read_result`, which parses
        // it.
        let read = Read { namespace, path };
        ctx.actor::<FsCapability>().send(&read);
    }

    /// Send `create_texture` for the zeroed atlas, unless a creation is
    /// already in flight. The reply (`CreateTextureResult`) routes back
    /// to this cap's own mailbox, where `on_create_texture_result`
    /// stores the assigned id.
    pub(super) fn ensure_atlas_texture(&mut self, ctx: &mut NativeCtx<'_>) {
        if self.atlas_texture_id.is_some() || self.atlas_create_inflight {
            return;
        }
        let create = CreateTexture {
            width: ATLAS_SIZE,
            height: ATLAS_SIZE,
            pixels: self.atlas.pixels().to_vec(),
        };
        // Address the render cap through the lineage-correct resolver
        // (ADR-0099); `send` propagates this handler's chain by default
        // so the `CreateTextureResult` reply settles back into it.
        ctx.actor::<RenderCapability>().send(&create);
        self.atlas_create_inflight = true;
    }

    /// Send one `update_texture` for a newly-rasterized glyph's rect.
    pub(super) fn upload_glyph(
        &self,
        ctx: &mut NativeCtx<'_>,
        texture_id: u32,
        entry: &AtlasEntry,
    ) {
        let update = UpdateTexture {
            texture_id,
            x: entry.x,
            y: entry.y,
            width: entry.width,
            height: entry.height,
            pixels: self.atlas.rect_rgba(entry),
        };
        ctx.actor::<RenderCapability>().send(&update);
    }

    /// Re-sync the GPU side after an atlas reset by uploading the full
    /// zeroed buffer. This ensures the render cap's staged pixels are a
    /// clean mirror of the reset CPU atlas before per-glyph uploads layer
    /// on top. Uses the same `update_texture` path as `upload_glyph`.
    pub(super) fn resync_atlas(&self, ctx: &mut NativeCtx<'_>, texture_id: u32) {
        let update = UpdateTexture {
            texture_id,
            x: 0,
            y: 0,
            width: ATLAS_SIZE,
            height: ATLAS_SIZE,
            pixels: self.atlas.pixels().to_vec(),
        };
        ctx.actor::<RenderCapability>().send(&update);
    }
}
