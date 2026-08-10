#[allow(clippy::wildcard_imports)]
use super::super::*;

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;

use rmcp::model::{Content, RawContent};
use tokio::time::sleep;

use crate::args::{
    CollectFailureEvidenceArgs, DeadEngineInfo, EngineInfo, FailureEvidenceFrameArgs, ListEnginesResponse,
};

const ENGINE_ID: &str = "abcdefab-cdef-4abc-8def-abcdefabcdef";
const UPPERCASE_ENGINE_ID: &str = "ABCDEFAB-CDEF-4ABC-8DEF-ABCDEFABCDEF";

struct FakeReply {
    delay: Duration,
    result: Result<FailureEvidenceValue, String>,
}

#[derive(Default)]
struct FakeSource {
    calls: Vec<FailureEvidenceQuery>,
    replies: VecDeque<FakeReply>,
}

impl FakeSource {
    fn with_replies(replies: impl IntoIterator<Item = FakeReply>) -> Self {
        Self { calls: Vec::new(), replies: replies.into_iter().collect() }
    }
}

impl FailureEvidenceSource for FakeSource {
    fn observe(
        &mut self,
        query: FailureEvidenceQuery,
    ) -> Pin<Box<dyn Future<Output = Result<FailureEvidenceValue, String>> + Send + '_>> {
        self.calls.push(query);
        let reply = self.replies.pop_front().expect("one fake reply per observation");
        Box::pin(async move {
            sleep(reply.delay).await;
            reply.result
        })
    }
}

fn json_reply(value: serde_json::Value) -> FakeReply {
    FakeReply { delay: Duration::ZERO, result: Ok(FailureEvidenceValue::Json(value)) }
}

fn delayed_json_reply(delay: Duration, value: serde_json::Value) -> FakeReply {
    FakeReply { delay, result: Ok(FailureEvidenceValue::Json(value)) }
}

fn args() -> CollectFailureEvidenceArgs {
    CollectFailureEvidenceArgs {
        engine_id: ENGINE_ID.into(),
        primary_error: "original send failed".into(),
        operation: Some("send_mail".into()),
        actors: Vec::new(),
        components: Vec::new(),
        kinds: Vec::new(),
        frame: None,
    }
}

fn result_json(result: &CallToolResult) -> serde_json::Value {
    let RawContent::Text(text) = &result.content[0].raw else {
        panic!("the first failure-evidence block must be text");
    };
    serde_json::from_str(&text.text).expect("inline failure-evidence JSON")
}

async fn assert_invalid_without_observations(request: CollectFailureEvidenceArgs, expected: &str) {
    let mut source = FakeSource::default();
    let error = collect_failure_evidence_with_source(
        request,
        &mut source,
        Duration::from_millis(10),
        Duration::from_millis(50),
    )
    .await
    .expect_err("invalid failure-evidence arguments must fail");
    assert!(error.message.contains(expected), "expected error containing {expected:?}, got {:?}", error.message);
    assert!(source.calls.is_empty(), "validation must finish before observation");
}

#[tokio::test]
async fn invalid_and_oversized_requests_make_no_observations() {
    let mut request = args();
    request.engine_id = "not-a-uuid".into();
    assert_invalid_without_observations(request, "valid UUID").await;

    let mut request = args();
    request.primary_error = "   ".into();
    assert_invalid_without_observations(request, "primary_error must not be empty").await;

    let mut request = args();
    request.primary_error = "x".repeat(MAX_PRIMARY_ERROR_BYTES + 1);
    assert_invalid_without_observations(request, "primary_error exceeds").await;

    let mut request = args();
    request.operation = Some("  ".into());
    assert_invalid_without_observations(request, "operation must not be empty").await;

    let mut request = args();
    request.operation = Some("x".repeat(MAX_OPERATION_BYTES + 1));
    assert_invalid_without_observations(request, "operation exceeds").await;

    for (field, count) in [
        ("actors", MAX_FAILURE_EVIDENCE_ACTORS),
        ("components", MAX_FAILURE_EVIDENCE_COMPONENTS),
        ("kinds", MAX_FAILURE_EVIDENCE_KINDS),
    ] {
        let values = (0..=count).map(|index| format!("selector-{index}")).collect();
        let mut request = args();
        match field {
            "actors" => request.actors = values,
            "components" => request.components = values,
            "kinds" => request.kinds = values,
            _ => unreachable!(),
        }
        assert_invalid_without_observations(request, &format!("at most {count} {field}")).await;
    }

    for field in ["actors", "components", "kinds"] {
        let mut request = args();
        match field {
            "actors" => request.actors = vec![" ".into()],
            "components" => request.components = vec![" ".into()],
            "kinds" => request.kinds = vec![" ".into()],
            _ => unreachable!(),
        }
        assert_invalid_without_observations(request, &format!("{field}[0] must not be empty")).await;
    }

    for (field, max_bytes) in
        [("actors", MAX_ADDRESS_BYTES), ("components", MAX_ADDRESS_BYTES), ("kinds", MAX_KIND_NAME_BYTES)]
    {
        let mut request = args();
        let values = vec!["x".repeat(max_bytes + 1)];
        match field {
            "actors" => request.actors = values,
            "components" => request.components = values,
            "kinds" => request.kinds = values,
            _ => unreachable!(),
        }
        assert_invalid_without_observations(request, &format!("{field}[0] exceeds")).await;
    }
}

#[tokio::test]
async fn invalid_frame_boundaries_make_no_observations() {
    let mut invalid_window = args();
    invalid_window.frame =
        Some(FailureEvidenceFrameArgs { window_id: "not-a-window".into(), scale: None, max_dimension: None });
    assert_invalid_without_observations(invalid_window, "window_id").await;

    for scale in [0.0, -0.1, 1.1, f32::INFINITY, f32::NAN] {
        let mut request = args();
        request.frame =
            Some(FailureEvidenceFrameArgs { window_id: "17".into(), scale: Some(scale), max_dimension: None });
        assert_invalid_without_observations(request, "scale must be finite and in (0.0, 1.0]").await;
    }

    let mut zero_dimension = args();
    zero_dimension.frame =
        Some(FailureEvidenceFrameArgs { window_id: "17".into(), scale: None, max_dimension: Some(0) });
    assert_invalid_without_observations(zero_dimension, "max_dimension must be greater than zero").await;
}

#[test]
fn exact_string_and_frame_boundaries_are_accepted() {
    let mut request = args();
    request.primary_error = "e".repeat(MAX_PRIMARY_ERROR_BYTES);
    request.operation = Some("o".repeat(MAX_OPERATION_BYTES));
    request.actors = vec!["a".repeat(MAX_ADDRESS_BYTES)];
    request.components = vec!["c".repeat(MAX_ADDRESS_BYTES)];
    request.kinds = vec!["k".repeat(MAX_KIND_NAME_BYTES)];
    request.frame = Some(FailureEvidenceFrameArgs {
        window_id: "17".into(),
        scale: Some(f32::MIN_POSITIVE),
        max_dimension: Some(1),
    });
    validate_failure_evidence_args(&mut request).expect("documented byte and frame boundaries are inclusive");

    let mut full_scale = args();
    full_scale.frame =
        Some(FailureEvidenceFrameArgs { window_id: "17".into(), scale: Some(1.0), max_dimension: Some(u32::MAX) });
    validate_failure_evidence_args(&mut full_scale).expect("full scale and maximum dimension are valid");
}

#[tokio::test]
async fn selectors_are_sorted_deduplicated_and_forwarded_exactly() {
    let mut request = args();
    request.kinds = vec!["z.kind".into(), "a.kind".into(), "a.kind".into()];
    request.components = vec!["component/z".into(), "component/a".into(), "component/a".into()];
    request.actors = vec!["actor/z".into(), "actor/a".into(), "actor/a".into()];
    let mut source = FakeSource::with_replies((0..8).map(|index| json_reply(serde_json::json!({"call": index}))));

    let result =
        collect_failure_evidence_with_source(request, &mut source, Duration::from_millis(50), Duration::from_secs(1))
            .await
            .expect("collection succeeds");

    assert_eq!(source.calls[0], FailureEvidenceQuery::Fleet { engine_id: ENGINE_ID.into() });
    assert_eq!(
        source.calls[1],
        FailureEvidenceQuery::Kinds { engine_id: ENGINE_ID.into(), names: vec!["a.kind".into(), "z.kind".into()] }
    );
    assert_eq!(
        source.calls[2],
        FailureEvidenceQuery::Component { engine_id: ENGINE_ID.into(), component: "component/a".into() }
    );
    assert_eq!(
        source.calls[3],
        FailureEvidenceQuery::Component { engine_id: ENGINE_ID.into(), component: "component/z".into() }
    );
    assert_eq!(
        source.calls[4],
        FailureEvidenceQuery::ActorLogs {
            engine_id: ENGINE_ID.into(),
            mailbox_name: "actor/a".into(),
            max: FAILURE_EVIDENCE_LOG_ENTRIES,
        }
    );
    assert_eq!(
        source.calls[5],
        FailureEvidenceQuery::ActorCost { engine_id: ENGINE_ID.into(), mailbox_name: "actor/a".into() }
    );
    assert_eq!(
        source.calls[6],
        FailureEvidenceQuery::ActorLogs {
            engine_id: ENGINE_ID.into(),
            mailbox_name: "actor/z".into(),
            max: FAILURE_EVIDENCE_LOG_ENTRIES,
        }
    );
    assert_eq!(
        source.calls[7],
        FailureEvidenceQuery::ActorCost { engine_id: ENGINE_ID.into(), mailbox_name: "actor/z".into() }
    );

    let json = result_json(&result);
    assert_eq!(json["primary_error"], "original send failed");
    assert_eq!(json["components"][0]["selector"], "component/a");
    assert_eq!(json["actors"][0]["mailbox_name"], "actor/a");
    assert_eq!(json["limits"]["actor_log_entries"], 100);
}

#[tokio::test]
async fn engine_id_is_canonicalized_before_queries_and_output() {
    let mut request = args();
    request.engine_id = UPPERCASE_ENGINE_ID.into();
    let mut source = FakeSource::with_replies([json_reply(serde_json::json!({"alive": []}))]);

    let result =
        collect_failure_evidence_with_source(request, &mut source, Duration::from_millis(50), Duration::from_secs(1))
            .await
            .expect("uppercase UUID spelling is valid");

    assert_eq!(source.calls, vec![FailureEvidenceQuery::Fleet { engine_id: ENGINE_ID.into() }]);
    assert_eq!(result_json(&result)["engine_id"], ENGINE_ID);

    let selected = select_failure_evidence_fleet(
        ListEnginesResponse {
            engines: Some(vec![EngineInfo { engine_id: ENGINE_ID.into(), rpc_port: 1, last_heartbeat_age_millis: 0 }]),
            recently_died: None,
        },
        ENGINE_ID,
    );
    assert_eq!(selected.alive.len(), 1, "the canonical selector matches canonical fleet rows");
}

#[tokio::test]
async fn partial_errors_and_timeouts_do_not_stop_later_observations() {
    let mut request = args();
    request.components = vec!["slow".into(), "later".into()];
    let mut source = FakeSource::with_replies([
        FakeReply { delay: Duration::ZERO, result: Err("fleet unavailable".into()) },
        delayed_json_reply(Duration::from_millis(30), serde_json::json!({"too": "late"})),
        json_reply(serde_json::json!({"doc": "kept"})),
    ]);

    let result = collect_failure_evidence_with_source(
        request,
        &mut source,
        Duration::from_millis(10),
        Duration::from_millis(100),
    )
    .await
    .expect("partial failures are bundle data");
    let json = result_json(&result);

    assert_eq!(json["fleet"]["status"], "error");
    assert_eq!(json["components"][0]["observation"]["status"], "timeout");
    assert_eq!(json["components"][1]["observation"]["status"], "ok");
    assert_eq!(source.calls.len(), 3);
}

#[tokio::test]
async fn whole_budget_marks_remaining_fields_without_starting_them() {
    let mut request = args();
    request.components = vec!["never-started".into()];
    request.actors = vec!["also-never-started".into()];
    let mut source =
        FakeSource::with_replies([delayed_json_reply(Duration::from_millis(50), serde_json::json!({"too": "late"}))]);

    let result =
        collect_failure_evidence_with_source(request, &mut source, Duration::from_secs(1), Duration::from_millis(10))
            .await
            .expect("budget exhaustion is bundle data");
    let json = result_json(&result);

    assert_eq!(json["fleet"]["status"], "budget_exhausted");
    assert_eq!(json["components"][0]["observation"]["status"], "budget_exhausted");
    assert_eq!(json["actors"][0]["logs"]["status"], "budget_exhausted");
    assert_eq!(json["actors"][0]["cost"]["status"], "budget_exhausted");
    assert_eq!(source.calls, vec![FailureEvidenceQuery::Fleet { engine_id: ENGINE_ID.into() }]);
}

#[test]
fn fleet_projection_keeps_only_the_selected_live_and_dead_rows() {
    let other = "00000000-0000-0000-0000-000000000999";
    let fleet = ListEnginesResponse {
        engines: Some(vec![
            EngineInfo { engine_id: ENGINE_ID.into(), rpc_port: 1, last_heartbeat_age_millis: 2 },
            EngineInfo { engine_id: other.into(), rpc_port: 3, last_heartbeat_age_millis: 4 },
        ]),
        recently_died: Some(vec![
            DeadEngineInfo {
                engine_id: ENGINE_ID.into(),
                rpc_port: 5,
                reason: "crashed".into(),
                detail: "closed".into(),
                died_age_millis: 6,
            },
            DeadEngineInfo {
                engine_id: other.into(),
                rpc_port: 7,
                reason: "terminated".into(),
                detail: String::new(),
                died_age_millis: 8,
            },
        ]),
    };

    let selected = select_failure_evidence_fleet(fleet, ENGINE_ID);
    assert_eq!(selected.alive.len(), 1);
    assert_eq!(selected.recently_died.len(), 1);
    assert_eq!(selected.alive[0].engine_id, ENGINE_ID);
    assert_eq!(selected.recently_died[0].engine_id, ENGINE_ID);
}

#[test]
fn oversized_json_uses_the_whole_response_spill_before_images() {
    let image = Content::image("cG5n", "image/png");
    let result = failure_evidence_result_with_spill("oversized".into(), vec![image.clone()], |tool, body| {
        assert_eq!(tool, "collect_failure_evidence");
        assert_eq!(body, "oversized");
        serde_json::json!({"file": "/tmp/bundle.json", "bytes": 123}).to_string()
    });

    assert_eq!(result_json(&result), serde_json::json!({"file": "/tmp/bundle.json", "bytes": 123}));
    assert_eq!(result.content[1], image);
}

#[test]
fn frame_argument_builder_forbids_mutation_checks_and_host_writes() {
    let capture = failure_evidence_capture_args(ENGINE_ID.into(), "mbx-AAAA-AAAA-AAAA".into(), Some(0.5), Some(320));

    assert_eq!(capture.engine_id, ENGINE_ID);
    assert_eq!(capture.window_id, "mbx-AAAA-AAAA-AAAA");
    assert!(capture.mails.is_empty());
    assert!(capture.after_mails.is_empty());
    assert!(capture.checks.is_empty());
    assert!(capture.similarity.is_none());
    assert_eq!(capture.scale, Some(0.5));
    assert_eq!(capture.max_dimension, Some(320));
    assert_eq!(capture.include_image, Some(true));
    assert!(capture.save_path.is_none());
}

#[test]
fn frame_projection_requires_exactly_one_inline_image() {
    let image = Content::image("cG5n", "image/png");
    let projected = project_failure_evidence_capture(CallToolResult::success(vec![
        image.clone(),
        Content::text("{\"verdict\":null}"),
    ]))
    .expect("one image plus capture text is valid");
    let FailureEvidenceValue::Frame { summary, images } = projected else {
        panic!("capture projection must remain a frame value");
    };
    assert_eq!(images, vec![image.clone()]);
    assert_eq!(summary["image_content_blocks"], 1);

    let missing = project_failure_evidence_capture(CallToolResult::success(vec![]))
        .expect_err("a frame observation requires an inline image");
    assert!(missing.contains("0 inline images; expected exactly one"));

    let multiple = project_failure_evidence_capture(CallToolResult::success(vec![image.clone(), image]))
        .expect_err("multiple inline images exceed the bounded frame contract");
    assert!(multiple.contains("2 inline images; expected exactly one"));
}

#[tokio::test]
async fn multiple_frame_images_are_recorded_as_an_error_and_not_emitted() {
    let mut request = args();
    request.frame = Some(FailureEvidenceFrameArgs { window_id: "42".into(), scale: None, max_dimension: None });
    let mut source = FakeSource::with_replies([
        json_reply(serde_json::json!({"alive": []})),
        FakeReply {
            delay: Duration::ZERO,
            result: Ok(FailureEvidenceValue::Frame {
                summary: serde_json::json!({"image_content_blocks": 2}),
                images: vec![Content::image("a", "image/png"), Content::image("b", "image/png")],
            }),
        },
    ]);

    let result =
        collect_failure_evidence_with_source(request, &mut source, Duration::from_millis(50), Duration::from_secs(1))
            .await
            .expect("capture cardinality failures are bundle data");

    assert_eq!(result.content.len(), 1, "invalid image blocks never escape after the JSON bundle");
    assert_eq!(result_json(&result)["frame"]["observation"]["status"], "error");
    assert_eq!(
        result_json(&result)["frame"]["observation"]["error"],
        "capture_frame returned 2 inline images; expected exactly one"
    );
}

#[tokio::test]
async fn frame_is_non_mutating_and_json_precedes_the_inline_png() {
    let mut request = args();
    request.frame =
        Some(FailureEvidenceFrameArgs { window_id: "42".into(), scale: Some(0.5), max_dimension: Some(320) });
    let image = Content::image("cG5n", "image/png");
    let mut source = FakeSource::with_replies([
        json_reply(serde_json::json!({"alive": []})),
        FakeReply {
            delay: Duration::ZERO,
            result: Ok(FailureEvidenceValue::Frame {
                summary: serde_json::json!({"image_content_blocks": 1}),
                images: vec![image.clone()],
            }),
        },
    ]);

    let result =
        collect_failure_evidence_with_source(request, &mut source, Duration::from_millis(50), Duration::from_secs(1))
            .await
            .expect("frame collection succeeds");

    assert_eq!(
        source.calls[1],
        FailureEvidenceQuery::Frame {
            engine_id: ENGINE_ID.into(),
            window_id: "42".into(),
            scale: Some(0.5),
            max_dimension: Some(320),
        }
    );
    assert!(matches!(result.content[0].raw, RawContent::Text(_)));
    assert_eq!(result.content[1], image);
    assert_eq!(result_json(&result)["frame"]["observation"]["status"], "ok");
}

#[test]
fn tool_router_registers_the_bounded_failure_evidence_schema() {
    let tool = Mcp::tool_router()
        .list_all()
        .into_iter()
        .find(|tool| tool.name.as_ref() == "collect_failure_evidence")
        .expect("collect_failure_evidence is registered");
    let schema = serde_json::to_value(tool.input_schema).expect("tool schema serializes");
    assert!(schema["required"].as_array().is_some_and(|required| {
        ["engine_id", "primary_error"].iter().all(|name| required.iter().any(|value| value == name))
    }));
    assert_eq!(schema["properties"]["actors"]["type"], "array");
    assert_eq!(schema["properties"]["frame"]["anyOf"][0]["$ref"], "#/$defs/FailureEvidenceFrameArgs");
}

#[test]
fn selector_limits_are_the_documented_contract() {
    assert_eq!(MAX_FAILURE_EVIDENCE_ACTORS, 8);
    assert_eq!(MAX_FAILURE_EVIDENCE_COMPONENTS, 8);
    assert_eq!(MAX_FAILURE_EVIDENCE_KINDS, 16);

    let mut request = args();
    request.actors = (0..MAX_FAILURE_EVIDENCE_ACTORS).map(|index| format!("actor-{index}")).collect();
    request.components = (0..MAX_FAILURE_EVIDENCE_COMPONENTS).map(|index| format!("component-{index}")).collect();
    request.kinds = (0..MAX_FAILURE_EVIDENCE_KINDS).map(|index| format!("kind-{index}")).collect();
    validate_failure_evidence_args(&mut request).expect("boundary counts are accepted");
}
