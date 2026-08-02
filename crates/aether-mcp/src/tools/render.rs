use super::{
    ComponentCapabilities, DeathReason, EnumVariant, FallbackCapability, HandlerCapability, KindId, MailboxId,
    McpError, Primitive, SchemaType,
};

/// Project a wire [`ComponentCapabilities`] for an MCP tool reply (issue 3006).
///
/// The wire / cache form stays full; only the tool response is trimmed.
/// When `full` is true the clone is unchanged. When false, every doc field
/// (component-level, each handler, and fallback) is reduced to its first
/// non-empty line — rustdoc's summary-line convention.
pub(super) fn project_capabilities(caps: &ComponentCapabilities, full: bool) -> ComponentCapabilities {
    if full {
        return caps.clone();
    }
    ComponentCapabilities {
        handlers: caps
            .handlers
            .iter()
            .map(|h| HandlerCapability {
                id: h.id,
                name: h.name.clone(),
                doc: h.doc.as_deref().map(first_doc_line).map(str::to_owned),
                reply: h.reply,
            })
            .collect(),
        fallback: caps
            .fallback
            .as_ref()
            .map(|f| FallbackCapability { doc: f.doc.as_deref().map(first_doc_line).map(str::to_owned) }),
        doc: caps.doc.as_deref().map(first_doc_line).map(str::to_owned),
        config: caps.config.clone(),
        // ADR-0163 §3: the asset catalog is name/len/sha256 metadata with
        // no rustdoc to summarize, so it passes through both the full and
        // projected views unchanged.
        assets: caps.assets.clone(),
        // ADR-0170: the requires-list is kind name + field name — already the
        // shortest form of what it says, and the whole point of surfacing it
        // is that a caller sees every request, so it never projects down.
        params: caps.params.clone(),
    }
}

/// First non-empty line of a rustdoc string, or the whole string when it is
/// a single line (after stripping leading blank lines).
fn first_doc_line(doc: &str) -> &str {
    for line in doc.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    doc.trim()
}

/// Render a [`SchemaType`] as a one-line human-readable shape string —
/// the compact form `describe_kinds` returns by default. The rendering is
/// intentionally lossy (names only, not discriminants or `repr_c`) and is
/// enough to build `send_mail` params for simple kinds without fetching
/// the full schema. Depth is capped at 6 (`…` past that) per CLAUDE.md's
/// recursion rule; schema depth is structurally bounded by the vocabulary
/// but the cap is cheap insurance against pathological nesting.
pub(super) fn render_shape(ty: &SchemaType) -> String {
    fn render(ty: &SchemaType, depth: u8) -> String {
        if depth > 6 {
            return "\u{2026}".to_owned();
        }
        match ty {
            SchemaType::Unit => "{}".to_owned(),
            SchemaType::Bool => "bool".to_owned(),
            SchemaType::Scalar(p) => match p {
                Primitive::U8 => "u8",
                Primitive::U16 => "u16",
                Primitive::U32 => "u32",
                Primitive::U64 => "u64",
                Primitive::I8 => "i8",
                Primitive::I16 => "i16",
                Primitive::I32 => "i32",
                Primitive::I64 => "i64",
                Primitive::F32 => "f32",
                Primitive::F64 => "f64",
            }
            .to_owned(),
            SchemaType::String => "String".to_owned(),
            SchemaType::Bytes => "Bytes".to_owned(),
            SchemaType::Option(inner) => format!("Option<{}>", render(inner, depth + 1)),
            SchemaType::Vec(inner) => format!("Vec<{}>", render(inner, depth + 1)),
            SchemaType::Array { element, len } => {
                format!("[{}; {}]", render(element, depth + 1), len)
            }
            SchemaType::Struct { fields, .. } => {
                let parts: Vec<String> =
                    fields.iter().map(|f| format!("{}: {}", f.name, render(&f.ty, depth + 1))).collect();
                format!("{{ {} }}", parts.join(", "))
            }
            SchemaType::Enum { variants } => {
                let parts: Vec<String> = variants
                    .iter()
                    .map(|v| match v {
                        EnumVariant::Unit { name, .. } => name.to_string(),
                        EnumVariant::Tuple { name, fields, .. } => {
                            let inner: Vec<String> = fields.iter().map(|f| render(f, depth + 1)).collect();
                            format!("{}({})", name, inner.join(", "))
                        }
                        EnumVariant::Struct { name, fields, .. } => {
                            let inner: Vec<String> =
                                fields.iter().map(|f| format!("{}: {}", f.name, render(&f.ty, depth + 1))).collect();
                            format!("{} {{ {} }}", name, inner.join(", "))
                        }
                    })
                    .collect();
                parts.join(" | ")
            }
            SchemaType::Map { key, value } => {
                format!("Map<{}, {}>", render(key, depth + 1), render(value, depth + 1))
            }
            SchemaType::TypeId(id) => {
                if *id == MailboxId::TYPE_ID {
                    "MailboxId".to_owned()
                } else if *id == KindId::TYPE_ID {
                    "KindId".to_owned()
                } else {
                    format!("TypeId({id:#x})")
                }
            }
        }
    }
    render(ty, 0)
}

/// Serialize a tool result to the JSON string `rmcp` wraps as text
/// content.
pub(super) fn json<T: serde::Serialize>(value: &T) -> Result<String, McpError> {
    serde_json::to_string(value).map_err(|e| McpError::internal_error(e.to_string(), None))
}

/// Flatten a wire [`DeathReason`] into the `(reason, detail)` pair the
/// `list_engines` tool renders: a short tag plus the variant's detail
/// string (empty for the clean `Terminated` case). Flat over a tagged
/// JSON enum so an LLM consumer reads the cause without a nested match.
pub(super) fn death_reason_parts(reason: DeathReason) -> (String, String) {
    match reason {
        DeathReason::Terminated => ("terminated".to_owned(), String::new()),
        DeathReason::Crashed { detail } => ("crashed".to_owned(), detail),
        DeathReason::Evicted { detail } => ("evicted".to_owned(), detail),
        DeathReason::SpawnFailed { detail } => ("spawn_failed".to_owned(), detail),
    }
}

// `e` is owned because callers do `.map_err(internal)` — the closure-
// converted form needs an `FnOnce(anyhow::Error) -> McpError`.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn internal(e: anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

pub(super) fn internal_msg(msg: &str) -> McpError {
    McpError::internal_error(msg.to_owned(), None)
}

/// iamacoffeepot/aether#1271: tools that ship potentially-large
/// payloads through the RPC framing (currently `load_component` /
/// `replace_component`) surface a `FrameTooLarge` / `EncodeTooLarge`
/// failure as `invalid_params` rather than `internal_error`. The
/// payload is a client-controllable input (the user picked the wasm
/// path), and the actionable remediation — build the release wasm,
/// raise `AETHER_MAX_FRAME_SIZE` — is specific to the caller. Falls
/// through to `internal` for every other shape.
///
/// Detection is by substring of the error chain because the structured
/// `RpcError` rides under `anyhow::Error` (the session's `call_once`
/// formats the wire error with `{e:?}` into a string; the encode-side
/// classifier formats `RpcClientError::Frame(...)` with `{e}`). Both
/// shapes embed the literal `frame too large` / `encoded frame too
/// large` strings the codec / RPC error variants produce.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn frame_size_aware_error(context: &str, e: anyhow::Error) -> McpError {
    let text = e.to_string();
    if text.contains("frame too large")
        || text.contains("encoded frame too large")
        || text.contains("FrameTooLarge")
        || text.contains("EncodeTooLarge")
    {
        return McpError::invalid_params(
            format!(
                "{context}: payload exceeds the RPC framing cap — typically because the supplied \
                 wasm is a debug build. Build the release wasm (target/wasm32-unknown-unknown/\
                 release/*.wasm) or raise the cap via the AETHER_MAX_FRAME_SIZE env var. \
                 Underlying: {text}",
            ),
            None,
        );
    }
    McpError::internal_error(text, None)
}
