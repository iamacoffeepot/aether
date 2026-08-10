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

const ENGINE_ID: &str = "00000000-0000-0000-0000-000000000475";

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

#[tokio::test]
async fn invalid_and_oversized_requests_make_no_observations() {
    let mut source = FakeSource::default();
    let mut invalid = args();
    invalid.primary_error = "   ".into();
    let error = collect_failure_evidence_with_source(
        invalid,
        &mut source,
        Duration::from_millis(10),
        Duration::from_millis(50),
    )
    .await
    .expect_err("blank primary error must fail");
    assert!(error.message.contains("primary_error"));
    assert!(source.calls.is_empty());

    let mut oversized = args();
    oversized.actors = (0..=MAX_FAILURE_EVIDENCE_ACTORS).map(|index| format!("actor-{index}")).collect();
    let error = collect_failure_evidence_with_source(
        oversized,
        &mut source,
        Duration::from_millis(10),
        Duration::from_millis(50),
    )
    .await
    .expect_err("too many actors must fail");
    assert!(error.message.contains("at most 8 actors"));
    assert!(source.calls.is_empty());

    let mut invalid_frame = args();
    invalid_frame.frame =
        Some(FailureEvidenceFrameArgs { window_id: "17".into(), scale: Some(0.0), max_dimension: None });
    collect_failure_evidence_with_source(
        invalid_frame,
        &mut source,
        Duration::from_millis(10),
        Duration::from_millis(50),
    )
    .await
    .expect_err("invalid frame controls must fail before collection");
    assert!(source.calls.is_empty());
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
