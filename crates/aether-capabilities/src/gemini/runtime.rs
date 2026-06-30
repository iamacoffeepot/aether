//! The `aether.gemini` runtime half (ADR-0122 identity/runtime split). Compiled
//! only under `feature = "runtime"` (the `mod runtime;` declaration in the
//! parent carries the gate), so a transport-only build of the
//! `GeminiCapability` identity never names these types nor pulls
//! `aether_substrate`. The substrate-typed imports are gated once by this
//! module rather than line-by-line; the `#[actor] impl` reaches the state, ctx
//! types, and reply helpers through the single `use super::runtime::*` glob in
//! the parent.

use super::adapter::{
    DisabledGeminiAdapter, UreqGeminiAdapter, aspect_ratio_str, image_size_str, map_adapter_error,
    thinking_level_str,
};
use super::config::GeminiConfig;
use super::{
    GeminiCapability, GeminiError, GroundingMetadata, LyriaGenerate, LyriaGenerateResult,
    NanobananaGenerate, NanobananaGenerateResult, lyria, nanobanana,
};
use crate::fs::{Access, FileAdapter, LocalFileAdapter};
use crate::shared::contentgen::adapter::{
    AdapterUsage, GeminiAdapter, GeminiImageRequest, GeminiMusicRequest, GeminiResponse,
};
use crate::shared::contentgen::staging::{gen_root, stage_gen_output};

pub use crate::shared::contentgen::task_queue::TaskQueue;
pub use std::sync::Arc;

use aether_actor::runtime;
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

#[runtime]
impl NativeActor for GeminiCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// state-bearing struct holding the adapter + the rate-limit queue.
    type State = GeminiCapabilityState;

    type Config = GeminiConfig;

    /// ADR-0050 + ADR-0074 Phase 5 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.gemini";

    fn init(
        config: GeminiConfig,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<GeminiCapabilityState, BootError> {
        Ok(GeminiCapabilityState {
            adapter: build_adapter(&config),
            tasks: TaskQueue::new(config.max_in_flight),
        })
    }

    /// Generate an image via Nano Banana off the dispatcher thread.
    ///
    /// # Agent
    /// Reply: `NanobananaGenerateResult` carrying a staged
    /// `save://gen/<uuid>.png` path. Validates the model and the
    /// per-model `aspect_ratio` / `image_size` / reference-count
    /// rules synchronously (the matching `…NotSupportedByModel` /
    /// `UnknownModel` error on a miss) before any dispatch.
    #[handler::manual]
    fn on_nanobanana_generate(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Manual>,
        mail: NanobananaGenerate,
    ) {
        let request_id = mail.request_id;
        // Opt-in / default-off; cross-model, so never validated.
        let include_sig = mail.include_thought_signature.unwrap_or(false);
        let Some(shape) = nanobanana::lookup_model(&mail.model) else {
            OutboundReply::reply(
                ctx,
                &NanobananaGenerateResult::Err {
                    request_id,
                    error: GeminiError::UnknownModel {
                        model: mail.model,
                        supported: nanobanana::supported_model_ids(),
                    },
                },
            );
            return;
        };
        let inputs = nanobanana::ValidationInputs {
            aspect_ratio: mail.aspect_ratio,
            image_size: mail.image_size,
            thinking_level_set: mail.thinking_level.is_some(),
            include_thoughts_set: mail.include_thoughts.is_some(),
            use_grounding_set: mail.use_grounding.is_some(),
            object_ref_count: mail.object_reference_paths.len(),
            character_ref_count: mail.character_reference_paths.len(),
        };
        if let Err(error) = nanobanana::validate(shape, &inputs) {
            OutboundReply::reply(ctx, &NanobananaGenerateResult::Err { request_id, error });
            return;
        }

        // Read reference bytes on the dispatcher thread (small,
        // local) before handing the network call off-thread.
        let mut ref_paths = mail.object_reference_paths;
        ref_paths.extend(mail.character_reference_paths);
        let reference_images = match read_reference_images(&ref_paths) {
            Ok(b) => b,
            Err(error) => {
                OutboundReply::reply(ctx, &NanobananaGenerateResult::Err { request_id, error });
                return;
            }
        };

        let req = GeminiImageRequest {
            model: mail.model,
            prompt: mail.prompt,
            aspect_ratio: aspect_ratio_str(mail.aspect_ratio).to_string(),
            image_size: mail.image_size.map(|s| image_size_str(s).to_string()),
            thinking_level: mail
                .thinking_level
                .map(|l| thinking_level_str(l).to_string()),
            include_thoughts: mail.include_thoughts,
            use_grounding: mail.use_grounding.unwrap_or(false),
            reference_images,
        };
        let adapter = Arc::clone(&state.adapter);
        state.tasks.submit(ctx, move || {
            let result = adapter.nanobanana_generate(req);
            // Staging runs here on the worker (blocking disk I/O), so
            // a megabyte PNG never rides the mail wire — the reply
            // carries the staged path.
            nanobanana_reply(request_id, include_sig, result)
        });
    }

    /// Generate music via Lyria off the dispatcher thread.
    ///
    /// # Agent
    /// Reply: `LyriaGenerateResult` carrying one staged
    /// `save://gen/<uuid>.wav` path per clip. Rejects an unknown
    /// model and a both-set `seed` + `sample_count` synchronously
    /// before any dispatch.
    #[handler::manual]
    fn on_lyria_generate(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Manual>,
        mail: LyriaGenerate,
    ) {
        let request_id = mail.request_id;
        if !lyria::is_supported(&mail.model) {
            OutboundReply::reply(
                ctx,
                &LyriaGenerateResult::Err {
                    request_id,
                    error: GeminiError::UnknownModel {
                        model: mail.model,
                        supported: lyria::supported_model_ids(),
                    },
                },
            );
            return;
        }
        if let Err(error) = lyria::validate(
            &mail.model,
            mail.seed.is_some(),
            mail.sample_count.is_some(),
        ) {
            OutboundReply::reply(ctx, &LyriaGenerateResult::Err { request_id, error });
            return;
        }

        let req = GeminiMusicRequest {
            model: mail.model,
            prompt: mail.prompt,
            sample_count: mail.sample_count.unwrap_or(1),
        };
        let adapter = Arc::clone(&state.adapter);
        state.tasks.submit(ctx, move || {
            let result = adapter.lyria_generate(req);
            // Staging (one path per clip) runs here on the worker.
            lyria_reply(request_id, result)
        });
    }

    /// ADR-0093 completion for a finished Nano Banana call: re-reply
    /// the worker's staged result to the original caller (drops the
    /// hold), then free the in-flight slot (draining the next pending
    /// request).
    #[handler(task)]
    fn on_nanobanana_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<NanobananaGenerateResult>,
    ) {
        done.resolve(ctx);
        state.tasks.on_complete(ctx);
    }

    /// ADR-0093 completion for a finished Lyria call.
    #[handler(task)]
    fn on_lyria_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<LyriaGenerateResult>,
    ) {
        done.resolve(ctx);
        state.tasks.on_complete(ctx);
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
    let adapter = LocalFileAdapter::new(root, Access::ReadWrite)
        .map_err(|e| GeminiError::AdapterError(e.to_string()))?;
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
        wall_clock_millis: u.wall_clock_millis,
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

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use crate::gemini::DisabledGeminiAdapter;
    use crate::gemini::GeminiCapability;
    use crate::gemini::runtime::{GeminiCapabilityState, nanobanana_reply};
    use crate::gemini::{
        AspectRatio, GeminiError, ImageSize, LyriaGenerate, LyriaGenerateResult,
        NanobananaGenerate, NanobananaGenerateResult,
    };
    use crate::shared::contentgen::adapter::STUB_PNG;
    use crate::shared::contentgen::adapter::StubGeminiAdapter;
    use crate::shared::contentgen::adapter::{AdapterUsage, GeminiArtifact, GeminiResponse};
    use crate::test_chassis::{decode_session_reply, drive_task_completion, test_mailer_and_rx};
    use aether_data::{Kind, MailboxId, SessionToken, Source, SourceAddr, Uuid};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::ctx::NativeCtx;
    use aether_substrate::mail::outbound::EgressEvent;
    use serde::de::DeserializeOwned;
    use std::sync::mpsc::Receiver;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, fs, process};

    fn session_sender() -> Source {
        Source::to(SourceAddr::Session(SessionToken(Uuid::nil())))
    }

    /// Thin alias over the shared `decode_session_reply`.
    fn decode_reply<K: Kind + DeserializeOwned>(rx: &Receiver<EgressEvent>) -> K {
        decode_session_reply(rx)
    }

    fn nb_request(model: &str, aspect_ratio: AspectRatio) -> NanobananaGenerate {
        NanobananaGenerate {
            request_id: 1,
            model: model.to_string(),
            prompt: "a cat".to_string(),
            aspect_ratio,
            image_size: None,
            thinking_level: None,
            include_thoughts: None,
            object_reference_paths: Vec::new(),
            character_reference_paths: Vec::new(),
            use_grounding: None,
            include_thought_signature: None,
        }
    }

    /// End-to-end through the ADR-0093 dispatch primitive: the stub
    /// Nano Banana adapter runs on the real worker thread, stages a PNG
    /// under a scratch `AETHER_GEN_DIR`, and the cap's
    /// `#[handler(task)]` completion re-replies the `Ok` result —
    /// carrying a staged `gen/<uuid>.png` path that exists on disk — to
    /// the original caller.
    #[test]
    fn gemini_stub_nanobanana() {
        let _guard = GEN_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let scratch = env::temp_dir().join(format!(
            "aether-gemini-nb-{}-{}",
            process::id(),
            request_nonce()
        ));
        fs::create_dir_all(&scratch).expect("scratch dir creates");
        // SAFETY: serialized by GEN_DIR_ENV_LOCK against the other
        // gen-dir tests; the real worker stages here.
        unsafe {
            env::set_var("AETHER_GEN_DIR", &scratch);
        }

        let (mailer, rx) = test_mailer_and_rx();
        let cap_mailbox = MailboxId(0);
        let mut state = GeminiCapabilityState::from_parts(Arc::new(StubGeminiAdapter), 4);
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            cap_mailbox,
        ));
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            session_sender(),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
        GeminiCapability::on_nanobanana_generate(
            &mut state,
            &mut ctx,
            nb_request(
                "gemini-3.1-flash-image-preview",
                AspectRatio::ASPECT_RATIO_1_1,
            ),
        );
        // The worker runs the stub call + staging and pushes the
        // completion wake; route it through the cap's task handler.
        drive_task_completion::<GeminiCapability>(&mut state, &transport, &rx);

        match decode_reply::<NanobananaGenerateResult>(&rx) {
            NanobananaGenerateResult::Ok { output_path, .. } => {
                assert!(
                    output_path.starts_with("gen/"),
                    "staged path was {output_path:?}"
                );
                assert_eq!(output_path.rsplit('.').next(), Some("png"));
                let bytes =
                    fs::read(scratch.join(&output_path)).expect("staged file exists on disk");
                assert_eq!(&bytes[..8], &STUB_PNG[..8]);
            }
            other @ NanobananaGenerateResult::Err { .. } => {
                panic!("expected Ok, got {other:?}")
            }
        }

        // SAFETY: same lock-guarded window as the set above.
        unsafe {
            env::remove_var("AETHER_GEN_DIR");
        }
        let _ = fs::remove_dir_all(&scratch);
    }

    /// Per-model validation: an unsupported aspect ratio / image
    /// size / over-count reference combo errors before any dispatch.
    #[test]
    fn gemini_nanobanana_per_model_validation() {
        let (mailer, rx) = test_mailer_and_rx();
        let cap_mailbox = MailboxId(0);
        let mut state = GeminiCapabilityState::from_parts(Arc::new(StubGeminiAdapter), 4);
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            cap_mailbox,
        ));
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            session_sender(),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
        // NB1 + the NB2-only extreme aspect ratio -> rejected.
        GeminiCapability::on_nanobanana_generate(
            &mut state,
            &mut ctx,
            nb_request("gemini-2.5-flash-image", AspectRatio::ASPECT_RATIO_8_1),
        );
        match decode_reply::<NanobananaGenerateResult>(&rx) {
            NanobananaGenerateResult::Err {
                error: GeminiError::AspectRatioNotSupportedByModel { .. },
                ..
            } => {}
            other => panic!("expected AspectRatioNotSupportedByModel, got {other:?}"),
        }
        // No dispatch happened — the synchronous validation error
        // never spawned work.
        assert_eq!(state.test_in_flight(), 0);

        // NB1 + an unsupported image size -> rejected.
        let mut req = nb_request("gemini-2.5-flash-image", AspectRatio::ASPECT_RATIO_1_1);
        req.image_size = Some(ImageSize::S512);
        GeminiCapability::on_nanobanana_generate(&mut state, &mut ctx, req);
        match decode_reply::<NanobananaGenerateResult>(&rx) {
            NanobananaGenerateResult::Err {
                error: GeminiError::ImageSizeNotSupportedByModel { .. },
                ..
            } => {}
            other => panic!("expected ImageSizeNotSupportedByModel, got {other:?}"),
        }
    }

    #[test]
    fn gemini_unknown_model_errors() {
        let (mailer, rx) = test_mailer_and_rx();
        let cap_mailbox = MailboxId(0);
        let mut state = GeminiCapabilityState::from_parts(Arc::new(StubGeminiAdapter), 4);
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            cap_mailbox,
        ));
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            session_sender(),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
        GeminiCapability::on_nanobanana_generate(
            &mut state,
            &mut ctx,
            nb_request("gemini-bogus", AspectRatio::ASPECT_RATIO_1_1),
        );
        match decode_reply::<NanobananaGenerateResult>(&rx) {
            NanobananaGenerateResult::Err {
                error: GeminiError::UnknownModel { model, supported },
                ..
            } => {
                assert_eq!(model, "gemini-bogus");
                assert!(supported.contains(&"gemini-3.1-flash-image-preview".to_string()));
            }
            other => panic!("expected UnknownModel, got {other:?}"),
        }
    }

    /// Lyria stub runs on the real worker, stages WAV clips under a
    /// scratch `AETHER_GEN_DIR`, and the `#[handler(task)]` completion
    /// re-replies an `Ok` carrying one staged path per clip.
    #[test]
    fn gemini_stub_lyria() {
        let _guard = GEN_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let scratch = env::temp_dir().join(format!(
            "aether-gemini-lyria-{}-{}",
            process::id(),
            request_nonce()
        ));
        fs::create_dir_all(&scratch).expect("scratch dir creates");
        // SAFETY: serialized by GEN_DIR_ENV_LOCK against the other
        // gen-dir tests; the real worker stages here.
        unsafe {
            env::set_var("AETHER_GEN_DIR", &scratch);
        }

        let (mailer, rx) = test_mailer_and_rx();
        let cap_mailbox = MailboxId(0);
        let mut state = GeminiCapabilityState::from_parts(Arc::new(StubGeminiAdapter), 4);
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            cap_mailbox,
        ));
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            session_sender(),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
        GeminiCapability::on_lyria_generate(
            &mut state,
            &mut ctx,
            LyriaGenerate {
                request_id: 2,
                model: "lyria-3".to_string(),
                prompt: "ambient".to_string(),
                negative_prompt: None,
                seed: None,
                sample_count: Some(2),
            },
        );
        // The worker runs the stub call + per-clip staging and pushes
        // the completion wake; route it through the cap's task handler.
        drive_task_completion::<GeminiCapability>(&mut state, &transport, &rx);
        match decode_reply::<LyriaGenerateResult>(&rx) {
            LyriaGenerateResult::Ok { output_paths, .. } => {
                assert_eq!(output_paths.len(), 2);
                assert!(
                    output_paths
                        .iter()
                        .all(|p| p.starts_with("gen/") && p.rsplit('.').next() == Some("wav"))
                );
            }
            other @ LyriaGenerateResult::Err { .. } => panic!("expected Ok, got {other:?}"),
        }

        // SAFETY: same lock-guarded window as the set above.
        unsafe {
            env::remove_var("AETHER_GEN_DIR");
        }
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn gemini_disabled_replies_unauthorized() {
        let (mailer, rx) = test_mailer_and_rx();
        let cap_mailbox = MailboxId(0);
        let mut state = GeminiCapabilityState::from_parts(Arc::new(DisabledGeminiAdapter), 4);
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            cap_mailbox,
        ));
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            session_sender(),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
        GeminiCapability::on_nanobanana_generate(
            &mut state,
            &mut ctx,
            nb_request(
                "gemini-3.1-flash-image-preview",
                AspectRatio::ASPECT_RATIO_1_1,
            ),
        );
        // The disabled adapter returns the Unauthorized sentinel on the
        // worker; the completion re-replies the mapped error.
        drive_task_completion::<GeminiCapability>(&mut state, &transport, &rx);
        match decode_reply::<NanobananaGenerateResult>(&rx) {
            NanobananaGenerateResult::Err {
                error: GeminiError::Unauthorized,
                ..
            } => {}
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    fn request_nonce() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
            #[allow(clippy::cast_possible_truncation)]
            let n = d.as_nanos() as u64;
            n
        })
    }

    /// Serializes the two seam tests that pin `AETHER_GEN_DIR` so
    /// their process-env mutation can't race nextest's other threads.
    static GEN_DIR_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Build a single-artifact `GeminiResponse` whose parse carried a
    /// `thought_signature`, the shape the cap's reply assembly sees.
    fn nb_response_with_signature(sig: &str) -> GeminiResponse {
        GeminiResponse {
            artifacts: vec![GeminiArtifact {
                bytes: STUB_PNG.to_vec(),
                ext: "png".to_string(),
            }],
            model_used: "gemini-3-pro-image-preview".to_string(),
            usage: AdapterUsage::default(),
            thought_signature: Some(sig.to_string()),
            grounding: None,
        }
    }

    /// Stage `nanobanana_reply` under a scratch `AETHER_GEN_DIR` so
    /// the seam tests never touch the user's real save dir. Holds the
    /// env lock across the set/reply/clear window.
    fn reply_under_scratch_gen_dir(
        include_sig: bool,
        resp: GeminiResponse,
    ) -> NanobananaGenerateResult {
        let _guard = GEN_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let scratch = env::temp_dir().join(format!(
            "aether-gemini-sig-{}-{}",
            process::id(),
            request_nonce()
        ));
        fs::create_dir_all(&scratch).expect("scratch dir creates");
        // SAFETY: serialized by GEN_DIR_ENV_LOCK against the other
        // seam test; no other test reads this var.
        unsafe {
            env::set_var("AETHER_GEN_DIR", &scratch);
        }
        let reply = nanobanana_reply(1, include_sig, Ok(resp));
        // SAFETY: same lock-guarded window as the set above.
        unsafe {
            env::remove_var("AETHER_GEN_DIR");
        }
        let _ = fs::remove_dir_all(&scratch);
        reply
    }

    /// Default-off: a response carrying a `thought_signature` is
    /// cleared from the reply when the flag is unset/false — the
    /// fix for the multi-MB signature dominating the result.
    #[test]
    fn thought_signature_cleared_when_flag_off() {
        let reply = reply_under_scratch_gen_dir(false, nb_response_with_signature("sig-abc"));
        match reply {
            NanobananaGenerateResult::Ok {
                thought_signature, ..
            } => {
                assert_eq!(
                    thought_signature, None,
                    "flag off clears the signature from the reply"
                );
            }
            other @ NanobananaGenerateResult::Err { .. } => {
                panic!("expected Ok, got {other:?}")
            }
        }
    }

    /// Opt-in: the multi-turn continuation path is unaffected — with
    /// the flag true the signature is retained exactly as parsed.
    #[test]
    fn thought_signature_retained_when_flag_on() {
        let reply = reply_under_scratch_gen_dir(true, nb_response_with_signature("sig-abc"));
        match reply {
            NanobananaGenerateResult::Ok {
                thought_signature, ..
            } => {
                assert_eq!(
                    thought_signature.as_deref(),
                    Some("sig-abc"),
                    "flag on retains the signature for a multi-turn continuation"
                );
            }
            other @ NanobananaGenerateResult::Err { .. } => {
                panic!("expected Ok, got {other:?}")
            }
        }
    }

    /// The flag is cross-model, not an NB2-only knob: Pro accepts
    /// `include_thought_signature: Some(true)` and dispatches rather
    /// than rejecting with `MissingRequiredField`. Mirror of
    /// `nb2_only_knob_rejected_on_older_model`, asserting acceptance.
    #[test]
    fn thought_signature_flag_accepted_on_pro() {
        // Acceptance dispatches off-thread, so the reply lands
        // asynchronously — peeking at the reply channel here would race
        // that dispatch (iamacoffeepot/aether#1296). The deterministic
        // proof of acceptance is the in-flight count, which `submit`
        // bumps synchronously on this thread; a synchronous validation
        // error `return`s before dispatch, leaving it at 0. So we don't
        // need the reply channel at all.
        let (mailer, _rx) = test_mailer_and_rx();
        let cap_mailbox = MailboxId(0);
        let mut state = GeminiCapabilityState::from_parts(Arc::new(StubGeminiAdapter), 4);
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            cap_mailbox,
        ));
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            session_sender(),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
        let mut req = nb_request("gemini-3-pro-image-preview", AspectRatio::ASPECT_RATIO_1_1);
        req.image_size = Some(ImageSize::K1);
        req.include_thought_signature = Some(true);
        GeminiCapability::on_nanobanana_generate(&mut state, &mut ctx, req);
        assert_eq!(
            state.test_in_flight(),
            1,
            "Pro must accept the cross-model signature flag and dispatch \
             it rather than rejecting synchronously"
        );
    }
}
