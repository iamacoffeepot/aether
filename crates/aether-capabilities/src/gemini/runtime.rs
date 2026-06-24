//! The `aether.gemini` runtime half (ADR-0122 identity/runtime split). Compiled
//! only under `feature = "runtime"` (the `mod runtime;` declaration in the
//! parent carries the gate), so a transport-only build of the
//! `GeminiCapability` identity never names these types nor pulls
//! `aether_substrate`. The substrate-typed imports are gated once by this
//! module rather than line-by-line; the `#[actor] impl` reaches the state, ctx
//! types, and reply helpers through the single `use super::runtime::*` glob in
//! the parent.

use super::adapter::{DisabledGeminiAdapter, UreqGeminiAdapter, map_adapter_error};
use super::config::GeminiConfig;
use super::{GeminiError, GroundingMetadata, LyriaGenerateResult, NanobananaGenerateResult};
use crate::fs::{FileAdapter, LocalFileAdapter};
use crate::shared::contentgen::adapter::{AdapterUsage, GeminiAdapter, GeminiResponse};
use crate::shared::contentgen::staging::{gen_root, stage_gen_output};

pub use crate::shared::contentgen::task_queue::TaskQueue;
pub use std::sync::Arc;

use aether_kinds::Usage;

pub use aether_actor::{Manual, OutboundReply};
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
pub use aether_substrate::chassis::error::BootError;

/// `aether.gemini` runtime state (ADR-0050). Owns the resolved adapter +
/// the cap-level rate-limit queue over the ADR-0093 dispatch primitive.
/// Single-threaded post-ADR-0038, so the queue state lives in plain
/// fields. The dispatcher holds this as the cap's state and routes
/// envelopes through the macro-emitted `Dispatch` impl; the addressing
/// identity is the distinct ZST [`GeminiCapability`](super::GeminiCapability).
/// Living in this private module keeps it `pub`-enough to satisfy the
/// `NativeActor::State` interface without exposing it as crate-public API.
pub struct GeminiCapabilityState {
    pub(super) adapter: Arc<dyn GeminiAdapter>,
    pub(super) tasks: TaskQueue,
}

#[cfg(test)]
impl GeminiCapabilityState {
    pub(crate) fn from_parts(adapter: Arc<dyn GeminiAdapter>, max_in_flight: usize) -> Self {
        Self {
            adapter,
            tasks: TaskQueue::new(max_in_flight),
        }
    }

    pub(crate) fn test_in_flight(&self) -> usize {
        self.tasks.in_flight()
    }
}

pub fn build_adapter(config: &GeminiConfig) -> Arc<dyn GeminiAdapter> {
    if config.disabled {
        tracing::info!(
            target: "aether_capabilities::gemini",
            "gemini adapter disabled — every request replies Unauthorized",
        );
        return Arc::new(DisabledGeminiAdapter);
    }
    config.api_key.as_ref().map_or_else(
        || {
            tracing::info!(
                target: "aether_capabilities::gemini",
                "GEMINI_API_KEY unset — every request replies Unauthorized",
            );
            Arc::new(DisabledGeminiAdapter) as Arc<dyn GeminiAdapter>
        },
        |key| {
            tracing::info!(
                target: "aether_capabilities::gemini",
                "gemini adapter configured (nanobanana + lyria)",
            );
            Arc::new(UreqGeminiAdapter::new(key.clone(), config.timeout)) as Arc<dyn GeminiAdapter>
        },
    )
}

/// Read reference-image bytes from the supplied save-namespace
/// paths (tool JSON takes paths, the wire stays bytes —
/// `feedback_no_bytes_in_llm_json`). A read failure aborts the
/// request with an `AdapterError`.
pub fn read_reference_images(paths: &[String]) -> Result<Vec<Vec<u8>>, GeminiError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let root = gen_root();
    let adapter =
        LocalFileAdapter::new(root, true).map_err(|e| GeminiError::AdapterError(e.to_string()))?;
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = adapter
            .read(path)
            .map_err(|e| GeminiError::AdapterError(format!("reference {path}: {e:?}")))?;
        out.push(bytes);
    }
    Ok(out)
}

fn to_usage(u: AdapterUsage) -> Usage {
    Usage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        wall_clock_ms: u.wall_clock_ms,
        cost_micros: u.cost_micros,
    }
}

pub fn nanobanana_reply(
    request_id: u64,
    include_sig: bool,
    result: Result<GeminiResponse, String>,
) -> NanobananaGenerateResult {
    match result {
        Ok(resp) => {
            let model_used = resp.model_used;
            let usage = to_usage(resp.usage);
            // Opt-in / default-off: clear the signature unless the
            // caller asked to retain it for a multi-turn continuation
            // (a signature can run to multiple MB and dominate the
            // reply). Parse stays unconditional; the gate is here.
            let thought_signature = if include_sig {
                resp.thought_signature
            } else {
                None
            };
            let grounding = resp
                .grounding
                .map(|(search_queries, source_urls)| GroundingMetadata {
                    search_queries,
                    source_urls,
                });
            let Some(artifact) = resp.artifacts.into_iter().next() else {
                return NanobananaGenerateResult::Err {
                    request_id,
                    error: GeminiError::AdapterError("adapter returned no image".to_string()),
                };
            };
            match stage_gen_output(&artifact.bytes, &artifact.ext) {
                Ok(output_path) => NanobananaGenerateResult::Ok {
                    request_id,
                    output_path,
                    model_used,
                    usage,
                    thought_signature,
                    grounding,
                },
                Err(e) => NanobananaGenerateResult::Err {
                    request_id,
                    error: GeminiError::AdapterError(format!("stage image: {e:?}")),
                },
            }
        }
        Err(raw) => NanobananaGenerateResult::Err {
            request_id,
            error: map_adapter_error(&raw),
        },
    }
}

pub fn lyria_reply(request_id: u64, result: Result<GeminiResponse, String>) -> LyriaGenerateResult {
    match result {
        Ok(resp) => {
            let mut output_paths = Vec::with_capacity(resp.artifacts.len());
            for artifact in &resp.artifacts {
                match stage_gen_output(&artifact.bytes, &artifact.ext) {
                    Ok(path) => output_paths.push(path),
                    Err(e) => {
                        return LyriaGenerateResult::Err {
                            request_id,
                            error: GeminiError::AdapterError(format!("stage clip: {e:?}")),
                        };
                    }
                }
            }
            LyriaGenerateResult::Ok {
                request_id,
                output_paths,
                model_used: resp.model_used,
                usage: to_usage(resp.usage),
            }
        }
        Err(raw) => LyriaGenerateResult::Err {
            request_id,
            error: map_adapter_error(&raw),
        },
    }
}
