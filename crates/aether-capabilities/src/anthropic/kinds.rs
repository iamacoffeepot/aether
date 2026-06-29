//! Wire kinds for the `aether.anthropic` capability (ADR-0050, ADR-0121).
//!
//! The seven anthropic-specific types moved here from `aether-kinds` so the
//! capability owns its mail vocabulary. [`aether_kinds::Usage`] stays central
//! — it is shared with the `aether.gemini` result kinds, so moving it here
//! would force gemini to reach across capabilities.

use serde::{Deserialize, Serialize};

use aether_kinds::Usage;

/// Conversation role on a [`Message`]. The Messages API only
/// distinguishes user vs assistant turns; `system` rides as a
/// separate top-level field on the request, not a role.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// One turn in an Anthropic completion request. `content` is the
/// flat text of the turn (v1 doesn't model multi-part content
/// blocks); `role` distinguishes user from assistant.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

/// Structured failure reason for an Anthropic completion (ADR-0050
/// §1). Typed variants cover the branches a caller routinely
/// matches on — `Overloaded` / `RateLimited` → back off,
/// `ContextLengthExceeded` → trim the prompt, `Unauthorized` →
/// config issue, `ContentPolicyRefused` → surface to the user,
/// `CliNotFound` → the `claude` binary isn't on PATH,
/// `UnknownModel` → typo / unsupported id,
/// `Timeout` → a backend call (notably the `claude` subprocess)
/// exceeded the cap's per-request deadline and the child was killed.
/// `ParamNotSupported` → the request set a knob the backend has no
/// way to honor (e.g. `max_tokens` / `temperature` on the CLI path,
/// which the `claude` binary exposes no flag for — reject rather than
/// silently drop). `AdapterError` is the catchall preserving
/// backend-specific detail as free-form text.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum AnthropicError {
    Overloaded,
    RateLimited {
        retry_after_millis: Option<u32>,
    },
    ContextLengthExceeded {
        limit: u32,
    },
    Unauthorized,
    ContentPolicyRefused,
    CliNotFound,
    UnknownModel {
        model: String,
        supported: Vec<String>,
    },
    Timeout {
        elapsed_millis: u32,
    },
    ParamNotSupported {
        param: String,
        reason: String,
    },
    AdapterError(String),
}

/// `aether.anthropic.messages.send` — request a text completion via
/// the official Anthropic Messages API (HTTPS). Mailed to the
/// `"aether.anthropic"` mailbox; reply lands as
/// `MessagesSendResult`. `request_id` correlates the reply
/// (caller-minted, echoed on both arms). `model` selects the
/// Messages model; `max_tokens` / `temperature` / `system` are the
/// usual completion knobs.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.anthropic.messages.send")]
pub struct MessagesSend {
    pub request_id: u64,
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub system: Option<String>,
}

/// `aether.anthropic.cli.send` — request a text completion via the
/// local `claude` subprocess (the user's subscription rail).
/// Identical input schema to [`MessagesSend`]; the routing choice
/// is the kind name. Reply lands as `CliSendResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.anthropic.cli.send")]
pub struct CliSend {
    pub request_id: u64,
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub system: Option<String>,
}

/// Reply to [`MessagesSend`]. Both arms echo the originating
/// `request_id` for correlation. `Ok` carries the completion text,
/// the model the provider actually served, and `Usage` accounting;
/// `Err` carries an `AnthropicError`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.anthropic.messages.send_result")]
pub enum MessagesSendResult {
    Ok {
        request_id: u64,
        text: String,
        model_used: String,
        usage: Usage,
    },
    Err {
        request_id: u64,
        error: AnthropicError,
    },
}

/// Reply to [`CliSend`]. Same shape as [`MessagesSendResult`]; the
/// CLI backend populates only `Usage.wall_clock_millis` (the subprocess
/// reports no token counts).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.anthropic.cli.send_result")]
pub enum CliSendResult {
    Ok {
        request_id: u64,
        text: String,
        model_used: String,
        usage: Usage,
    },
    Err {
        request_id: u64,
        error: AnthropicError,
    },
}
