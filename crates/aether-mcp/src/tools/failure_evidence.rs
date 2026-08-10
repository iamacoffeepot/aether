use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content, RawContent};
use serde_json::Value;
use tokio::time::{self, Instant};

use crate::args::{
    ActorCostArgs, ActorFailureEvidence, ActorLogsArgs, CaptureFrameArgs, CollectFailureEvidenceArgs,
    DescribeComponentArgs, DescribeKindsArgs, FailureEvidenceBundle, FailureEvidenceFleet, FailureEvidenceLimits,
    FailureEvidenceObservation, FrameFailureEvidence, ListEnginesArgs, ListEnginesResponse, NamedFailureEvidence,
};

use super::Mcp;
use super::ids::{parse_engine_id, parse_window_id};
use super::render::internal_msg;

pub(super) const MAX_FAILURE_EVIDENCE_ACTORS: usize = 8;
pub(super) const MAX_FAILURE_EVIDENCE_COMPONENTS: usize = 8;
pub(super) const MAX_FAILURE_EVIDENCE_KINDS: usize = 16;
pub(super) const FAILURE_EVIDENCE_LOG_ENTRIES: u32 = 100;
pub(super) const FAILURE_EVIDENCE_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(3);
pub(super) const FAILURE_EVIDENCE_BUNDLE_BUDGET: Duration = Duration::from_secs(15);

const MAX_PRIMARY_ERROR_BYTES: usize = 4_096;
const MAX_OPERATION_BYTES: usize = 256;
const MAX_ADDRESS_BYTES: usize = 4_096;
const MAX_KIND_NAME_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum FailureEvidenceQuery {
    Fleet { engine_id: String },
    Kinds { engine_id: String, names: Vec<String> },
    Component { engine_id: String, component: String },
    ActorLogs { engine_id: String, mailbox_name: String, max: u32 },
    ActorCost { engine_id: String, mailbox_name: String },
    Frame { engine_id: String, window_id: String, scale: Option<f32>, max_dimension: Option<u32> },
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum FailureEvidenceValue {
    Json(Value),
    Frame { summary: Value, images: Vec<Content> },
}

pub(super) trait FailureEvidenceSource {
    fn observe(
        &mut self,
        query: FailureEvidenceQuery,
    ) -> Pin<Box<dyn Future<Output = Result<FailureEvidenceValue, String>> + Send + '_>>;
}

struct McpFailureEvidenceSource<'a> {
    mcp: &'a Mcp,
}

impl FailureEvidenceSource for McpFailureEvidenceSource<'_> {
    #[allow(
        clippy::too_many_lines,
        reason = "the exhaustive query adapter keeps every aggregate observation mapped visibly to its existing tool"
    )]
    fn observe(
        &mut self,
        query: FailureEvidenceQuery,
    ) -> Pin<Box<dyn Future<Output = Result<FailureEvidenceValue, String>> + Send + '_>> {
        Box::pin(async move {
            match query {
                FailureEvidenceQuery::Fleet { engine_id } => {
                    let body = super::engine::list_engines(self.mcp, ListEnginesArgs { show: Some("all".into()) })
                        .await
                        .map_err(mcp_error_message)?;
                    let fleet: ListEnginesResponse = serde_json::from_str(&body)
                        .map_err(|error| format!("list_engines returned invalid JSON: {error}"))?;
                    serde_json::to_value(select_failure_evidence_fleet(fleet, &engine_id))
                        .map(FailureEvidenceValue::Json)
                        .map_err(|error| format!("list_engines projection: {error}"))
                }
                FailureEvidenceQuery::Kinds { engine_id, names } => {
                    let body = super::describe::describe_kinds(
                        self.mcp,
                        DescribeKindsArgs {
                            engine_id: Some(engine_id),
                            families: false,
                            names: Some(names),
                            prefix: None,
                            full: true,
                        },
                    )
                    .await
                    .map_err(mcp_error_message)?;
                    parse_tool_json("describe_kinds", &body).map(FailureEvidenceValue::Json)
                }
                FailureEvidenceQuery::Component { engine_id, component } => {
                    let body = super::describe::describe_component(
                        self.mcp,
                        DescribeComponentArgs { engine_id, component, full: true },
                    )
                    .await
                    .map_err(mcp_error_message)?;
                    parse_tool_json("describe_component", &body).map(FailureEvidenceValue::Json)
                }
                FailureEvidenceQuery::ActorLogs { engine_id, mailbox_name, max } => {
                    let body = super::logs_cost::actor_logs(
                        self.mcp,
                        ActorLogsArgs {
                            engine_id,
                            mailbox_name,
                            max: Some(max),
                            level: None,
                            since: None,
                            contains: None,
                        },
                    )
                    .await
                    .map_err(mcp_error_message)?;
                    parse_tool_json("actor_logs", &body).map(FailureEvidenceValue::Json)
                }
                FailureEvidenceQuery::ActorCost { engine_id, mailbox_name } => {
                    let body = super::logs_cost::actor_cost(
                        self.mcp,
                        ActorCostArgs { engine_id, mailbox_name, kind_id: None },
                    )
                    .await
                    .map_err(mcp_error_message)?;
                    parse_tool_json("actor_cost", &body).map(FailureEvidenceValue::Json)
                }
                FailureEvidenceQuery::Frame { engine_id, window_id, scale, max_dimension } => {
                    let result = super::capture::capture_frame(
                        self.mcp,
                        CaptureFrameArgs {
                            engine_id,
                            window_id,
                            mails: Vec::new(),
                            after_mails: Vec::new(),
                            checks: Vec::new(),
                            similarity: None,
                            scale,
                            max_dimension,
                            include_image: Some(true),
                            save_path: None,
                        },
                    )
                    .await
                    .map_err(mcp_error_message)?;
                    let mut images = Vec::new();
                    let mut capture_text = Vec::new();
                    for content in result.content {
                        match &content.raw {
                            RawContent::Image(_) => images.push(content),
                            RawContent::Text(text) => capture_text.push(text.text.clone()),
                            _ => {
                                return Err("capture_frame returned an unexpected non-image content block".into());
                            }
                        }
                    }
                    if images.is_empty() {
                        return Err("capture_frame returned no inline PNG".into());
                    }
                    Ok(FailureEvidenceValue::Frame {
                        summary: serde_json::json!({
                            "image_content_blocks": images.len(),
                            "capture_text": capture_text,
                        }),
                        images,
                    })
                }
            }
        })
    }
}

fn mcp_error_message(error: McpError) -> String {
    error.message.into_owned()
}

fn parse_tool_json(tool: &str, body: &str) -> Result<Value, String> {
    serde_json::from_str(body).map_err(|error| format!("{tool} returned invalid JSON: {error}"))
}

pub(super) fn select_failure_evidence_fleet(fleet: ListEnginesResponse, engine_id: &str) -> FailureEvidenceFleet {
    FailureEvidenceFleet {
        alive: fleet.engines.unwrap_or_default().into_iter().filter(|engine| engine.engine_id == engine_id).collect(),
        recently_died: fleet
            .recently_died
            .unwrap_or_default()
            .into_iter()
            .filter(|engine| engine.engine_id == engine_id)
            .collect(),
    }
}

fn validate_text(value: &str, field: &str, max_bytes: usize) -> Result<(), McpError> {
    if value.trim().is_empty() {
        return Err(McpError::invalid_params(format!("{field} must not be empty"), None));
    }
    if value.len() > max_bytes {
        return Err(McpError::invalid_params(format!("{field} exceeds the {max_bytes}-byte limit"), None));
    }
    Ok(())
}

fn validate_selectors(
    values: &mut Vec<String>,
    field: &str,
    max_count: usize,
    max_bytes: usize,
) -> Result<(), McpError> {
    if values.len() > max_count {
        return Err(McpError::invalid_params(
            format!("collect_failure_evidence accepts at most {max_count} {field}"),
            None,
        ));
    }
    for (index, value) in values.iter().enumerate() {
        validate_text(value, &format!("{field}[{index}]"), max_bytes)?;
    }
    values.sort();
    values.dedup();
    Ok(())
}

pub(super) fn validate_failure_evidence_args(args: &mut CollectFailureEvidenceArgs) -> Result<(), McpError> {
    parse_engine_id(&args.engine_id)?;
    validate_text(&args.primary_error, "primary_error", MAX_PRIMARY_ERROR_BYTES)?;
    if let Some(operation) = &args.operation {
        validate_text(operation, "operation", MAX_OPERATION_BYTES)?;
    }
    validate_selectors(&mut args.actors, "actors", MAX_FAILURE_EVIDENCE_ACTORS, MAX_ADDRESS_BYTES)?;
    validate_selectors(&mut args.components, "components", MAX_FAILURE_EVIDENCE_COMPONENTS, MAX_ADDRESS_BYTES)?;
    validate_selectors(&mut args.kinds, "kinds", MAX_FAILURE_EVIDENCE_KINDS, MAX_KIND_NAME_BYTES)?;
    if let Some(frame) = &args.frame {
        parse_window_id(&frame.window_id)?;
        super::capture::resolve_capture_image_options(frame.scale, frame.max_dimension, Some(true), false)?;
    }
    Ok(())
}

async fn run_observation<S: FailureEvidenceSource>(
    source: &mut S,
    query: FailureEvidenceQuery,
    deadline: Instant,
    observation_timeout: Duration,
) -> (FailureEvidenceObservation, Vec<Content>) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return (FailureEvidenceObservation::BudgetExhausted, Vec::new());
    }
    let limited_by_budget = remaining < observation_timeout;
    match time::timeout(remaining.min(observation_timeout), source.observe(query)).await {
        Ok(Ok(FailureEvidenceValue::Json(value))) => (FailureEvidenceObservation::Ok { value }, Vec::new()),
        Ok(Ok(FailureEvidenceValue::Frame { summary, images })) => {
            (FailureEvidenceObservation::Ok { value: summary }, images)
        }
        Ok(Err(error)) => (FailureEvidenceObservation::Error { error }, Vec::new()),
        Err(_) if limited_by_budget => (FailureEvidenceObservation::BudgetExhausted, Vec::new()),
        Err(_) => (FailureEvidenceObservation::Timeout, Vec::new()),
    }
}

pub(super) fn failure_evidence_result_with_spill(
    body: String,
    images: Vec<Content>,
    spill: impl FnOnce(&str, String) -> String,
) -> CallToolResult {
    let mut content = Vec::with_capacity(1 + images.len());
    content.push(Content::text(spill("collect_failure_evidence", body)));
    content.extend(images);
    CallToolResult::success(content)
}

pub(super) async fn collect_failure_evidence_with_source<S: FailureEvidenceSource>(
    mut args: CollectFailureEvidenceArgs,
    source: &mut S,
    observation_timeout: Duration,
    bundle_budget: Duration,
) -> Result<CallToolResult, McpError> {
    validate_failure_evidence_args(&mut args)?;
    let deadline = Instant::now() + bundle_budget;
    let mut images = Vec::new();

    let (fleet, _) = run_observation(
        source,
        FailureEvidenceQuery::Fleet { engine_id: args.engine_id.clone() },
        deadline,
        observation_timeout,
    )
    .await;

    let kinds = if args.kinds.is_empty() {
        None
    } else {
        let (observation, _) = run_observation(
            source,
            FailureEvidenceQuery::Kinds { engine_id: args.engine_id.clone(), names: args.kinds },
            deadline,
            observation_timeout,
        )
        .await;
        Some(observation)
    };

    let mut components = Vec::with_capacity(args.components.len());
    for component in args.components {
        let (observation, _) = run_observation(
            source,
            FailureEvidenceQuery::Component { engine_id: args.engine_id.clone(), component: component.clone() },
            deadline,
            observation_timeout,
        )
        .await;
        components.push(NamedFailureEvidence { selector: component, observation });
    }

    let mut actors = Vec::with_capacity(args.actors.len());
    for mailbox_name in args.actors {
        let (logs, _) = run_observation(
            source,
            FailureEvidenceQuery::ActorLogs {
                engine_id: args.engine_id.clone(),
                mailbox_name: mailbox_name.clone(),
                max: FAILURE_EVIDENCE_LOG_ENTRIES,
            },
            deadline,
            observation_timeout,
        )
        .await;
        let (cost, _) = run_observation(
            source,
            FailureEvidenceQuery::ActorCost { engine_id: args.engine_id.clone(), mailbox_name: mailbox_name.clone() },
            deadline,
            observation_timeout,
        )
        .await;
        actors.push(ActorFailureEvidence { mailbox_name, logs, cost });
    }

    let frame = if let Some(frame) = args.frame {
        let window_id = frame.window_id;
        let (observation, frame_images) = run_observation(
            source,
            FailureEvidenceQuery::Frame {
                engine_id: args.engine_id.clone(),
                window_id: window_id.clone(),
                scale: frame.scale,
                max_dimension: frame.max_dimension,
            },
            deadline,
            observation_timeout,
        )
        .await;
        images.extend(frame_images);
        Some(FrameFailureEvidence { window_id, observation })
    } else {
        None
    };

    let bundle = FailureEvidenceBundle {
        engine_id: args.engine_id,
        primary_error: args.primary_error,
        operation: args.operation,
        limits: FailureEvidenceLimits {
            max_actors: MAX_FAILURE_EVIDENCE_ACTORS,
            max_components: MAX_FAILURE_EVIDENCE_COMPONENTS,
            max_kinds: MAX_FAILURE_EVIDENCE_KINDS,
            actor_log_entries: FAILURE_EVIDENCE_LOG_ENTRIES,
            observation_timeout_millis: u64::try_from(observation_timeout.as_millis()).unwrap_or(u64::MAX),
            bundle_budget_millis: u64::try_from(bundle_budget.as_millis()).unwrap_or(u64::MAX),
        },
        fleet,
        kinds,
        components,
        actors,
        frame,
    };
    let body = serde_json::to_string(&bundle)
        .map_err(|error| internal_msg(&format!("collect_failure_evidence serialize: {error}")))?;
    Ok(failure_evidence_result_with_spill(body, images, super::bytes::spill_oversized_response))
}

pub(super) async fn collect_failure_evidence(
    mcp: &Mcp,
    args: CollectFailureEvidenceArgs,
) -> Result<CallToolResult, McpError> {
    collect_failure_evidence_with_source(
        args,
        &mut McpFailureEvidenceSource { mcp },
        FAILURE_EVIDENCE_OBSERVATION_TIMEOUT,
        FAILURE_EVIDENCE_BUNDLE_BUDGET,
    )
    .await
}
