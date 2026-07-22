//! End-to-end scenario for the ADR-0159 `aether.gemini` guest component. Each
//! test boots a `SubstrateHarness`, loads `aether-gemini`'s wasm artifact
//! (built separately for `wasm32-unknown-unknown`), and drives a generate
//! request through the full mail round-trip into a deterministic refusal path
//! — no network, no API key.
//!
//! Two refusals are covered:
//!
//! - **Empty HTTP allowlist** — the component is configured with a (dummy) API
//!   key, so it builds a real `aether.http.fetch`; the egress cap's
//!   deny-by-default allowlist refuses the provider host, and the
//!   `FetchResult::Err { AllowlistDenied }` surfaces back as a
//!   `NanobananaGenerateResult::Err { AdapterError }`. This exercises the whole
//!   `on_nanobanana_generate → fetch → on_fetch_result → reply` state machine.
//! - **Disabled config** — an empty API key short-circuits to
//!   `Err { Unauthorized }` before any fetch, mirroring the native cap's
//!   key-absent boot.
//!
//! Skipped when the component wasm hasn't been built (both
//! `target/wasm32-unknown-unknown/{debug,release}/aether_gemini.wasm` absent);
//! CI builds it before invoking `cargo test`.

use aether_data::Kind;
use aether_gemini::{
    AspectRatio, GeminiComponentConfig, GeminiError, LyriaGenerate, LyriaGenerateResult, NanobananaGenerate,
    NanobananaGenerateResult,
};
use aether_harness_substrate::test_helpers::{init_save_sandbox, require_wasm, test_namespace_roots};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_http::{HttpCapability, HttpConfig};
use aether_kinds::{LoadComponent, LoadResult};
use std::fs;

// Force linkage of `aether-gemini`'s `inventory::submit!` `KindDescriptor`
// entries into this test binary — the linker strips submits for kinds the
// test code doesn't statically reference.
#[allow(unused_imports)]
use aether_gemini as _;

/// User-facing name passed to `LoadComponent`.
const COMPONENT_NAME: &str = "gem";

/// Full lineage address the loaded component registers at (ADR-0099) — what
/// `LoadResult.name` returns and where peers mail it.
fn component_address() -> String {
    use aether_actor::Addressable;
    format!("aether.component/{}:{}", aether_component::WasmTrampoline::NAMESPACE, COMPONENT_NAME)
}

/// Boot a harness with the component host, the native `aether.http` egress cap
/// under an empty (deny-by-default) allowlist, and a `save` sandbox, then load
/// the gemini component with `config`.
fn boot_and_load(config: &GeminiComponentConfig) -> SubstrateHarness {
    let sandbox = init_save_sandbox("gemini-component");
    let mut harness = SubstrateHarness::builder()
        .with_component_host()
        .with_actor_configured::<HttpCapability>((), HttpConfig::default())
        .namespace_roots(test_namespace_roots(sandbox))
        .build()
        .expect("boot");

    let wasm = fs::read(require_wasm("aether_gemini").expect("checked by caller")).expect("read gemini wasm");
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(COMPONENT_NAME.to_owned()),
                    config: config.encode_into_bytes(),
                    export: None,
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
    harness
}

fn nanobanana_request() -> NanobananaGenerate {
    NanobananaGenerate {
        request_id: 7,
        model: "gemini-3.1-flash-image-preview".to_owned(),
        prompt: "a cat".to_owned(),
        aspect_ratio: AspectRatio::ASPECT_RATIO_1_1,
        image_size: None,
        thinking_level: None,
        include_thoughts: None,
        object_reference_paths: Vec::new(),
        character_reference_paths: Vec::new(),
        use_grounding: None,
        include_thought_signature: None,
    }
}

/// The full round-trip: a keyed component builds a real fetch, the empty
/// allowlist refuses it, and the typed `AdapterError` surfaces back through the
/// component's reply — echoing the request id.
#[test]
fn allowlist_refusal_surfaces_typed_error() {
    if require_wasm("aether_gemini").is_none() {
        return;
    }
    let config = GeminiComponentConfig { api_key: "test-key".to_owned(), timeout_millis: 2_000, ..Default::default() };
    let mut harness = boot_and_load(&config);

    let result = harness
        .execute(vec![("gen", HarnessOp::send_and_await(component_address(), &nanobanana_request()))])
        .expect("generate + reply");

    match result.reply::<NanobananaGenerateResult>("gen").expect("decode NanobananaGenerateResult") {
        NanobananaGenerateResult::Err { request_id, error } => {
            assert_eq!(request_id, 7, "reply echoes the request id");
            assert!(
                matches!(error, GeminiError::AdapterError(_)),
                "an empty allowlist denies the fetch and surfaces AdapterError, got {error:?}",
            );
        }
        NanobananaGenerateResult::Ok { .. } => panic!("no key and no network — expected an error reply"),
    }
}

/// An empty API key disables the provider: the request short-circuits to
/// `Unauthorized` before any fetch (so the empty allowlist is never reached).
#[test]
fn disabled_config_replies_unauthorized() {
    if require_wasm("aether_gemini").is_none() {
        return;
    }
    // Default config carries an empty api_key → disabled.
    let mut harness = boot_and_load(&GeminiComponentConfig::default());

    let result = harness
        .execute(vec![("gen", HarnessOp::send_and_await(component_address(), &nanobanana_request()))])
        .expect("generate + reply");

    match result.reply::<NanobananaGenerateResult>("gen").expect("decode NanobananaGenerateResult") {
        NanobananaGenerateResult::Err { request_id, error } => {
            assert_eq!(request_id, 7);
            assert_eq!(error, GeminiError::Unauthorized, "empty key replies Unauthorized without a fetch");
        }
        NanobananaGenerateResult::Ok { .. } => panic!("disabled provider must not succeed"),
    }
}

/// Synchronous validation still runs through the loaded component: an unknown
/// Lyria model is rejected before any dispatch, echoing the request id.
#[test]
fn unknown_lyria_model_rejected() {
    if require_wasm("aether_gemini").is_none() {
        return;
    }
    let config = GeminiComponentConfig { api_key: "test-key".to_owned(), ..Default::default() };
    let mut harness = boot_and_load(&config);

    let request = LyriaGenerate {
        request_id: 11,
        model: "lyria-bogus".to_owned(),
        prompt: "ambient".to_owned(),
        negative_prompt: None,
        seed: None,
        sample_count: None,
    };
    let result = harness
        .execute(vec![("gen", HarnessOp::send_and_await(component_address(), &request))])
        .expect("generate + reply");

    match result.reply::<LyriaGenerateResult>("gen").expect("decode LyriaGenerateResult") {
        LyriaGenerateResult::Err { request_id, error: GeminiError::UnknownModel { model, .. } } => {
            assert_eq!(request_id, 11);
            assert_eq!(model, "lyria-bogus");
        }
        other => panic!("expected UnknownModel, got {other:?}"),
    }
}
