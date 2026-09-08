//! The `aether.clipboard.*` text clipboard request/reply vocabulary.
//! The capability owns these kinds (ADR-0121), and the marker layer keeps
//! them available to wasm guests without pulling in the native backend.

/// Request the current UTF-8 clipboard text.
#[aether_data::kind(name = "aether.clipboard.get_text", no_serde)]
pub struct GetClipboardText;

/// Reply to [`GetClipboardText`].
#[aether_data::kind(name = "aether.clipboard.get_text_result", eq, no_serde)]
pub enum GetClipboardTextResult {
    Ok { text: String },
    Err { error: String },
}

/// Replace the current clipboard text with `text`.
#[aether_data::kind(name = "aether.clipboard.set_text", no_serde)]
pub struct SetClipboardText {
    pub text: String,
}

/// Reply to [`SetClipboardText`].
#[aether_data::kind(name = "aether.clipboard.set_text_result", eq, no_serde)]
pub enum SetClipboardTextResult {
    Ok,
    Err { error: String },
}
