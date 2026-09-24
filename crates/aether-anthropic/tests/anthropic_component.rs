//! End-to-end scenario tests for the ADR-0159 guest `aether.anthropic`
//! component. Each boots a `SubstrateHarness`, loads the crate's own wasm
//! artifact (built separately for `wasm32-unknown-unknown`), composes the
//! `aether.http` / `aether.process` edge caps with **empty allowlists**, and
//! drives a request kind into the component to assert the typed error the edge
//! refusal maps back to.
//!
//! These exercise the wiring the pure-function unit tests can't: the two-handler
//! `send_with_context` / `take_context` state machine, the edge dispatch, and
//! the reply routed back to the original caller. The provider is never actually
//! reached — an empty `aether.http` allowlist denies the Messages fetch and an
//! empty `aether.process` allowlist refuses the `claude` run — so the tests are
//! deterministic and cost-free.
//!
//! Skipped when the component's wasm hasn't been built (CI pre-builds it before
//! `cargo test`); the harness itself is headless here (no wgpu needed).

use std::path::Path;
use std::{env, fs};

use aether_actor::ActorRef;
use aether_anthropic::{
    AnthropicComponent, AnthropicComponentConfig, AnthropicError, CliSend, CliSendResult, Message, MessagesSend,
    MessagesSendResult, Role,
};
use aether_data::Kind;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::require_runtime;
use aether_http::{HttpCapability, HttpConfig};
use aether_kinds::LoadComponent;
use aether_process::{ProcessCapability, ProcessConfig, ProcessParams};

/// User-facing component name passed to `LoadComponent`.
const COMPONENT_NAME: &str = "anthropic";

/// A model in the component's supported table, so a Messages request clears the
/// synchronous `UnknownModel` gate and actually dispatches the fetch.
const SUPPORTED_MODEL: &str = "claude-haiku-4-5-20251001";

/// Boot a headless harness with the component host plus the two edge caps, both
/// deny-by-default (empty allowlists). The process cap's work root is never
/// reached — the run is refused before any spawn — so any path serves.
fn boot() -> SubstrateHarness {
    SubstrateHarness::builder()
        .with_component_host()
        .with_actor_configured::<HttpCapability>((), HttpConfig::default())
        .with_actor_configured::<ProcessCapability>(
            ProcessParams { work_root: env::temp_dir() },
            ProcessConfig::default(),
        )
        .build()
        .expect("boot headless harness with http + process edges")
}

/// Load the component wasm under `COMPONENT_NAME` with the given init-config and
/// return its reference, panicking on failure so the test surfaces the message.
fn load_component(
    harness: &mut SubstrateHarness,
    wasm_path: &Path,
    config: &AnthropicComponentConfig,
) -> ActorRef<AnthropicComponent> {
    let wasm = fs::read(wasm_path).expect("read anthropic component wasm");
    harness
        .load::<AnthropicComponent>(LoadComponent {
            wasm,
            name: Some(COMPONENT_NAME.to_owned()),
            config: config.encode_into_bytes(),
            export: None,
        })
        .unwrap_or_else(|error| panic!("load_component: {error}"))
        .0
}

fn user_turn(text: &str) -> Vec<Message> {
    vec![Message { role: Role::User, content: text.to_owned() }]
}

/// A supported model dispatches a Messages fetch (the key is the http cap's
/// operator binding, ADR-0235, never component config), which the empty
/// `aether.http` allowlist denies; the component maps that back to a typed
/// `AnthropicError` reply rather than hanging or warn-dropping.
#[test]
fn messages_send_maps_http_allowlist_refusal_to_typed_error() {
    let Some(wasm_path) = require_runtime("aether_anthropic") else {
        return;
    };
    let mut harness = boot();
    let config = AnthropicComponentConfig { disabled: false, timeout_millis: 0, cli_binary: "claude".to_owned() };
    let component = load_component(&mut harness, &wasm_path, &config);

    let result = harness
        .execute(vec![(
            "send",
            HarnessOp::send_and_await_reply(
                &component,
                &MessagesSend {
                    request_id: 42,
                    model: SUPPORTED_MODEL.to_owned(),
                    messages: user_turn("hello"),
                    max_tokens: Some(8),
                    temperature: None,
                    system: None,
                },
            ),
        )])
        .expect("messages send + reply");

    match result.reply::<MessagesSendResult>("send").expect("decode MessagesSendResult") {
        MessagesSendResult::Err { request_id, error } => {
            assert_eq!(request_id, 42, "reply echoes the caller-minted request_id");
            // The empty allowlist denies egress; the component surfaces it as an
            // AdapterError (no typed anthropic variant for egress policy).
            let AnthropicError::AdapterError(detail) = &error else {
                panic!("expected AdapterError from the denied fetch, got {error:?}");
            };
            assert!(detail.contains("not permitted"), "detail should name the egress refusal, got {detail:?}");
        }
        MessagesSendResult::Ok { .. } => panic!("a denied fetch must not yield Ok"),
    }
}

/// A `cli.send` with an empty `aether.process` allowlist is refused
/// (`NotPermitted`), which the component folds into the `CliNotFound` skip the
/// kind already models — the graceful "claude backend unavailable" reply.
#[test]
fn cli_send_maps_process_refusal_to_cli_not_found() {
    let Some(wasm_path) = require_runtime("aether_anthropic") else {
        return;
    };
    let mut harness = boot();
    // Default config: `claude` as the binary (the CLI path needs no key).
    let component = load_component(&mut harness, &wasm_path, &AnthropicComponentConfig::default());

    let result = harness
        .execute(vec![(
            "send",
            HarnessOp::send_and_await_reply(
                &component,
                &CliSend {
                    request_id: 7,
                    model: SUPPORTED_MODEL.to_owned(),
                    messages: user_turn("hello"),
                    max_tokens: None,
                    temperature: None,
                    system: None,
                },
            ),
        )])
        .expect("cli send + reply");

    match result.reply::<CliSendResult>("send").expect("decode CliSendResult") {
        CliSendResult::Err { request_id, error } => {
            assert_eq!(request_id, 7, "reply echoes the caller-minted request_id");
            assert_eq!(error, AnthropicError::CliNotFound, "an allowlist refusal maps to CliNotFound");
        }
        CliSendResult::Ok { .. } => panic!("a refused run must not yield Ok"),
    }
}
