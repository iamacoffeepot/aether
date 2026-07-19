//! The `aether.clipboard.*` text clipboard request/reply vocabulary.
//! The capability owns these kinds (ADR-0121), and the marker layer keeps
//! them available to wasm guests without pulling in the native backend.

use serde::{Deserialize, Serialize};

/// Request the current UTF-8 clipboard text.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.clipboard.get_text")]
pub struct GetClipboardText;

/// Reply to [`GetClipboardText`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.clipboard.get_text_result")]
pub enum GetClipboardTextResult {
    Ok { text: String },
    Err { error: String },
}

/// Replace the current clipboard text with `text`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.clipboard.set_text")]
pub struct SetClipboardText {
    pub text: String,
}

/// Reply to [`SetClipboardText`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.clipboard.set_text_result")]
pub enum SetClipboardTextResult {
    Ok,
    Err { error: String },
}
