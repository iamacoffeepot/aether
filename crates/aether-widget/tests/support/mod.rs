//! The composition every widget scenario shares. Wherever the widget module
//! loads, its actors declare Window, Lifecycle, Render, Text and Clipboard; the
//! harness basics compose the first two, and each suite composes its own render.

use aether_clipboard::{ClipboardCapability, ClipboardParams};
use aether_harness_substrate::SubstrateHarnessBuilder;
use aether_text::TextCapability;

/// Compose text and the deterministic in-memory clipboard. Text declares render
/// and fs, so the caller also composes a render (the real one or the headless
/// stub) and namespace roots.
pub fn widget_caps(builder: SubstrateHarnessBuilder) -> SubstrateHarnessBuilder {
    builder.with_actor::<TextCapability>(()).with_actor::<ClipboardCapability>(ClipboardParams::InMemory)
}
