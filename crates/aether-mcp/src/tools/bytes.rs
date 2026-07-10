use super::{EnumVariant, SchemaType, Uuid};
use aether_data::NamedField;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use std::future::Future;
use std::io;
use std::mem;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::fs;

async fn resolve_named_fields(
    mut map: serde_json::Map<String, serde_json::Value>,
    fields: &[NamedField],
    max_file_bytes: usize,
) -> anyhow::Result<serde_json::Value> {
    for field in fields {
        if let Some(slot) = map.remove(&*field.name) {
            let resolved = resolve_bytes_params(slot, &field.ty, max_file_bytes).await?;
            map.insert(field.name.to_string(), resolved);
        }
    }
    Ok(serde_json::Value::Object(map))
}

fn render_named_fields(
    mut map: serde_json::Map<String, serde_json::Value>,
    fields: &[NamedField],
    inline_max: usize,
) -> serde_json::Value {
    for field in fields {
        if let Some(slot) = map.get_mut(&*field.name) {
            let taken = mem::take(slot);
            *slot = render_bytes_reply(taken, &field.ty, inline_max);
        }
    }
    serde_json::Value::Object(map)
}

/// Blob-embed preprocessor (issue 1944). The wire codec is strict and
/// canonical — a `SchemaType::Bytes` param encodes only from a JSON byte
/// array — so the consumer-facing ergonomics live here, in the MCP front
/// that already owns the JSON params before schema-encoding them. Walk
/// `value` alongside `schema` and, at every `Bytes` node, resolve a
/// `$`-sigil embed object into the canonical byte array `encode_schema`
/// accepts. A literal `[…]` array passes straight through (back-compat);
/// a one-key embed object expands — `{"$file": path}` reads the file on
/// the harness host and inlines its bytes, `{"$base64": s}` decodes,
/// `{"$text": s}` UTF-8-encodes. Any other shape at a `Bytes` node, or an
/// unknown `$`-tag, errors. Recursion depth is bounded by the
/// compile-time kind schema, not by user-controlled runtime data, so it
/// mirrors `encode_schema`'s own recursive walk; only the arms that can
/// carry a `Bytes` leaf (`Struct` / `Option` / `Vec` / `Array` / `Map`)
/// descend, every other value passes through untouched. `max_file_bytes`
/// is the RPC frame cap a `{"$file"}` read is guarded against; the
/// production call sites pass `max_frame_size()`.
#[allow(clippy::too_many_lines)] // one arm per SchemaType variant; splitting the enum arm out would obscure the symmetry with render_bytes_reply
pub(super) fn resolve_bytes_params<'a>(
    value: serde_json::Value,
    schema: &'a SchemaType,
    max_file_bytes: usize,
) -> Pin<Box<dyn Future<Output = anyhow::Result<serde_json::Value>> + Send + 'a>> {
    use serde_json::Value;
    Box::pin(async move {
        match schema {
            SchemaType::Bytes => resolve_bytes_embed(value, max_file_bytes).await,
            SchemaType::Option(inner) => {
                if value.is_null() {
                    Ok(value)
                } else {
                    resolve_bytes_params(value, inner, max_file_bytes).await
                }
            }
            SchemaType::Vec(inner) | SchemaType::Array { element: inner, .. } => match value {
                Value::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        out.push(resolve_bytes_params(item, inner, max_file_bytes).await?);
                    }
                    Ok(Value::Array(out))
                }
                other => Ok(other),
            },
            SchemaType::Struct { fields, .. } => match value {
                Value::Object(map) => resolve_named_fields(map, fields, max_file_bytes).await,
                other => Ok(other),
            },
            SchemaType::Map { value: value_schema, .. } => match value {
                Value::Object(mut map) => {
                    let keys: Vec<String> = map.keys().cloned().collect();
                    for key in keys {
                        if let Some(slot) = map.remove(&key) {
                            let resolved = resolve_bytes_params(slot, value_schema, max_file_bytes).await?;
                            map.insert(key, resolved);
                        }
                    }
                    Ok(Value::Object(map))
                }
                other => Ok(other),
            },
            SchemaType::Enum { variants } => match value {
                // Unit variant — bare string, no payload to descend into.
                Value::String(_) => Ok(value),
                // Non-unit variant — externally-tagged one-key object
                // `{"VariantName": payload}`. Resolve the tag, then recurse
                // into the payload according to the matched variant kind.
                Value::Object(map) if map.len() == 1 => {
                    let (tag, payload) = map.into_iter().next().expect("len == 1");
                    let variant = variants.iter().find(|v| v.name() == tag.as_str());
                    let resolved = match variant {
                        None | Some(EnumVariant::Unit { .. }) => payload,
                        Some(EnumVariant::Tuple { fields, .. }) => match fields.as_ref() {
                            [single] => resolve_bytes_params(payload, single, max_file_bytes).await?,
                            _ => match payload {
                                Value::Array(items) => {
                                    let mut out = Vec::with_capacity(items.len());
                                    for (item, field) in items.into_iter().zip(fields.iter()) {
                                        out.push(resolve_bytes_params(item, field, max_file_bytes).await?);
                                    }
                                    Value::Array(out)
                                }
                                other => other,
                            },
                        },
                        Some(EnumVariant::Struct { fields, .. }) => match payload {
                            Value::Object(inner) => resolve_named_fields(inner, fields, max_file_bytes).await?,
                            other => other,
                        },
                    };
                    let mut out = serde_json::Map::new();
                    out.insert(tag, resolved);
                    Ok(Value::Object(out))
                }
                other => Ok(other),
            },
            // Scalars, String, TypeId, Unit, Bool: no `Bytes` leaf is
            // reachable through the embed grammar, so pass through.
            _ => Ok(value),
        }
    })
}

/// Resolve a single `Bytes`-node value into the canonical JSON byte
/// array (issue 1944). A literal `[…]` array passes through; a one-key
/// `$`-sigil object expands into bytes; anything else errors. `{"$file"}`
/// is guarded above `max_file_bytes` (the RPC frame cap) so a blob too
/// large to ride in mail errors with a pointer to the staged-path
/// mechanism rather than being silently inlined.
pub(super) async fn resolve_bytes_embed(
    value: serde_json::Value,
    max_file_bytes: usize,
) -> anyhow::Result<serde_json::Value> {
    use serde_json::Value;
    let obj = match value {
        // Canonical form — already a byte array. Back-compat passthrough.
        Value::Array(_) => return Ok(value),
        Value::Object(map) => map,
        _ => anyhow::bail!(
            "a Bytes field accepts a byte array or a single $-sigil embed \
             ($file / $base64 / $text)"
        ),
    };
    if obj.len() != 1 {
        anyhow::bail!(
            "a Bytes embed object must have exactly one $-sigil key \
             ($file / $base64 / $text)"
        );
    }
    let (key, body) = obj.into_iter().next().expect("len == 1");
    let bytes: Vec<u8> = match key.as_str() {
        "$file" => {
            let path = body.as_str().ok_or_else(|| anyhow::anyhow!("$file value must be a string path"))?;
            let bytes = fs::read(path).await.map_err(|e| anyhow::anyhow!("$file: reading {path:?}: {e}"))?;
            if bytes.len() > max_file_bytes {
                anyhow::bail!(
                    "$file {path:?} is {} bytes, over the {max_file_bytes}-byte RPC frame cap; a \
                     blob this large must stage as a hub-read path (ADR-0115/0116), not inline \
                     into mail",
                    bytes.len()
                );
            }
            bytes
        }
        "$base64" => {
            let s = body.as_str().ok_or_else(|| anyhow::anyhow!("$base64 value must be a string"))?;
            STANDARD.decode(s).map_err(|e| anyhow::anyhow!("$base64: invalid base64: {e}"))?
        }
        "$text" => {
            let s = body.as_str().ok_or_else(|| anyhow::anyhow!("$text value must be a string"))?;
            s.as_bytes().to_vec()
        }
        other => anyhow::bail!(
            "a Bytes field accepts a byte array or a single $-sigil embed; got {other:?} \
             (expected $file / $base64 / $text)"
        ),
    };
    Ok(Value::Array(bytes.into_iter().map(Value::from).collect()))
}

/// Reply-side mirror of [`resolve_bytes_params`] (issue 1944). The strict
/// decoder emits a `Bytes` field as a JSON byte array; render it back to
/// a bare string when the bytes are valid UTF-8 (the read-back-as-text
/// ergonomic), else to `{"base64": …}`. Walks `schema` to reach a `Bytes`
/// leaf nested in a composite, every other value untouched.
pub(super) fn render_bytes_reply(
    value: serde_json::Value,
    schema: &SchemaType,
    inline_max: usize,
) -> serde_json::Value {
    use serde_json::Value;
    match schema {
        SchemaType::Bytes => render_bytes_leaf(value, inline_max),
        SchemaType::Option(inner) => {
            if value.is_null() {
                value
            } else {
                render_bytes_reply(value, inner, inline_max)
            }
        }
        SchemaType::Vec(inner) | SchemaType::Array { element: inner, .. } => match value {
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|v| render_bytes_reply(v, inner, inline_max)).collect())
            }
            other => other,
        },
        SchemaType::Struct { fields, .. } => match value {
            Value::Object(map) => render_named_fields(map, fields, inline_max),
            other => other,
        },
        SchemaType::Map { value: value_schema, .. } => match value {
            Value::Object(mut map) => {
                for slot in map.values_mut() {
                    let taken = mem::take(slot);
                    *slot = render_bytes_reply(taken, value_schema, inline_max);
                }
                Value::Object(map)
            }
            other => other,
        },
        SchemaType::Enum { variants } => match value {
            // Unit variant — bare string, no payload to descend into.
            Value::String(_) => value,
            // Non-unit variant — externally-tagged one-key object
            // `{"VariantName": payload}`. Resolve the tag, then recurse
            // into the payload according to the matched variant kind.
            Value::Object(map) if map.len() == 1 => {
                let (tag, payload) = map.into_iter().next().expect("len == 1");
                let rendered = match variants.iter().find(|v| v.name() == tag.as_str()) {
                    None | Some(EnumVariant::Unit { .. }) => payload,
                    Some(EnumVariant::Tuple { fields, .. }) => match fields.as_ref() {
                        [single] => render_bytes_reply(payload, single, inline_max),
                        _ => match payload {
                            Value::Array(items) => Value::Array(
                                items
                                    .into_iter()
                                    .zip(fields.iter())
                                    .map(|(v, f)| render_bytes_reply(v, f, inline_max))
                                    .collect(),
                            ),
                            other => other,
                        },
                    },
                    Some(EnumVariant::Struct { fields, .. }) => match payload {
                        Value::Object(inner) => render_named_fields(inner, fields, inline_max),
                        other => other,
                    },
                };
                let mut out = serde_json::Map::new();
                out.insert(tag, rendered);
                Value::Object(out)
            }
            other => other,
        },
        _ => value,
    }
}

/// Resolve the reply-`Bytes` spill threshold (`AETHER_MCP_REPLY_INLINE_MAX_BYTES`,
/// default 256 KiB). A reply `Bytes` leaf larger than this is staged to a
/// harness-host temp file and surfaced as `{"file": …}` instead of inlined as
/// base64 (issue 2108) — the reply-side mirror of the input `{"$file": …}`
/// embed. Deliberately distinct from the RPC frame cap: an over-frame reply
/// never reaches this projection (the inbound frame decode fails first), and
/// 64 MiB is far past the point base64 bloats the tool channel.
pub(super) fn reply_inline_max_bytes() -> usize {
    use std::env;
    const DEFAULT_REPLY_INLINE_MAX_BYTES: usize = 256 * 1024;
    // Process-level tuning knob, not capability config: the out-of-process MCP
    // front has no `Config`-derive plumbing, so it reads this directly the same
    // way `main.rs` reads `AETHER_MCP_PORT`.
    #[allow(clippy::disallowed_methods)]
    let raw = env::var("AETHER_MCP_REPLY_INLINE_MAX_BYTES").ok();
    raw.and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(DEFAULT_REPLY_INLINE_MAX_BYTES)
}

/// Stage `bytes` to a unique harness-host temp file under `dir` and return its
/// absolute path. Mirrors the boot-manifest staging writes (`stage_boot_manifest`):
/// a blocking `std::fs::write` into the temp dir, named `aether-reply-<uuid>.bin`.
pub(super) fn spill_reply_bytes(dir: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    // `std::fs`, not the module's `tokio::fs`: this write is blocking by design
    // (the MCP front handling a tool reply is not latency-critical).
    use std::fs as std_fs;
    let path = dir.join(format!("aether-reply-{}.bin", Uuid::new_v4()));
    std_fs::write(&path, bytes)?;
    Ok(path)
}

/// Render one decoded `Bytes` value — a JSON array of byte numbers — for the
/// caller. The ladder: a value over `inline_max` (regardless of UTF-8 validity)
/// is spilled to a harness-host temp file and rendered as `{"file": …}`; a
/// smaller value renders as a bare string (valid UTF-8) or a `{"base64": …}`
/// object (binary). A value that isn't the array-of-bytes shape is returned
/// untouched.
pub(super) fn render_bytes_leaf(value: serde_json::Value, inline_max: usize) -> serde_json::Value {
    use std::env;
    render_bytes_leaf_in(value, inline_max, &env::temp_dir())
}

/// `render_bytes_leaf` with an explicit spill directory — the seam the unit
/// tests drive so spills land under a scratch dir rather than the real temp dir.
pub(super) fn render_bytes_leaf_in(value: serde_json::Value, inline_max: usize, spill_dir: &Path) -> serde_json::Value {
    use serde_json::Value;
    let Value::Array(items) = &value else {
        return value;
    };
    let mut bytes = Vec::with_capacity(items.len());
    for item in items {
        match item.as_u64().and_then(|n| u8::try_from(n).ok()) {
            Some(b) => bytes.push(b),
            None => return value,
        }
    }
    // Large reply (regardless of UTF-8 validity) → spill to a host temp file and
    // surface a `{"file": …}` reference rather than inlining. On a spill IO error
    // fall through to the in-band rendering below — the projector must never
    // error or drop reply data (consistent with every other arm's pass-through).
    if bytes.len() > inline_max
        && let Ok(path) = spill_reply_bytes(spill_dir, &bytes)
    {
        let mut obj = serde_json::Map::new();
        obj.insert("file".to_owned(), Value::String(path.to_string_lossy().into_owned()));
        return Value::Object(obj);
    }
    str::from_utf8(&bytes).map_or_else(
        |_| {
            let mut obj = serde_json::Map::new();
            obj.insert("base64".to_owned(), Value::String(STANDARD.encode(&bytes)));
            Value::Object(obj)
        },
        |s| Value::String(s.to_owned()),
    )
}
